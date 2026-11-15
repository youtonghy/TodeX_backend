//! Builds the Objective-C half of Computer Use's macOS screenshots
//! (`src/computer/platform/macos_capture.m`): ScreenCaptureKit captures of
//! one window, or of one app's windows on a display, which xa11y lacks.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let source = "src/computer/platform/macos_capture.m";
    println!("cargo:rerun-if-changed={source}");
    cc::Build::new()
        .file(source)
        .flag("-fobjc-arc")
        .flag("-fobjc-exceptions")
        .flag("-fmodules")
        .compile("todex_macos_capture");
    println!("cargo:rustc-link-lib=framework=ScreenCaptureKit");
    println!("cargo:rustc-link-lib=framework=CoreGraphics");
    println!("cargo:rustc-link-lib=framework=Foundation");
}
