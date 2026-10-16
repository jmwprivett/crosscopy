//! Prints what crosscopy sees on the clipboard: `cargo run -p crosscopy-clipboard --example dump`

fn main() {
    let contents = crosscopy_clipboard::Clipboard::new().and_then(|mut c| c.read());
    let contents = match contents {
        Ok(contents) => contents,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let preview = |s: &str| s.chars().take(80).collect::<String>().replace('\n', "\\n");
    println!("excluded: {}", crosscopy_clipboard::is_excluded());
    println!("text:  {:?}", contents.text.as_deref().map(preview));
    println!("html:  {:?}", contents.html.as_deref().map(preview));
    println!("rtf:   {:?}", contents.rtf.as_ref().map(|r| preview(&String::from_utf8_lossy(r))));
    println!("png:   {:?}", contents.png.as_ref().map(|p| format!("{} bytes", p.len())));
    println!("files: {:?}", contents.files);
}
