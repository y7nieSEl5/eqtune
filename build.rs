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
}
