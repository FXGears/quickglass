//! Build script for the vendored, QuickGlass-local `freetype-sys`.
//!
//! Two modes:
//!   * Default: link the system `freetype2` through `pkg-config`, exactly as
//!     the upstream crate does. Used by the Linux build, where libfreetype is
//!     already present.
//!   * `bundled`: compile the vendored FreeType C sources into a static
//!     library with no external dependencies. Used by the Windows build.
//!     Unlike upstream, PNG support is not compiled in, so neither libpng nor
//!     zlib is needed; FreeType still loads TTF/TTC/OTF faces, which is all the
//!     renderer requests (it never asks FreeType for PNG colour bitmaps).

fn add_sources(build: &mut cc::Build, root: &str, files: &[&str]) {
    let root = std::path::Path::new(root);
    build.files(files.iter().map(|src| {
        let mut p = root.join(src);
        p.set_extension("c");
        p
    }));

    build.include(root);
}

fn main() {
    if !cfg!(feature = "bundled") {
        let target = std::env::var("TARGET").unwrap();
        if !target.contains("android") && !target.contains("ohos") {
            pkg_config::Config::new()
                .atleast_version("24.3.18")
                .probe("freetype2")
                .unwrap();
        }
        return;
    }

    let mut build = cc::Build::new();

    build
        .warnings(false)
        .include(".")
        .include("freetype2/include")
        .define("FT2_BUILD_LIBRARY", None)
        // Only the modules QuickGlass uses (TrueType faces, AA rendering);
        // see qg/qg_ftmodule.h.
        .include("qg")
        .define("FT_CONFIG_MODULES_H", "<qg_ftmodule.h>");

    add_sources(
        &mut build,
        "freetype2/src",
        &[
            "autofit/autofit",
            "base/ftbase",
            "base/ftbitmap",
            "base/ftdebug",
            "base/ftinit",
            "base/ftmm",
            "base/ftsystem",
            "gzip/ftgzip",
            "psnames/psnames",
            "sfnt/sfnt",
            "smooth/smooth",
            "truetype/truetype",
        ],
    );

    build.compile("freetype2");
}
