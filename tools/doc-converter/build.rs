//! Build script — copies the pdfium binary fetched by scripts/fetch-pdfium.sh for the target platform
//! next to the compiled executable so pdfium-render's default loader finds it.
//!
//! Vendor layout:
//!   vendor/pdfium/<platform>/<binary>
//!     win-x64/pdfium.dll
//!     linux-x64/libpdfium.so
//!     mac-arm64/libpdfium.dylib
//!
//! Target binary destination:
//!   target/<profile>/<binary>

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    // target/<profile>/ is the parent of the `build` directory above OUT_DIR. Its depth
    // depends on the cargo build-dir layout:
    //   OUT_DIR = target/<profile>/build/<crate>-<hash>/out     (older layout)
    //   OUT_DIR = target/<profile>/build/<crate>/<hash>/out     (cargo 1.9x+ layout)
    let target_profile_dir = out_dir
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "build"))
        .and_then(|b| b.parent())
        .expect("expected a `build` directory above OUT_DIR")
        .to_path_buf();

    let (subdir, binary_name) = match target_triple().as_str() {
        t if t.contains("windows")   => ("win-x64",   "pdfium.dll"),
        t if t.contains("linux") && t.contains("aarch64") => ("linux-arm64", "libpdfium.so"),
        t if t.contains("linux")     => ("linux-x64", "libpdfium.so"),
        t if t.contains("darwin") && t.contains("aarch64") => ("mac-arm64", "libpdfium.dylib"),
        t if t.contains("darwin")    => ("mac-x64",   "libpdfium.dylib"),
        other => {
            println!("cargo:warning=doc-converter: unknown target triple '{}', pdfium NOT copied", other);
            return;
        }
    };

    let vendor_binary = manifest_dir
        .join("vendor").join("pdfium").join(subdir).join(binary_name);
    let dest = target_profile_dir.join(binary_name);

    println!("cargo:rerun-if-changed={}", vendor_binary.display());
    if !vendor_binary.is_file() {
        println!("cargo:warning=doc-converter: pdfium not found at {} - run tools/doc-converter/scripts/fetch-pdfium.sh (or set DPE_PDFIUM_PATH at runtime)",
                 vendor_binary.display());
        return;
    }

    if let Err(e) = fs::copy(&vendor_binary, &dest) {
        println!("cargo:warning=doc-converter: failed to copy pdfium to {}: {}",
                 dest.display(), e);
    }
}

fn target_triple() -> String {
    env::var("TARGET").unwrap_or_default()
}
