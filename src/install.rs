//! Self-contained `install` / `uninstall` / `status` subcommands (replacing the
//! old install.sh / uninstall.sh). Everything here needs root.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::{self, CONFIG_DIR, CONFIG_PATH};
use crate::desktop;

const BIN_PATH: &str = "/usr/local/bin/wayland-mouse";
const SERVICE_NAME: &str = "wayland-mouse.service";
const SERVICE_PATH: &str = "/etc/systemd/system/wayland-mouse.service";
const MODULES_LOAD: &str = "/etc/modules-load.d/uinput.conf";
const SERVICE_UNIT: &str = include_str!("../wayland-mouse.service");

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

/// Run a command, ignoring failure and its output (best-effort cleanup steps —
/// e.g. stopping a service that may not exist yet). We narrate each step
/// ourselves, so systemctl's own chatter is just noise.
fn sh_quiet(cmd: &str, args: &[&str]) {
    let _ = Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Run a command quietly, returning whether it succeeded.
fn sh(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn install() -> i32 {
    if !is_root() {
        eprintln!("install needs root:  sudo wayland-mouse install");
        return 1;
    }

    println!("==> Stopping any running service");
    sh_quiet("systemctl", &["stop", SERVICE_NAME]);

    if let Err(e) = install_binary() {
        eprintln!("error: {e}");
        return 1;
    }

    if let Err(e) = ensure_config() {
        eprintln!("error: {e}");
        return 1;
    }

    println!("==> Ensuring uinput loads at boot");
    let _ = fs::write(MODULES_LOAD, "uinput\n");
    sh_quiet("modprobe", &["uinput"]);

    println!("==> Disabling the compositor's own pointer accel");
    let accel_auto = desktop::disable_native_accel();

    println!("==> Installing & starting the service");
    if let Err(e) = fs::write(SERVICE_PATH, SERVICE_UNIT) {
        eprintln!("error writing {SERVICE_PATH}: {e}");
        return 1;
    }
    sh_quiet("systemctl", &["daemon-reload"]);
    if !sh("systemctl", &["enable", "--now", SERVICE_NAME]) {
        eprintln!("warning: could not enable/start {SERVICE_NAME} (is this a systemd system?)");
    }

    println!();
    println!("Done — wheel + pointer acceleration are live.");
    if accel_auto {
        println!("  Your desktop's own pointer acceleration was turned off so it doesn't");
        println!("  stack on ours (restored on uninstall). Check it any time with `status`.");
    } else {
        println!("  ⚠ IMPORTANT: turn off your compositor's pointer acceleration (see the note");
        println!("    above) — otherwise its curve stacks on top of wayland-mouse's.");
    }
    println!();
    println!("  Tune:       sudo wayland-mouse tune");
    println!("  Verify:     wayland-mouse status");
    println!(
        "  Configure:  sudo $EDITOR {CONFIG_PATH}   then  sudo systemctl restart {SERVICE_NAME}"
    );
    println!("  Logs:       journalctl -u {SERVICE_NAME} -f");
    println!("  Remove:     sudo wayland-mouse uninstall");
    0
}

fn install_binary() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("finding own path: {e}"))?;
    let own_ver = env!("CARGO_PKG_VERSION");

    // Guard the classic upgrade footgun: `cargo install` drops the new binary in
    // ~/.cargo/bin, but `sudo wayland-mouse install` runs the OLD copy already on
    // root's PATH (/usr/local/bin) because sudo's secure_path omits ~/.cargo/bin —
    // silently reinstalling the old version. If the invoking user has a newer copy
    // in their cargo bin, stop and point them at it rather than no-op the upgrade.
    if let Some((path, ver)) = newer_user_binary(own_ver) {
        return Err(format!(
            "refusing to install the older v{own_ver} over the newer v{ver} you have at\n  \
             {path}\n\
             (sudo ran this copy because ~/.cargo/bin isn't on root's PATH).\n\
             Re-run:  sudo {path} install"
        ));
    }

    let already_installed = exe
        .canonicalize()
        .ok()
        .zip(Path::new(BIN_PATH).canonicalize().ok())
        .map(|(a, b)| a == b)
        .unwrap_or(false);
    if already_installed {
        println!("==> Binary already at {BIN_PATH} (v{own_ver})");
        return Ok(());
    }
    println!("==> Installing binary (v{own_ver}) -> {BIN_PATH}");
    fs::copy(&exe, BIN_PATH).map_err(|e| format!("copying binary to {BIN_PATH}: {e}"))?;
    fs::set_permissions(BIN_PATH, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {BIN_PATH}: {e}"))?;
    Ok(())
}

/// Under `sudo`, look for a `wayland-mouse` in the invoking user's cargo bin that
/// is newer than `own_ver`. Returns `(path, "x.y.z")` if one is found — the sign
/// that `sudo` grabbed a stale copy off root's PATH instead of the just-upgraded
/// one. Best-effort: any uncertainty (no `SUDO_USER`, missing file, unparseable
/// version, or it's us) yields `None` so a normal reinstall proceeds untouched.
fn newer_user_binary(own_ver: &str) -> Option<(String, String)> {
    let user = std::env::var("SUDO_USER").ok()?;
    if user.is_empty() || user == "root" {
        return None;
    }
    let path = format!("/home/{user}/.cargo/bin/wayland-mouse");
    let cand = Path::new(&path);
    if !cand.exists() {
        return None;
    }
    // If we *are* that binary, there's nothing newer to point to.
    if std::env::current_exe()
        .ok()
        .and_then(|e| e.canonicalize().ok())
        == cand.canonicalize().ok()
    {
        return None;
    }
    let out = Command::new(&path).arg("--version").output().ok()?;
    // `--version` prints e.g. "wayland-mouse 0.7.0"; take the last token.
    let other_ver = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .last()?
        .to_string();
    semver_gt(&other_ver, own_ver).then_some((path, other_ver))
}

/// `true` if dotted version `a` is strictly greater than `b`, compared
/// numerically field by field (missing fields count as 0). Non-numeric or
/// pre-release suffixes are ignored — good enough to catch a real upgrade.
fn semver_gt(a: &str, b: &str) -> bool {
    fn parse(s: &str) -> (u32, u32, u32) {
        let mut it = s
            .trim()
            .trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(|f| f.parse().unwrap_or(0));
        (
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
        )
    }
    parse(a) > parse(b)
}

/// Write the config only if absent — never clobber a user's tuning.
fn ensure_config() -> Result<(), String> {
    fs::create_dir_all(CONFIG_DIR).map_err(|e| format!("creating {CONFIG_DIR}: {e}"))?;
    if Path::new(CONFIG_PATH).exists() {
        println!("==> Keeping existing config {CONFIG_PATH}");
        return Ok(());
    }
    println!("==> Writing default config -> {CONFIG_PATH}");
    fs::write(CONFIG_PATH, config::DEFAULT_TEMPLATE)
        .map_err(|e| format!("writing {CONFIG_PATH}: {e}"))?;
    Ok(())
}

pub fn uninstall() -> i32 {
    if !is_root() {
        eprintln!("uninstall needs root:  sudo wayland-mouse uninstall");
        return 1;
    }

    println!("==> Stopping & disabling the service");
    sh_quiet("systemctl", &["disable", "--now", SERVICE_NAME]);
    let _ = fs::remove_file(SERVICE_PATH);
    sh_quiet("systemctl", &["daemon-reload"]);

    let _ = fs::remove_file(BIN_PATH);
    let _ = fs::remove_file(MODULES_LOAD);

    println!("==> Restoring the compositor's pointer accel");
    desktop::restore_native_accel();

    println!();
    println!("Removed the daemon, service, and uinput autoload.");
    println!("Left in place: {CONFIG_PATH}  (delete manually if you want it gone).");
    0
}

pub fn status() -> i32 {
    let active = Command::new("systemctl")
        .args(["is-active", SERVICE_NAME])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let enabled = Command::new("systemctl")
        .args(["is-enabled", SERVICE_NAME])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".into());

    println!("wayland-mouse {}", env!("CARGO_PKG_VERSION"));
    println!("  service:  {SERVICE_NAME}  active={active}  enabled={enabled}");
    println!(
        "  binary:   {}",
        if Path::new(BIN_PATH).exists() {
            BIN_PATH
        } else {
            "(not installed at /usr/local/bin)"
        }
    );
    println!(
        "  config:   {CONFIG_PATH}{}",
        if Path::new(CONFIG_PATH).exists() {
            ""
        } else {
            "  (missing — defaults apply)"
        }
    );
    println!("  desktop:  {:?}", desktop::detect());
    match desktop::gnome_accel_now() {
        Some((profile, speed)) if profile == "flat" => {
            println!("  GNOME accel: flat (speed {speed})  ✓  only wayland-mouse's curve applies");
        }
        Some((profile, speed)) => {
            println!("  GNOME accel: {profile} (speed {speed})  ⚠ not flat — GNOME's own accel is");
            println!("               stacking on top; re-run `sudo wayland-mouse install`, or set");
            println!("               Settings → Mouse → Acceleration Profile to Flat.");
        }
        None => {}
    }
    println!();
    config::print_effective(Path::new(CONFIG_PATH));
    0
}

#[cfg(test)]
mod tests {
    use super::semver_gt;

    #[test]
    fn semver_gt_orders_versions() {
        assert!(semver_gt("0.6.0", "0.5.2")); // the case that bit us
        assert!(semver_gt("0.7.0", "0.6.1"));
        assert!(semver_gt("1.0.0", "0.9.9"));
        assert!(semver_gt("0.6.10", "0.6.9")); // numeric, not lexical
        assert!(!semver_gt("0.6.0", "0.6.0")); // equal is not greater
        assert!(!semver_gt("0.5.2", "0.6.0"));
        assert!(semver_gt("0.6.1", "0.6")); // missing patch == 0
    }

    #[test]
    fn semver_gt_tolerates_prefix_and_suffix() {
        assert!(semver_gt("v0.6.1", "v0.6.0"));
        assert!(semver_gt("0.7.0-rc1", "0.6.9")); // suffix ignored
    }
}
