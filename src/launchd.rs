//! Build-from-source install: copy the binary to a stable location and manage a
//! launchd LaunchAgent so the daemon runs at login. No code signing required —
//! locally built code is not quarantined, so Gatekeeper never applies.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    home()
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn uid() -> u32 {
    // SAFETY: getuid() is always safe and cannot fail.
    unsafe { libc::getuid() }
}

/// Copy this executable to a stable location and load a LaunchAgent for the daemon.
pub fn install() -> anyhow::Result<()> {
    let current = std::env::current_exe()?;
    let dest = installed_bin();
    fs::create_dir_all(support_dir())?;
    copy_install_binary(&current, &dest)?;
    {
        let mut perms = fs::metadata(&dest)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dest, perms)?;
    }

    let log = support_dir().join("daemon.log");
    let plist = plist_path();
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&plist, plist_contents(&dest, &log))?;

    let domain = format!("gui/{}", uid());
    let service = format!("{domain}/{LABEL}");
    let plist_str = plist.to_string_lossy();
    // (Re)load the daemon. If it is already loaded, restart it in place with `kickstart -k`
    // so it re-execs the freshly copied binary. A bootout+bootstrap pair would race the
    // still-terminating KeepAlive job — bootout returns before the label is gone, so the
    // immediate bootstrap collides with the still-registered service and launchd rejects it
    // with "5: Input/output error". Only bootstrap when nothing is loaded yet.
    let args = reload_args(service_is_loaded(&service), &domain, &service, &plist_str);
    let status = Command::new("launchctl").args(&args).status()?;
    if !status.success() {
        anyhow::bail!("launchctl {} failed ({status})", args[0]);
    }
    Ok(())
}

/// Whether launchd already has `service` (a `gui/<uid>/<label>` target) loaded.
fn service_is_loaded(service: &str) -> bool {
    Command::new("launchctl")
        .arg("print")
        .arg(service)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// launchctl args that (re)load the daemon: restart an already-loaded job in place (which
/// avoids the bootout/bootstrap race), otherwise bootstrap a fresh one.
fn reload_args(is_loaded: bool, domain: &str, service: &str, plist: &str) -> Vec<String> {
    if is_loaded {
        vec!["kickstart".into(), "-k".into(), service.into()]
    } else {
        vec!["bootstrap".into(), domain.into(), plist.into()]
    }
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
    let _ = fs::remove_file(&plist);
    let _ = fs::remove_file(installed_bin());
    Ok(())
}

fn copy_install_binary(current: &Path, dest: &Path) -> anyhow::Result<()> {
    if same_file(current, dest)? {
        return Ok(());
    }
    fs::copy(current, dest)?;
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
    let a_meta = fs::metadata(a)?;
    let b_meta = match fs::metadata(b) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    Ok(a_meta.dev() == b_meta.dev() && a_meta.ino() == b_meta.ino())
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
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn plist_has_essentials() {
        let p = plist_contents(Path::new("/x/eqtune"), Path::new("/x/log"));
        assert!(p.contains("<string>app.eqtune.daemon</string>"));
        assert!(p.contains("<string>/x/eqtune</string>"));
        assert!(p.contains("<string>daemon</string>"));
        assert!(p.contains("RunAtLoad"));
        assert!(p.contains("KeepAlive"));
    }

    #[test]
    fn reload_args_restarts_a_loaded_service_in_place() {
        // Already loaded: restart in place, never bootout+bootstrap (which races the
        // still-terminating KeepAlive job and fails with EIO).
        let args = reload_args(true, "gui/501", "gui/501/app.eqtune.daemon", "/x.plist");
        assert_eq!(
            args,
            ["kickstart", "-k", "gui/501/app.eqtune.daemon"].map(String::from)
        );
    }

    #[test]
    fn reload_args_bootstraps_when_nothing_is_loaded() {
        let args = reload_args(false, "gui/501", "gui/501/app.eqtune.daemon", "/x.plist");
        assert_eq!(args, ["bootstrap", "gui/501", "/x.plist"].map(String::from));
    }

    #[test]
    fn copy_install_binary_skips_same_source_and_dest() -> anyhow::Result<()> {
        let dir = test_dir("self-copy");
        let bin = dir.join("eqtune");
        fs::write(&bin, b"installed binary")?;

        copy_install_binary(&bin, &bin)?;

        assert_eq!(fs::read(&bin)?, b"installed binary");
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn copy_install_binary_skips_symlink_to_dest() -> anyhow::Result<()> {
        let dir = test_dir("symlink-copy");
        let bin = dir.join("eqtune");
        let link = dir.join("eqtune-link");
        fs::write(&bin, b"installed binary")?;
        std::os::unix::fs::symlink(&bin, &link)?;

        copy_install_binary(&link, &bin)?;

        assert_eq!(fs::read(&bin)?, b"installed binary");
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn copy_install_binary_copies_distinct_source() -> anyhow::Result<()> {
        let dir = test_dir("distinct-copy");
        let source = dir.join("target-eqtune");
        let dest = dir.join("installed-eqtune");
        fs::write(&source, b"release binary")?;
        fs::write(&dest, b"old binary")?;

        copy_install_binary(&source, &dest)?;

        assert_eq!(fs::read(&dest)?, b"release binary");
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    fn test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "eqtune-launchd-{}-{name}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
