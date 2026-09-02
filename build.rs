//! Build script: on macOS, compiles the Objective-C Core Audio shim, links the audio
//! frameworks, and embeds an Info.plist. eqtune is macOS-only (Core Audio), so native
//! build steps are skipped on other targets and when docs.rs cross-documents the macOS
//! target from its Linux builder, where no Apple compiler or SDK is available.

const MIN_MACOS: &str = "14.2";

fn main() {
    println!("cargo:rerun-if-changed=shim/tap_shim.m");
    println!("cargo:rerun-if-changed=shim/tap_shim.h");
    println!("cargo:rerun-if-changed=resources/Info.plist");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");
    println!("cargo:rerun-if-env-changed=DOCS_RS");

    if std::env::var_os("DOCS_RS").is_some()
        || std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
    {
        return;
    }

    cc::Build::new()
        .file("shim/tap_shim.m")
        .include("shim")
        .flag("-fobjc-arc")
        .flag(format!("-mmacosx-version-min={MIN_MACOS}"))
        .flag("-Werror=unguarded-availability-new")
        .compile("eqtune_shim");

    // The process-tap API starts at 14.2. Pin the final Mach-O load command as well as
    // the Objective-C compilation above, so a build on a newer SDK cannot silently raise
    // the advertised deployment floor.
    println!("cargo:rustc-link-arg=-mmacosx-version-min={MIN_MACOS}");

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
