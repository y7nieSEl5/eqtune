//! Build-from-source install: copy the binary to a stable location and manage a
//! launchd LaunchAgent so the daemon runs at login. No code signing required —
//! locally built code is not quarantined, so Gatekeeper never applies.

use std::path::{Path, PathBuf};
use std::process::Command;

const LABEL: &str = "app.eqtune.daemon";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn support_dir() -> PathBuf {
    home().join("Library/Application Support/eqtune")
}

fn installed_bin() -> PathBuf {
    support_dir().join("eqtune")
}

fn plist_path() -> PathBuf {
    home().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

fn uid() -> u32 {
    // SAFETY: getuid() is always safe and cannot fail.
    unsafe { libc::getuid() }
}

/// Copy this executable to a stable location and load a LaunchAgent for the daemon.
pub fn install() -> anyhow::Result<()> {
    let current = std::env::current_exe()?;
    let dest = installed_bin();
    std::fs::create_dir_all(support_dir())?;
    std::fs::copy(&current, &dest)?;
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms)?;
    }

    let log = support_dir().join("daemon.log");
    let plist = plist_path();
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&plist, plist_contents(&dest, &log))?;

    let domain = format!("gui/{}", uid());
    // Replace any previous instance, then bootstrap the new one.
    let _ = Command::new("launchctl")
        .arg("bootout")
        .arg(format!("{domain}/{LABEL}"))
        .status();
    let status = Command::new("launchctl")
        .arg("bootstrap")
        .arg(&domain)
        .arg(&plist)
        .status()?;
    if !status.success() {
        // Fall back to the legacy verb on older systems.
        Command::new("launchctl").arg("load").arg("-w").arg(&plist).status()?;
    }
    Ok(())
}

/// Stop and remove the LaunchAgent and the installed binary (config is left in place).
pub fn uninstall() -> anyhow::Result<()> {
    let domain = format!("gui/{}", uid());
    let plist = plist_path();
    let _ = Command::new("launchctl")
        .arg("bootout")
        .arg(format!("{domain}/{LABEL}"))
        .status();
    let _ = Command::new("launchctl").arg("unload").arg(&plist).status();
    let _ = std::fs::remove_file(&plist);
    let _ = std::fs::remove_file(installed_bin());
    Ok(())
}

fn plist_contents(bin: &Path, log: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        bin = bin.display(),
        log = log.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_has_essentials() {
        let p = plist_contents(Path::new("/x/eqtune"), Path::new("/x/log"));
        assert!(p.contains("<string>app.eqtune.daemon</string>"));
        assert!(p.contains("<string>/x/eqtune</string>"));
        assert!(p.contains("<string>daemon</string>"));
        assert!(p.contains("RunAtLoad"));
        assert!(p.contains("KeepAlive"));
    }
}
