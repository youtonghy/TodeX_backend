//! Builds the Objective-C halves of the macOS host integrations:
//! - `src/computer/platform/macos_capture.m`: Computer Use's ScreenCaptureKit
//!   captures of one window, or of one app's windows on a display, which
//!   xa11y lacks;
//! - `src/camera/platform/macos_camera.m`: the agent camera (AVFoundation).
//!
//! It also embeds an Info.plist in the daemon executable: macOS only lets a
//! process that declares `NSCameraUsageDescription` ask for the camera (it
//! is killed otherwise). The signature must carry `macos/entitlements.plist`
//! for the hardened runtime to allow the camera.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let sources = [
        "src/computer/platform/macos_capture.m",
        "src/camera/platform/macos_camera.m",
    ];
    for source in sources {
        println!("cargo:rerun-if-changed={source}");
    }
    cc::Build::new()
        .files(sources)
        .flag("-fobjc-arc")
        .flag("-fobjc-exceptions")
        .flag("-fmodules")
        .compile("todex_macos_capture");
    for framework in [
        "ScreenCaptureKit",
        "CoreGraphics",
        "Foundation",
        "AVFoundation",
        "CoreMedia",
        "CoreVideo",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }

    let template = "macos/Info.plist";
    println!("cargo:rerun-if-changed={template}");
    let plist = std::fs::read_to_string(template)
        .expect("macos/Info.plist")
        .replace(
            "@VERSION@",
            &std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION"),
        );
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("Info.plist");
    std::fs::write(&out, plist).expect("write Info.plist");
    println!(
        "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        out.display()
    );
}
