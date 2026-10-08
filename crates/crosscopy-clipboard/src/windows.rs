//! Windows backend: a message-only window registered as a clipboard
//! format listener receives `WM_CLIPBOARDUPDATE` on every change.

use crate::{ClipboardChanged, Error, Result};
use std::cell::RefCell;
use std::io;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;
use windows_sys::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM,
};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, GetClipboardData, IsClipboardFormatAvailable,
    OpenClipboard, RegisterClipboardFormatW, RemoveClipboardFormatListener,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
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
    for _ in 0..10 {
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
