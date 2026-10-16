//! Windows backend: a message-only window registered as a clipboard
//! format listener receives `WM_CLIPBOARDUPDATE` on every change. Contents
//! are read and written with the raw clipboard API, all formats within one
//! open/close so other apps see a single consistent update.

use crate::{ClipboardChanged, Contents, Error, Result, normalize_newlines};
use std::cell::RefCell;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;
use windows_sys::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, GlobalFree, HWND, LPARAM, LRESULT, WPARAM,
};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE,
    MSG, PostQuitMessage, RegisterClassW, TranslateMessage, WM_CLIPBOARDUPDATE, WNDCLASSW,
};

thread_local! {
    // The window procedure runs on the watcher thread, so a thread-local
    // is enough to hand it the channel without any global state.
    static SINK: RefCell<Option<Sender<ClipboardChanged>>> = const { RefCell::new(None) };
}

pub fn watch() -> Result<Receiver<ClipboardChanged>> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<io::Result<()>>(1);

    thread::Builder::new()
        .name("clipboard-watcher".into())
        .spawn(move || {
            SINK.with(|s| *s.borrow_mut() = Some(tx));
            let hwnd = match unsafe { create_listener_window() } {
                Ok(hwnd) => hwnd,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            unsafe {
                run_message_loop();
                RemoveClipboardFormatListener(hwnd);
                DestroyWindow(hwnd);
            }
            tracing::debug!("clipboard watcher stopped");
        })?;

    ready_rx.recv().map_err(|_| Error::WatcherDied)??;
    Ok(rx)
}

unsafe fn create_listener_window() -> io::Result<HWND> {
    let class_name = wide("crosscopy-clipboard-listener");
    unsafe {
        let hinstance = GetModuleHandleW(null());
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinstance;
        wc.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err(io::Error::last_os_error());
        }

        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            hinstance,
            null(),
        );
        if hwnd.is_null() {
            return Err(io::Error::last_os_error());
        }
        if AddClipboardFormatListener(hwnd) == 0 {
            let err = io::Error::last_os_error();
            DestroyWindow(hwnd);
            return Err(err);
        }
        Ok(hwnd)
    }
}

unsafe fn run_message_loop() {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        let alive = SINK.with(|s| {
            s.borrow()
                .as_ref()
                .is_some_and(|tx| tx.send(ClipboardChanged).is_ok())
        });
        if !alive {
            // Receiver dropped: exit the message loop so the thread ends.
            unsafe { PostQuitMessage(0) };
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

struct ExclusionFormats {
    /// Presence alone means "don't monitor" (used by KeePass and others).
    presence: [u32; 2],
    /// A DWORD value of 0 means the source app opted out of history/cloud.
    zero_valued: [u32; 2],
}

fn exclusion_formats() -> &'static ExclusionFormats {
    static FORMATS: OnceLock<ExclusionFormats> = OnceLock::new();
    FORMATS.get_or_init(|| ExclusionFormats {
        presence: [
            register_format("ExcludeClipboardContentFromMonitorProcessing"),
            register_format("Clipboard Viewer Ignore"),
        ],
        zero_valued: [
            register_format("CanUploadToCloudClipboard"),
            register_format("CanIncludeInClipboardHistory"),
        ],
    })
}

pub fn is_excluded() -> bool {
    let formats = exclusion_formats();
    let present = |f: u32| f != 0 && unsafe { IsClipboardFormatAvailable(f) } != 0;

    if formats.presence.iter().copied().any(present) {
        return true;
    }
    let zero_valued: Vec<u32> = formats.zero_valued.iter().copied().filter(|&f| present(f)).collect();
    if zero_valued.is_empty() {
        return false;
    }
    match with_open_clipboard(|| zero_valued.iter().any(|&f| unsafe { dword_is_zero(f) })) {
        Some(excluded) => excluded,
        None => {
            tracing::warn!("could not open clipboard to check exclusion flags; skipping item");
            true
        }
    }
}

/// Opens the clipboard, retrying briefly since other apps hold it transiently.
fn with_open_clipboard<T>(f: impl FnOnce() -> T) -> Option<T> {
    for _ in 0..25 {
        if unsafe { OpenClipboard(null_mut()) } != 0 {
            let result = f();
            unsafe { CloseClipboard() };
            return Some(result);
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

/// Caller must hold the clipboard open.
unsafe fn dword_is_zero(format: u32) -> bool {
    unsafe {
        let handle = GetClipboardData(format);
        if handle.is_null() || GlobalSize(handle) < size_of::<u32>() {
            return false;
        }
        let ptr = GlobalLock(handle) as *const u32;
        if ptr.is_null() {
            return false;
        }
        let value = ptr.read_unaligned();
        GlobalUnlock(handle);
        value == 0
    }
}

fn register_format(name: &str) -> u32 {
    unsafe { RegisterClipboardFormatW(wide(name).as_ptr()) }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// Standard formats; windows-sys only defines these behind the OLE feature.
const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;
const CF_HDROP: u32 = 15;
const DROPEFFECT_COPY: u32 = 1;

struct DataFormats {
    html: u32,
    rtf: u32,
    /// Chrome, Office and the Snipping Tool use "PNG"; GTK apps "image/png".
    png: [u32; 2],
    /// Tells Explorer whether pasted files are a copy or a move.
    drop_effect: u32,
}

fn data_formats() -> &'static DataFormats {
    static FORMATS: OnceLock<DataFormats> = OnceLock::new();
    FORMATS.get_or_init(|| DataFormats {
        html: register_format("HTML Format"),
        rtf: register_format("Rich Text Format"),
        png: [register_format("PNG"), register_format("image/png")],
        drop_effect: register_format("Preferred DropEffect"),
    })
}

pub struct Backend;

impl Backend {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    pub fn read(&mut self) -> Result<Contents> {
        with_open_clipboard(|| unsafe { read_open() }).ok_or(Error::Busy)
    }

    pub fn write(&mut self, contents: &Contents) -> Result<Contents> {
        // Encode first so the clipboard is only held briefly.
        let blocks = encode(contents);
        with_open_clipboard(|| unsafe { write_open(&blocks) }).ok_or(Error::Busy)??;
        self.read()
    }
}

/// Caller must hold the clipboard open.
unsafe fn read_open() -> Contents {
    let formats = data_formats();
    let get = |format: u32| unsafe { get_bytes(format) };
    let png = formats
        .png
        .iter()
        .find_map(|&f| get(f))
        .filter(|data| data.starts_with(b"\x89PNG"))
        .or_else(|| get(CF_DIB).and_then(|dib| crate::dib::to_png(&dib)));
    Contents {
        text: get(CF_UNICODETEXT).map(|data| normalize_newlines(utf16_until_nul(&data))),
        html: get(formats.html).and_then(|data| parse_cf_html(&data)),
        rtf: get(formats.rtf).map(|data| trim_nuls(&data).to_vec()).filter(|rtf| !rtf.is_empty()),
        png,
        files: get(CF_HDROP).map(|data| parse_dropfiles(&data)).unwrap_or_default(),
    }
}

/// Caller must hold the clipboard open.
unsafe fn write_open(blocks: &[(u32, Vec<u8>)]) -> Result<()> {
    unsafe {
        if EmptyClipboard() == 0 {
            return Err(io::Error::last_os_error().into());
        }
        for (format, data) in blocks {
            if *format == 0 {
                continue;
            }
            let handle = GlobalAlloc(GMEM_MOVEABLE, data.len().max(1));
            if handle.is_null() {
                return Err(io::Error::last_os_error().into());
            }
            let ptr = GlobalLock(handle) as *mut u8;
            if ptr.is_null() {
                GlobalFree(handle);
                return Err(io::Error::last_os_error().into());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            GlobalUnlock(handle);
            // On success the clipboard owns the memory.
            if SetClipboardData(*format, handle).is_null() {
                let err = io::Error::last_os_error();
                GlobalFree(handle);
                return Err(err.into());
            }
        }
        Ok(())
    }
}

fn encode(contents: &Contents) -> Vec<(u32, Vec<u8>)> {
    let formats = data_formats();
    if !contents.files.is_empty() {
        return vec![
            (CF_HDROP, dropfiles(&contents.files)),
            (formats.drop_effect, DROPEFFECT_COPY.to_le_bytes().to_vec()),
        ];
    }
    let mut blocks = Vec::new();
    if let Some(text) = &contents.text {
        blocks.push((CF_UNICODETEXT, utf16_with_nul(text.replace('\n', "\r\n").encode_utf16())));
    }
    if let Some(html) = &contents.html {
        blocks.push((formats.html, cf_html(html)));
    }
    if let Some(rtf) = &contents.rtf {
        blocks.push((formats.rtf, [rtf.as_slice(), &[0]].concat()));
    }
    if let Some(png) = &contents.png {
        blocks.push((formats.png[0], png.clone()));
        // Most apps (Paint, Word, ...) only read bitmaps; Windows derives
        // CF_DIB and CF_BITMAP from this on demand.
        match crate::dib::from_png(png) {
            Some(dib) => blocks.push((CF_DIBV5, dib)),
            None => tracing::warn!("could not decode PNG; it will only paste into apps that accept PNG"),
        }
    }
    blocks
}

/// Copies out a clipboard format's data. Caller must hold the clipboard open.
unsafe fn get_bytes(format: u32) -> Option<Vec<u8>> {
    unsafe {
        if format == 0 || IsClipboardFormatAvailable(format) == 0 {
            return None;
        }
        let handle = GetClipboardData(format);
        if handle.is_null() {
            return None;
        }
        let size = GlobalSize(handle);
        let ptr = GlobalLock(handle) as *const u8;
        if ptr.is_null() {
            return None;
        }
        let data = std::slice::from_raw_parts(ptr, size).to_vec();
        GlobalUnlock(handle);
        Some(data)
    }
}

fn utf16_until_nul(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

fn utf16_with_nul(units: impl Iterator<Item = u16>) -> Vec<u8> {
    units.chain([0]).flat_map(u16::to_le_bytes).collect()
}

fn trim_nuls(data: &[u8]) -> &[u8] {
    let end = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &data[..end]
}

/// A `DROPFILES` header followed by NUL-terminated wide paths and a final NUL.
fn dropfiles(paths: &[PathBuf]) -> Vec<u8> {
    const HEADER_LEN: u32 = 20;
    let mut out = Vec::new();
    out.extend(HEADER_LEN.to_le_bytes()); // pFiles
    out.extend([0u8; 12]); // pt, fNC
    out.extend(1u32.to_le_bytes()); // fWide
    for path in paths {
        out.extend(utf16_with_nul(path.as_os_str().encode_wide()));
    }
    out.extend([0u8; 2]);
    out
}

fn parse_dropfiles(data: &[u8]) -> Vec<PathBuf> {
    let field = |at: usize| data.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let (Some(offset), Some(wide)) = (field(0), field(16)) else {
        return Vec::new();
    };
    let Some(list) = data.get(offset as usize..) else {
        return Vec::new();
    };
    let names: Vec<String> = if wide != 0 {
        let units: Vec<u16> = list.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
        units
            .split(|&u| u == 0)
            .take_while(|name| !name.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    } else {
        list.split(|&b| b == 0)
            .take_while(|name| !name.is_empty())
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .collect()
    };
    names.into_iter().map(PathBuf::from).collect()
}

/// Wraps HTML in the `CF_HTML` header. The HTML is stored verbatim between
/// `StartHTML` and `EndHTML`, so [`parse_cf_html`] returns it unchanged.
fn cf_html(html: &str) -> Vec<u8> {
    fn header(start_html: usize, end_html: usize, start_fragment: usize, end_fragment: usize) -> String {
        format!(
            "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\n\
             StartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n"
        )
    }
    let offset = header(0, 0, 0, 0).len();
    let (fragment_start, fragment_end) = fragment_bounds(html);
    let mut out = header(offset, offset + html.len(), offset + fragment_start, offset + fragment_end).into_bytes();
    out.extend(html.as_bytes());
    out.push(0);
    out
}

/// The part of `html` that was actually copied: between the fragment
/// markers if present, else the body's contents, else everything.
fn fragment_bounds(html: &str) -> (usize, usize) {
    const START: &str = "<!--StartFragment-->";
    const END: &str = "<!--EndFragment-->";
    if let (Some(start), Some(end)) = (html.find(START), html.rfind(END))
        && start + START.len() <= end
    {
        return (start + START.len(), end);
    }
    let lower = html.to_ascii_lowercase();
    let start = lower
        .find("<body")
        .and_then(|i| lower[i..].find('>').map(|j| i + j + 1))
        .unwrap_or(0);
    let end = lower.rfind("</body>").filter(|&end| end >= start).unwrap_or(html.len());
    (start, end)
}

/// Extracts the HTML document (or just the fragment, if that is all the
/// header describes) from `CF_HTML` data.
fn parse_cf_html(data: &[u8]) -> Option<String> {
    let data = trim_nuls(data);
    let header_end = data.iter().position(|&b| b == b'<').unwrap_or(data.len());
    let header = std::str::from_utf8(&data[..header_end]).ok()?;
    let field = |name: &str| {
        header
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':')?.trim().parse::<i64>().ok())
    };
    let range = |start: Option<i64>, end: Option<i64>| match (start, end) {
        (Some(start), Some(end)) if start >= 0 && end >= start => {
            let (start, end) = (start as usize, (end as usize).min(data.len()));
            (start <= end).then_some(start..end)
        }
        _ => None,
    };
    let range = range(field("StartHTML"), field("EndHTML"))
        .or_else(|| range(field("StartFragment"), field("EndFragment")))?;
    let html = String::from_utf8_lossy(&data[range]).into_owned();
    (!html.is_empty()).then_some(html)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_html_round_trips() {
        for html in [
            "<b>bold</b>",
            "<html><body>\r\n<!--StartFragment--><i>x</i><!--EndFragment-->\r\n</body></html>",
            "<meta charset='utf-8'><p>héllo</p>",
        ] {
            assert_eq!(parse_cf_html(&cf_html(html)).as_deref(), Some(html));
        }
    }

    #[test]
    fn cf_html_fragment_offsets_point_at_the_copied_part() {
        let data = cf_html("<html><body><p>hi</p></body></html>");
        let text = String::from_utf8_lossy(&data).into_owned();
        let field = |name: &str| -> usize {
            let at = text.find(name).unwrap() + name.len() + 1;
            text[at..at + 10].parse().unwrap()
        };
        assert_eq!(&data[field("StartFragment")..field("EndFragment")], b"<p>hi</p>");
    }

    #[test]
    fn cf_html_with_only_fragment_offsets_parses() {
        // Some apps set StartHTML to -1, meaning "use the fragment".
        let body = "<p>x</p>";
        let header_len = "Version:1.0\r\nStartHTML:-1\r\nEndHTML:-1\r\nStartFragment:00000000\r\nEndFragment:00000000\r\n".len();
        let data = format!(
            "Version:1.0\r\nStartHTML:-1\r\nEndHTML:-1\r\nStartFragment:{:08}\r\nEndFragment:{:08}\r\n{body}",
            header_len,
            header_len + body.len()
        );
        assert_eq!(parse_cf_html(data.as_bytes()).as_deref(), Some(body));
        assert_eq!(parse_cf_html(b"garbage"), None);
    }

    #[test]
    fn dropfiles_round_trip() {
        let paths = vec![PathBuf::from(r"C:\Users\me\a b.txt"), PathBuf::from(r"D:\ünï\folder")];
        assert_eq!(parse_dropfiles(&dropfiles(&paths)), paths);
        assert!(parse_dropfiles(&[1, 2]).is_empty());
    }
}
