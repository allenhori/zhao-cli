//! Embeds `assets/zhao.ico` (and the package's version/description metadata)
//! into the Windows `zhao.exe`, so Explorer and the taskbar show zhao's
//! icon instead of the generic executable one. Compiled in only when the
//! build host is Windows -- release builds for it run natively on a
//! Windows runner -- so macOS/Linux builds never need the `winresource`
//! build dependency at all.

fn main() {
    println!("cargo:rerun-if-changed=assets/zhao.ico");
    #[cfg(windows)]
    embed_windows_resources();
}

#[cfg(windows)]
fn embed_windows_resources() {
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/zhao.ico");
    resource
        .compile()
        .expect("failed to embed the Windows icon and version resources");
}
