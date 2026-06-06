//! Build script: compiles the Objective-C Core Audio shim and links the system
//! audio frameworks it depends on.

fn main() {
    println!("cargo:rerun-if-changed=shim/tap_shim.m");
    println!("cargo:rerun-if-changed=shim/tap_shim.h");

    cc::Build::new()
        .file("shim/tap_shim.m")
        .include("shim")
        .flag("-fobjc-arc")
        .compile("eqtune_shim");

    // Frameworks the shim links against.
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    println!("cargo:rustc-link-lib=framework=CoreAudio");
    println!("cargo:rustc-link-lib=framework=AudioToolbox");

    // Embed an Info.plist into the binary so macOS shows a proper audio-capture
    // permission prompt (no code signing needed); applies to bin targets only.
    println!("cargo:rerun-if-changed=resources/Info.plist");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!(
        "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{manifest}/resources/Info.plist"
    );
}
