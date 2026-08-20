//! Build-from-source install: copy the binary to a stable location and manage a
//! launchd LaunchAgent so the daemon runs at login. The installed daemon copy is
//! ad-hoc signed locally; no Developer ID certificate or notarization is required.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Context;

const LABEL: &str = "app.eqtune.daemon";
const STARTUP_CHECKS: usize = 20;
const STARTUP_CHECK_INTERVAL: Duration = Duration::from_millis(100);

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
    install_binary(&current, &dest)?;

    let log = support_dir().join("daemon.log");
    let plist = plist_path();
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&plist, plist_contents(&dest, &log))?;

    let domain = format!("gui/{}", uid());
    let service = format!("{domain}/{LABEL}");
    let plist_str = plist.to_string_lossy();
    load_or_restart_daemon(&domain, &service, &plist_str)
}

/// Whether launchd already has `service` (a `gui/<uid>/<label>` target) loaded.
fn service_is_loaded(service: &str) -> bool {
    query_service_loaded(service).unwrap_or(false)
}

/// Query launchd without conflating "not loaded" with an inability to run or query
/// `launchctl`. Install uses the best-effort boolean wrapper above; uninstall uses this
/// checked form because it must not delete the files after merely guessing that the
/// daemon is stopped.
fn query_service_loaded(service: &str) -> anyhow::Result<bool> {
    let output = launchctl(["print", service]).output()?;
    classify_service_query(
        output.status.success(),
        output.status.code(),
        &String::from_utf8_lossy(&output.stderr),
    )
}

fn classify_service_query(
    success: bool,
    status_code: Option<i32>,
    stderr: &str,
) -> anyhow::Result<bool> {
    if success {
        return Ok(true);
    }
    // On supported macOS releases, `launchctl print` reports an absent service with
    // exit 113 and this diagnostic. Require both: an unrelated launchctl failure must be
    // surfaced rather than treated as proof that it is safe to delete the daemon binary.
    if status_code == Some(113) && stderr.contains("Could not find service") {
        return Ok(false);
    }
    anyhow::bail!(
        "could not query launchd service (status {}): {}",
        status_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "terminated by signal".into()),
        stderr.trim()
    )
}

fn service_is_running(service: &str) -> bool {
    let output = match launchctl(["print", service]).output() {
        Ok(output) if output.status.success() => output,
        _ => return false,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    service_state_is_running(&stdout)
}

/// Whether `launchctl print` output reports the job in the running state. Parsed
/// leniently so it doesn't hinge on launchd's exact formatting: match the `state` key
/// (whatever whitespace surrounds the `=`) whose value's first token is `running`, which
/// also tolerates a trailing annotation like a pid. Multi-word states (`not running`,
/// `spawn failed`) and other keys (`last exit state`) don't match.
fn service_state_is_running(output: &str) -> bool {
    output.lines().any(|line| {
        line.split_once('=').is_some_and(|(key, value)| {
            key.trim() == "state" && value.split_whitespace().next() == Some("running")
        })
    })
}

fn wait_for_service_running(service: &str) -> anyhow::Result<()> {
    for _ in 0..STARTUP_CHECKS {
        if service_is_running(service) {
            return Ok(());
        }
        std::thread::sleep(STARTUP_CHECK_INTERVAL);
    }
    anyhow::bail!("launchd loaded {LABEL}, but the daemon did not reach the running state")
}

fn load_or_restart_daemon(domain: &str, service: &str, plist: &str) -> anyhow::Result<()> {
    // Prefer kickstart for an already-loaded healthy job: it re-execs the freshly copied
    // binary without the bootout/bootstrap race. If launchd keeps stale launch constraints
    // and the restarted job never reaches "running", fall back to a full unload/reload.
    if service_is_loaded(service) {
        run_launchctl(&reload_args(true, domain, service, plist))?;
        if wait_for_service_running(service).is_ok() {
            return Ok(());
        }
        let _ = launchctl(["bootout", service]).status();
        bootstrap_with_retry(domain, plist)?;
    } else {
        run_launchctl(&reload_args(false, domain, service, plist))?;
    }
    wait_for_service_running(service)
}

/// launchctl args that (re)load the daemon: restart an already-loaded job in place,
/// otherwise bootstrap a fresh one.
fn reload_args(is_loaded: bool, domain: &str, service: &str, plist: &str) -> Vec<String> {
    if is_loaded {
        vec!["kickstart".into(), "-k".into(), service.into()]
    } else {
        vec!["bootstrap".into(), domain.into(), plist.into()]
    }
}

fn bootstrap_with_retry(domain: &str, plist: &str) -> anyhow::Result<()> {
    let args = reload_args(false, domain, "", plist);
    let mut last_status = None;
    for _ in 0..STARTUP_CHECKS {
        let status = Command::new("launchctl").args(&args).status()?;
        if status.success() {
            return Ok(());
        }
        last_status = Some(status);
        std::thread::sleep(STARTUP_CHECK_INTERVAL);
    }
    let status = last_status
        .map(|s| s.to_string())
        .unwrap_or_else(|| "not attempted".to_string());
    anyhow::bail!("launchctl bootstrap failed after retrying ({status})")
}

fn run_launchctl(args: &[String]) -> anyhow::Result<()> {
    let status = Command::new("launchctl").args(args).status()?;
    if !status.success() {
        anyhow::bail!("launchctl {} failed ({status})", args[0]);
    }
    Ok(())
}

fn launchctl<const N: usize>(args: [&str; N]) -> Command {
    let mut command = Command::new("launchctl");
    command.args(args);
    command
}

/// Stop and remove the LaunchAgent and the installed binary (config is left in place).
pub fn uninstall() -> anyhow::Result<()> {
    let domain = format!("gui/{}", uid());
    let service = format!("{domain}/{LABEL}");
    let plist = plist_path();
    if query_service_loaded(&service)? {
        run_launchctl(&["bootout".into(), service])?;
    }
    remove_file_if_exists(&plist, "LaunchAgent plist")?;
    remove_file_if_exists(&installed_bin(), "installed binary")?;
    Ok(())
}

fn remove_file_if_exists(path: &Path, description: &str) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            Err(e).with_context(|| format!("could not remove {description} {}", path.display()))
        }
    }
}

fn install_binary(current: &Path, dest: &Path) -> anyhow::Result<bool> {
    replace_install_binary(current, dest, ad_hoc_sign)
}

fn replace_install_binary(
    current: &Path,
    dest: &Path,
    sign: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    if same_file(current, dest)? {
        return Ok(false);
    }
    let tmp = install_tmp_path(dest);
    let result = (|| {
        copy_install_binary(current, &tmp)?;
        let mut perms = fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tmp, perms)?;
        sign(&tmp)?;
        fs::rename(&tmp, dest)?;
        Ok(true)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn copy_install_binary(current: &Path, dest: &Path) -> anyhow::Result<()> {
    fs::copy(current, dest)?;
    Ok(())
}

fn install_tmp_path(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("eqtune"))
        .to_os_string();
    name.push(format!(".installing.{}", std::process::id()));
    dest.with_file_name(name)
}

fn ad_hoc_sign(path: &Path) -> anyhow::Result<()> {
    let output = Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(path)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "codesign failed for {} ({}): {}",
            path.display(),
            output.status,
            stderr.trim()
        );
    }
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
    fn service_state_parser_accepts_running_state_tolerantly() {
        // The canonical line, plus formatting variations that must still count as running:
        // arbitrary whitespace around `=`, and a trailing annotation on the value.
        assert!(service_state_is_running(
            "\tstate = running\n\tprogram = /x/eqtune\n"
        ));
        assert!(service_state_is_running("state=running\n"));
        assert!(service_state_is_running("   state   =   running   \n"));
        assert!(service_state_is_running("\tstate = running (pid 4321)\n"));
        // Non-running states must not match — including multi-word states and other keys
        // that merely contain the word "running".
        assert!(!service_state_is_running(
            "\tstate = spawn failed\n\tstate = active\n"
        ));
        assert!(!service_state_is_running("\tstate = not running\n"));
        assert!(!service_state_is_running("\tlast exit state = running\n"));
        assert!(!service_state_is_running("\tprogram = /usr/bin/running\n"));
    }

    #[test]
    fn service_query_distinguishes_absence_from_real_failure() {
        assert!(classify_service_query(true, Some(0), "").unwrap());
        assert!(
            !classify_service_query(
                false,
                Some(113),
                "Could not find service \"app.eqtune.daemon\" in domain"
            )
            .unwrap()
        );
        assert!(classify_service_query(false, Some(1), "Operation not permitted").is_err());
        assert!(classify_service_query(false, Some(113), "unexpected failure").is_err());
    }

    #[test]
    fn uninstall_file_removal_is_idempotent_and_truthful() -> anyhow::Result<()> {
        let dir = test_dir("uninstall-files");
        let installed = dir.join("eqtune");
        fs::write(&installed, b"binary")?;

        remove_file_if_exists(&installed, "installed binary")?;
        remove_file_if_exists(&installed, "installed binary")?;
        assert!(!installed.exists());

        let not_a_file = dir.join("not-a-file");
        fs::create_dir(&not_a_file)?;
        let error = remove_file_if_exists(&not_a_file, "LaunchAgent plist").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("could not remove LaunchAgent plist")
        );

        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn install_binary_skips_same_source_and_dest() -> anyhow::Result<()> {
        let dir = test_dir("self-copy");
        let bin = dir.join("eqtune");
        fs::write(&bin, b"installed binary")?;

        let copied = replace_install_binary(&bin, &bin, |_| unreachable!())?;

        assert!(!copied);
        assert_eq!(fs::read(&bin)?, b"installed binary");
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn install_binary_skips_symlink_to_dest() -> anyhow::Result<()> {
        let dir = test_dir("symlink-copy");
        let bin = dir.join("eqtune");
        let link = dir.join("eqtune-link");
        fs::write(&bin, b"installed binary")?;
        std::os::unix::fs::symlink(&bin, &link)?;

        let copied = replace_install_binary(&link, &bin, |_| unreachable!())?;

        assert!(!copied);
        assert_eq!(fs::read(&bin)?, b"installed binary");
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn install_binary_copies_distinct_source() -> anyhow::Result<()> {
        let dir = test_dir("distinct-copy");
        let source = dir.join("target-eqtune");
        let dest = dir.join("installed-eqtune");
        fs::write(&source, b"release binary")?;
        fs::write(&dest, b"old binary")?;

        let copied = replace_install_binary(&source, &dest, |_| Ok(()))?;

        assert!(copied);
        assert_eq!(fs::read(&dest)?, b"release binary");
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    #[test]
    fn install_binary_removes_staged_copy_when_signing_fails() -> anyhow::Result<()> {
        let dir = test_dir("sign-failure");
        let source = dir.join("target-eqtune");
        let dest = dir.join("installed-eqtune");
        fs::write(&source, b"release binary")?;
        fs::write(&dest, b"old binary")?;

        let err = replace_install_binary(&source, &dest, |_| anyhow::bail!("sign failed"))
            .expect_err("signing failure must abort install");

        assert!(err.to_string().contains("sign failed"));
        assert_eq!(fs::read(&dest)?, b"old binary");
        assert!(!install_tmp_path(&dest).exists());
        let _ = fs::remove_dir_all(dir);
        Ok(())
    }

    fn test_dir(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        // A process-wide counter guarantees uniqueness across parallel test threads even
        // when the clock is too coarse to distinguish two calls; the timestamp is kept only
        // to keep any leftover directory greppable/ordered if a test fails to clean up.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "eqtune-launchd-{}-{name}-{nanos}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
