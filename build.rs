fn main() {
    // A build script's `#[cfg]` describes the host, not the target, so a
    // Linux -> Windows cross build must branch on CARGO_CFG_TARGET_OS instead.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    if target_os == "windows" {
        println!("cargo:rustc-link-lib=advapi32");

        let mut res = winresource::WindowsResource::new();
        res.set_icon("resources/icon.ico");
        res.compile().expect("Failed to compile Windows resources");
    } else {
        // MIT-SHM (XShmPutImage/XShmAttach) lives in libXext, bound by direct FFI
        // in src/x11.rs rather than through a crate. The `x11` crate links libX11;
        // libXext must be requested explicitly. Both are stock X11 client libraries
        // (XLibre-compatible), present on any desktop X server install.
        println!("cargo:rustc-link-lib=Xext");
    }
}
