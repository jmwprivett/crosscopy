//! Embeds the app icon into the Windows executable. The icon is
//! `assets/app-icon.png`, rendered by `cargo xtask app-icon`.

use image::imageops;
use std::env;
use std::fs::File;
use std::path::PathBuf;

fn main() {
    let icon = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../assets/app-icon.png");
    println!("cargo:rerun-if-changed={}", icon.display());
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let source = image::open(&icon).expect("reading assets/app-icon.png").to_rgba8();
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16, 20, 24, 32, 40, 48, 64, 128, 256] {
        let scaled = imageops::resize(&source, size, size, imageops::FilterType::Lanczos3);
        let image = ico::IconImage::from_rgba_data(size, size, scaled.into_raw());
        dir.add_entry(ico::IconDirEntry::encode(&image).expect("encoding icon"));
    }
    let ico_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("crosscopy.ico");
    dir.write(File::create(&ico_path).expect("creating icon file"))
        .expect("writing icon file");

    let mut resources = winresource::WindowsResource::new();
    resources
        .set_icon(ico_path.to_str().unwrap())
        .set("ProductName", "CrossCopy")
        .set("FileDescription", "CrossCopy");
    resources.compile().expect("embedding Windows resources");
}
