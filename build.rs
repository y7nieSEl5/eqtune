//! Build script: on macOS, compiles the Objective-C Core Audio shim, links the audio
//! frameworks, and embeds an Info.plist. eqtune is macOS-only (Core Audio), so on other
//! targets this is a no-op (lets the crate at least configure, e.g. for a docs.rs build).

fn main() {
    println!("cargo:rerun-if-changed=shim/tap_shim.m");
    println!("cargo:rerun-if-changed=shim/tap_shim.h");
    println!("cargo:rerun-if-changed=resources/Info.plist");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    cc::Build::new()
        .file("shim/tap_shim.m")
        .include("shim")
        .flag("-fobjc-arc")
        .compile("eqtune_shim");

    for framework in ["Foundation", "CoreFoundation", "CoreAudio", "AudioToolbox"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }

    // Embed an Info.plist into the binary so macOS shows a proper audio-capture
    // permission prompt (no code signing needed); applies to bin targets only.
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let template_path = manifest.join("resources/Info.plist");
    let template = std::fs::read_to_string(&template_path).unwrap();
    let marker = "@EQTUNE_VERSION@";
    assert!(
        template.contains(marker),
        "{} must contain {marker}",
        template_path.display()
    );
    let rendered = template.replace(marker, env!("CARGO_PKG_VERSION"));
    let generated =
        std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("eqtune-Info.plist");
    std::fs::write(&generated, rendered).unwrap();
    println!(
        "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        generated.display()
    );
}
