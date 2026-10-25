//! Configuration: layered TOML (`preset` → global overrides → per-device
//! overrides → DPI rescale) deserialized with serde, resolved into a flat
//! [`Settings`] the hot path uses.
//!
//! The file format is human-friendly (`[wheel] max_multiplier = 6.0`); the
//! runtime struct keeps the original mathematical field names. One serde type
//! tree is the single source of truth a future GUI can round-trip.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Where the system config lives (a directory, so we can grow into it).
pub const CONFIG_DIR: &str = "/etc/wayland-mouse";
pub const CONFIG_PATH: &str = "/etc/wayland-mouse/config.toml";

/// The `ptr_*` curve in every preset is expressed at this DPI; [`rescale`] maps
/// it to the device's actual DPI so the feel is DPI-independent (like macOS).
pub const REFERENCE_DPI: f64 = 1400.0;

/// Commented starter config written by `install` when none exists.
pub const DEFAULT_TEMPLATE: &str = include_str!("../wayland-mouse.toml.example");

// ---------------------------------------------------------------------------
// Runtime settings (resolved, per device — what the hot path reads)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Settings {
    // wheel acceleration
    pub wheel_enabled: bool,
    pub threshold_dps: f64,
    pub accel: f64,
    pub exponent: f64,
    pub max_mult: f64,
    /// Multiplier floor at slow speed. < 1.0 de-amplifies slow scrolling for
    /// finer control in magnitude apps (Chrome/Firefox); 1.0 = no floor.
    pub min_mult: f64,
    pub attack: f64,
    pub release: f64,
    pub reset_gap: Duration,
    /// Emit acceleration as extra whole-detent notches (each its own SYN frame)
    /// instead of one inflated hi-res delta, so apps that scroll a fixed amount
    /// per wheel event (Flutter/Electron, e.g. the Ubuntu App Center) honor it.
    pub wheel_discrete_steps: bool,
    /// Only emit those notches once the multiplier reaches this; below it a
    /// single fine hi-res event keeps smooth apps gliding. 1.0 = always notch.
    pub step_start: f64,

    // pointer acceleration (macOS-like logistic S-curve)
    pub pointer_accel: bool,
    pub ptr_base: f64,
    pub ptr_max: f64,
    pub ptr_mid: f64,
    pub ptr_width: f64,
    pub ptr_tau: f64,
    pub dpi: f64,

    // temporary click lock (tap-to-drag), per device
    pub click_lock: bool,
    pub click_lock_hold: Duration,
    pub click_lock_release: Duration,

    pub debug: bool,
}

/// Defaults for the per-device click lock. Disabled unless a device rule turns
/// it on; the timings match the original `tapdrag` defaults.
pub const CLICK_LOCK_HOLD: Duration = Duration::from_millis(180);
pub const CLICK_LOCK_RELEASE: Duration = Duration::from_millis(300);

// ---------------------------------------------------------------------------
// Presets — concrete curve sets, expressed at REFERENCE_DPI
// ---------------------------------------------------------------------------

/// Names accepted for the built-in presets (for validation / docs).
pub const PRESET_NAMES: &[&str] = &["mac-like", "subtle", "off"];

fn mac_like() -> Settings {
    Settings {
        wheel_enabled: true,
        threshold_dps: 5.0,
        accel: 0.06,
        exponent: 1.0,
        max_mult: 6.0,
        // Slow scroll runs a touch below 1:1 for finer control in magnitude apps
        // (Chrome/Firefox); notches only kick in past 1.5× so those apps glide
        // until a fast flick. Tuned by feel; see the wheel module for the math.
        min_mult: 0.9,
        attack: 0.6,
        release: 0.15,
        reset_gap: Duration::from_millis(180),
        wheel_discrete_steps: true,
        step_start: 1.5,

        pointer_accel: true,
        ptr_base: 0.5,
        ptr_max: 2.5,
        ptr_mid: 4000.0,
        ptr_width: 2000.0,
        ptr_tau: 0.006,
        dpi: REFERENCE_DPI,

        click_lock: false,
        click_lock_hold: CLICK_LOCK_HOLD,
        click_lock_release: CLICK_LOCK_RELEASE,

        debug: false,
    }
}

/// A gentle nudge rather than a transformation: kicks in later, ramps softer,
/// lower caps.
fn subtle() -> Settings {
    Settings {
        wheel_enabled: true,
        threshold_dps: 10.0,
        accel: 0.05,
        exponent: 1.0,
        max_mult: 4.0,
        min_mult: 1.0,
        attack: 0.5,
        release: 0.15,
        reset_gap: Duration::from_millis(180),
        wheel_discrete_steps: true,
        step_start: 1.0,

        pointer_accel: true,
        ptr_base: 0.8,
        ptr_max: 1.6,
        ptr_mid: 5000.0,
        ptr_width: 2500.0,
        ptr_tau: 0.012,
        dpi: REFERENCE_DPI,

        click_lock: false,
        click_lock_hold: CLICK_LOCK_HOLD,
        click_lock_release: CLICK_LOCK_RELEASE,

        debug: false,
    }
}

/// Pure passthrough: both subsystems off, identity values if ever consulted.
fn off() -> Settings {
    Settings {
        wheel_enabled: false,
        threshold_dps: 0.0,
        accel: 0.0,
        exponent: 1.0,
        max_mult: 1.0,
        min_mult: 1.0,
        attack: 1.0,
        release: 1.0,
        reset_gap: Duration::from_millis(180),
        wheel_discrete_steps: false,
        step_start: 1.0,

        pointer_accel: false,
        ptr_base: 1.0,
        ptr_max: 1.0,
        ptr_mid: 4000.0,
        ptr_width: 2000.0,
        ptr_tau: 0.012,
        dpi: REFERENCE_DPI,

        click_lock: false,
        click_lock_hold: CLICK_LOCK_HOLD,
        click_lock_release: CLICK_LOCK_RELEASE,

        debug: false,
    }
}

/// Resolve a built-in preset by name; `None` if unknown (caller decides the fallback).
fn builtin_preset(name: &str) -> Option<Settings> {
    match name.trim().to_lowercase().as_str() {
        "mac-like" | "mac" | "macos" | "mac_like" => Some(mac_like()),
        "subtle" | "gentle" => Some(subtle()),
        "off" | "flat" | "none" | "passthrough" => Some(off()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// On-disk format (serde)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ConfigFile {
    pub preset: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dpi: Option<f64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name_filter: String,
    #[serde(skip_serializing_if = "is_false")]
    pub debug: bool,
    #[serde(skip_serializing_if = "WheelCfg::is_empty")]
    pub wheel: WheelCfg,
    #[serde(skip_serializing_if = "PointerCfg::is_empty")]
    pub pointer: PointerCfg,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub device: Vec<DeviceRule>,
    /// User-created presets. Built-in names (`mac-like`, `subtle`, `off`) stay
    /// in code and are not stored here.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub presets: BTreeMap<String, PresetDef>,
}

impl Default for ConfigFile {
    fn default() -> Self {
        ConfigFile {
            preset: "mac-like".to_string(),
            dpi: None,
            name_filter: String::new(),
            debug: false,
            wheel: WheelCfg::default(),
            pointer: PointerCfg::default(),
            device: Vec::new(),
            presets: BTreeMap::new(),
        }
    }
}

/// A user-created preset. Values are config-space (un-rescaled); missing fields
/// inherit from the `mac-like` baseline. The tuner writes full snapshots.
#[derive(Deserialize, Serialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct PresetDef {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dpi: Option<f64>,
    #[serde(skip_serializing_if = "WheelCfg::is_empty")]
    pub wheel: WheelCfg,
    #[serde(skip_serializing_if = "PointerCfg::is_empty")]
    pub pointer: PointerCfg,
}

impl PresetDef {
    /// Resolve this preset into config-space settings (mac-like baseline).
    pub fn to_settings(&self) -> Settings {
        let mut s = mac_like();
        apply_wheel(&mut s, &self.wheel);
        apply_pointer(&mut s, &self.pointer);
        s.dpi = self.dpi.unwrap_or(REFERENCE_DPI);
        s
    }

    /// Capture a full snapshot of resolved settings.
    #[cfg(feature = "tune")]
    pub fn from_settings(s: &Settings) -> Self {
        PresetDef {
            dpi: Some(s.dpi),
            wheel: WheelCfg {
                enabled: Some(s.wheel_enabled),
                start_speed: Some(s.threshold_dps),
                strength: Some(s.accel),
                curve: Some(s.exponent),
                max_multiplier: Some(s.max_mult),
                min_multiplier: Some(s.min_mult),
                smoothing_up: Some(s.attack),
                smoothing_down: Some(s.release),
                reset_after_ms: Some(s.reset_gap.as_secs_f64() * 1000.0),
                discrete_steps: Some(s.wheel_discrete_steps),
                steps_above: Some(s.step_start),
            },
            pointer: PointerCfg {
                enabled: Some(s.pointer_accel),
                precision_gain: Some(s.ptr_base),
                max_gain: Some(s.ptr_max),
                midpoint_speed: Some(s.ptr_mid),
                transition_width: Some(s.ptr_width),
                smoothing_ms: Some(s.ptr_tau * 1000.0),
            },
        }
    }
}

/// Per-device temporary click lock (tap-to-drag). Omitted everywhere by default.
#[derive(Deserialize, Serialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ClickLockCfg {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_ms: Option<f64>,
}

impl ClickLockCfg {
    fn is_empty(&self) -> bool {
        self.enabled.is_none() && self.hold_ms.is_none() && self.release_ms.is_none()
    }
}

#[derive(Deserialize, Serialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct WheelCfg {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_speed: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strength: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curve: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_multiplier: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoothing_up: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoothing_down: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_multiplier: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_after_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discrete_steps: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps_above: Option<f64>,
}

impl WheelCfg {
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.start_speed.is_none()
            && self.strength.is_none()
            && self.curve.is_none()
            && self.max_multiplier.is_none()
            && self.min_multiplier.is_none()
            && self.smoothing_up.is_none()
            && self.smoothing_down.is_none()
            && self.reset_after_ms.is_none()
            && self.discrete_steps.is_none()
            && self.steps_above.is_none()
    }
}

#[derive(Deserialize, Serialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct PointerCfg {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precision_gain: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_gain: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub midpoint_speed: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transition_width: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoothing_ms: Option<f64>,
}

impl PointerCfg {
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.precision_gain.is_none()
            && self.max_gain.is_none()
            && self.midpoint_speed.is_none()
            && self.transition_width.is_none()
            && self.smoothing_ms.is_none()
    }
}

#[derive(Deserialize, Serialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct DeviceRule {
    /// Case-insensitive substring matched against the device name.
    #[serde(rename = "match")]
    pub match_: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dpi: Option<f64>,
    #[serde(skip_serializing_if = "WheelCfg::is_empty")]
    pub wheel: WheelCfg,
    #[serde(skip_serializing_if = "PointerCfg::is_empty")]
    pub pointer: PointerCfg,
    #[serde(skip_serializing_if = "ClickLockCfg::is_empty")]
    pub click_lock: ClickLockCfg,
}

fn is_false(b: &bool) -> bool {
    !*b
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

impl ConfigFile {
    /// Index of the first device rule whose `match` substring is in the name.
    pub fn rule_index(&self, device_name: &str) -> Option<usize> {
        let lname = device_name.to_lowercase();
        self.device
            .iter()
            .position(|d| !d.match_.is_empty() && lname.contains(&d.match_.to_lowercase()))
    }

    /// First device rule whose `match` substring is in `device_name`.
    pub fn rule_for(&self, device_name: &str) -> Option<&DeviceRule> {
        self.rule_index(device_name).map(|i| &self.device[i])
    }

    /// Names shown by the tuner: built-ins first, then custom profiles.
    pub fn preset_names(&self) -> Vec<String> {
        let mut names: Vec<String> = PRESET_NAMES.iter().map(|s| (*s).to_string()).collect();
        names.extend(self.presets.keys().cloned());
        names
    }

    /// Whether `name` resolves to a built-in or a custom preset.
    pub fn has_preset(&self, name: &str) -> bool {
        self.presets.contains_key(name) || builtin_preset(name).is_some()
    }

    /// True when `name` is one of the hard-coded presets.
    pub fn is_builtin_preset(name: &str) -> bool {
        builtin_preset(name).is_some()
    }

    /// Resolve a preset by name (custom first, then built-ins).
    pub fn preset_settings(&self, name: &str) -> Option<Settings> {
        if let Some(p) = self.presets.get(name) {
            return Some(p.to_settings());
        }
        builtin_preset(name)
    }

    /// Whether any device rule explicitly enables click lock for this name.
    pub fn click_lock_for(&self, device_name: &str) -> bool {
        self.rule_for(device_name)
            .and_then(|r| r.click_lock.enabled)
            .unwrap_or(false)
    }

    /// Resolve config-space settings (no DPI rescale) for a device.
    pub fn resolve_config_space(&self, device_name: &str) -> Settings {
        let rule = self.rule_for(device_name);
        let preset_name = rule
            .and_then(|r| r.preset.as_deref())
            .unwrap_or(&self.preset);
        let mut s = self.preset_settings(preset_name).unwrap_or_else(|| {
            eprintln!("wayland-mouse: unknown preset {preset_name:?}, using 'mac-like'");
            mac_like()
        });

        apply_wheel(&mut s, &self.wheel);
        apply_pointer(&mut s, &self.pointer);
        if let Some(r) = rule {
            apply_wheel(&mut s, &r.wheel);
            apply_pointer(&mut s, &r.pointer);
            apply_click_lock(&mut s, &r.click_lock);
        }

        s.dpi = rule.and_then(|r| r.dpi).or(self.dpi).unwrap_or(s.dpi);
        s.debug = self.debug;
        s
    }

    /// Resolve the effective [`Settings`] for a device by name.
    ///
    /// Layering: base preset (device rule's preset if set, else the global
    /// preset) → global overrides → device overrides → DPI rescale.
    pub fn resolve(&self, device_name: &str) -> Settings {
        let mut s = self.resolve_config_space(device_name);
        rescale(&mut s);
        s
    }

    /// Effective settings with no device rule applied (the global baseline).
    pub fn resolve_global(&self) -> Settings {
        self.resolve("")
    }
}

fn apply_wheel(s: &mut Settings, w: &WheelCfg) {
    if let Some(v) = w.enabled {
        s.wheel_enabled = v;
    }
    if let Some(v) = w.start_speed {
        s.threshold_dps = v;
    }
    if let Some(v) = w.strength {
        s.accel = v;
    }
    if let Some(v) = w.curve {
        s.exponent = v;
    }
    if let Some(v) = w.max_multiplier {
        s.max_mult = v;
    }
    if let Some(v) = w.min_multiplier {
        s.min_mult = v;
    }
    if let Some(v) = w.smoothing_up {
        s.attack = v;
    }
    if let Some(v) = w.smoothing_down {
        s.release = v;
    }
    if let Some(v) = w.reset_after_ms {
        s.reset_gap = Duration::from_secs_f64((v / 1000.0).max(0.0));
    }
    if let Some(v) = w.discrete_steps {
        s.wheel_discrete_steps = v;
    }
    if let Some(v) = w.steps_above {
        s.step_start = v;
    }
}

fn apply_click_lock(s: &mut Settings, c: &ClickLockCfg) {
    if let Some(v) = c.enabled {
        s.click_lock = v;
    }
    if let Some(v) = c.hold_ms {
        s.click_lock_hold = Duration::from_secs_f64((v / 1000.0).max(0.0));
    }
    if let Some(v) = c.release_ms {
        s.click_lock_release = Duration::from_secs_f64((v / 1000.0).max(0.0));
    }
}

fn apply_pointer(s: &mut Settings, p: &PointerCfg) {
    if let Some(v) = p.enabled {
        s.pointer_accel = v;
    }
    if let Some(v) = p.precision_gain {
        s.ptr_base = v;
    }
    if let Some(v) = p.max_gain {
        s.ptr_max = v;
    }
    if let Some(v) = p.midpoint_speed {
        s.ptr_mid = v;
    }
    if let Some(v) = p.transition_width {
        s.ptr_width = v;
    }
    if let Some(v) = p.smoothing_ms {
        s.ptr_tau = (v / 1000.0).max(0.0);
    }
}

/// Map the REFERENCE_DPI curve onto the device's real DPI: speed breakpoints
/// scale up, gains scale down, so the on-screen feel stays identical.
fn rescale(s: &mut Settings) {
    if s.dpi > 0.0 {
        let k = s.dpi / REFERENCE_DPI;
        s.ptr_mid *= k;
        s.ptr_width *= k;
        s.ptr_base /= k;
        s.ptr_max /= k;
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load and parse the config. A missing file yields defaults (mac-like); only a
/// read or parse error is `Err`.
pub fn load(path: &Path) -> Result<ConfigFile, String> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ConfigFile::default()),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    toml::from_str(&text).map_err(|e| format!("parsing {}:\n{e}", path.display()))
}

// ---------------------------------------------------------------------------
// `config --print` and `config --check`
// ---------------------------------------------------------------------------

/// Print the effective (post-rescale) settings and any device rules.
pub fn print_effective(path: &Path) -> i32 {
    let cf = match load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    if path.exists() {
        println!("# effective config from {}", path.display());
    } else {
        println!(
            "# no config at {} — built-in defaults (preset = mac-like)",
            path.display()
        );
    }
    print_settings(
        &format!("global preset={:?}", cf.preset),
        &cf.resolve_global(),
    );
    for r in &cf.device {
        if r.match_.is_empty() {
            continue;
        }
        println!();
        let preset = r.preset.as_deref().unwrap_or(&cf.preset);
        print_settings(
            &format!("device matching {:?} preset={preset:?}", r.match_),
            &cf.resolve(&r.match_),
        );
    }
    if !cf.presets.is_empty() {
        println!("\n[presets]");
        for name in cf.presets.keys() {
            if let Some(s) = cf.preset_settings(name) {
                print_settings(name, &s);
            }
        }
    }
    0
}

fn print_settings(label: &str, s: &Settings) {
    println!("[{label}]  dpi = {}", s.dpi);
    println!(
        "  wheel:   enabled={} start_speed={} strength={} curve={} max_multiplier={} \
         min_multiplier={} smoothing_up={} smoothing_down={} reset_after_ms={:.0} \
         discrete_steps={} steps_above={}",
        s.wheel_enabled,
        s.threshold_dps,
        s.accel,
        s.exponent,
        s.max_mult,
        s.min_mult,
        s.attack,
        s.release,
        s.reset_gap.as_secs_f64() * 1000.0,
        s.wheel_discrete_steps,
        s.step_start,
    );
    println!(
        "  pointer: enabled={} precision_gain={:.3} max_gain={:.3} midpoint_speed={:.0} \
         transition_width={:.0} smoothing_ms={:.1}  (global/device values include DPI rescale)",
        s.pointer_accel,
        s.ptr_base,
        s.ptr_max,
        s.ptr_mid,
        s.ptr_width,
        s.ptr_tau * 1000.0,
    );
    println!(
        "  click_lock: enabled={} hold_ms={:.0} release_ms={:.0}",
        s.click_lock,
        s.click_lock_hold.as_secs_f64() * 1000.0,
        s.click_lock_release.as_secs_f64() * 1000.0,
    );
}

/// Validate the config: syntax, unknown keys (warn), value ranges (warn),
/// unknown presets (warn). Returns a process exit code (non-zero only on a
/// hard parse error).
pub fn check(path: &Path) -> i32 {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "no config at {} — built-in defaults apply (preset = mac-like)",
                path.display()
            );
            return 0;
        }
        Err(e) => {
            eprintln!("error reading {}: {e}", path.display());
            return 1;
        }
    };

    // 1. Syntax + typed parse.
    let value: toml::Value = match toml::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("✗ parse error in {}:\n{e}", path.display());
            return 1;
        }
    };
    let cf: ConfigFile = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("✗ {}:\n{e}", path.display());
            return 1;
        }
    };

    let mut warnings = 0u32;
    let mut warn = |msg: String| {
        eprintln!("⚠ {msg}");
        warnings += 1;
    };

    // 2. Unknown keys.
    const TOP: &[&str] = &[
        "preset",
        "dpi",
        "name_filter",
        "debug",
        "wheel",
        "pointer",
        "device",
        "presets",
    ];
    const WHEEL: &[&str] = &[
        "enabled",
        "start_speed",
        "strength",
        "curve",
        "max_multiplier",
        "min_multiplier",
        "smoothing_up",
        "smoothing_down",
        "reset_after_ms",
        "discrete_steps",
        "steps_above",
    ];
    const POINTER: &[&str] = &[
        "enabled",
        "precision_gain",
        "max_gain",
        "midpoint_speed",
        "transition_width",
        "smoothing_ms",
    ];
    const DEVICE: &[&str] = &["match", "preset", "dpi", "wheel", "pointer", "click_lock"];
    const PRESET: &[&str] = &["dpi", "wheel", "pointer"];
    const CLICK_LOCK: &[&str] = &["enabled", "hold_ms", "release_ms"];

    if let Some(t) = value.as_table() {
        unknowns(t, TOP, "", &mut warn);
        if let Some(w) = t.get("wheel").and_then(|v| v.as_table()) {
            unknowns(w, WHEEL, "wheel.", &mut warn);
        }
        if let Some(p) = t.get("pointer").and_then(|v| v.as_table()) {
            unknowns(p, POINTER, "pointer.", &mut warn);
        }
        if let Some(arr) = t.get("device").and_then(|v| v.as_array()) {
            for (i, d) in arr.iter().enumerate() {
                if let Some(dt) = d.as_table() {
                    unknowns(dt, DEVICE, &format!("device[{i}]."), &mut warn);
                    if let Some(w) = dt.get("wheel").and_then(|v| v.as_table()) {
                        unknowns(w, WHEEL, &format!("device[{i}].wheel."), &mut warn);
                    }
                    if let Some(p) = dt.get("pointer").and_then(|v| v.as_table()) {
                        unknowns(p, POINTER, &format!("device[{i}].pointer."), &mut warn);
                    }
                    if let Some(c) = dt.get("click_lock").and_then(|v| v.as_table()) {
                        unknowns(
                            c,
                            CLICK_LOCK,
                            &format!("device[{i}].click_lock."),
                            &mut warn,
                        );
                    }
                }
            }
        }
        if let Some(pt) = t.get("presets").and_then(|v| v.as_table()) {
            for (name, pv) in pt {
                if let Some(p) = pv.as_table() {
                    unknowns(p, PRESET, &format!("presets.{name}."), &mut warn);
                    if let Some(w) = p.get("wheel").and_then(|v| v.as_table()) {
                        unknowns(w, WHEEL, &format!("presets.{name}.wheel."), &mut warn);
                    }
                    if let Some(ptr) = p.get("pointer").and_then(|v| v.as_table()) {
                        unknowns(ptr, POINTER, &format!("presets.{name}.pointer."), &mut warn);
                    }
                }
            }
        }
        if t.get("button").is_some() {
            warn("[[button]] remapping was removed and is ignored".into());
        }
    }

    // 3. Preset names.
    if !cf.has_preset(&cf.preset) {
        warn(format!(
            "preset {:?} is unknown (valid: {})",
            cf.preset,
            cf.preset_names().join(", ")
        ));
    }
    for r in &cf.device {
        if let Some(p) = &r.preset {
            if !cf.has_preset(p) {
                warn(format!("device {:?}: preset {:?} is unknown", r.match_, p));
            }
        }
    }
    for name in cf.presets.keys() {
        if ConfigFile::is_builtin_preset(name) {
            warn(format!(
                "presets.{name} shadows a built-in preset; rename it"
            ));
        }
    }

    // 4. Value ranges (on the resolved global settings).
    validate_ranges(&cf.resolve_global(), &mut warn);
    for name in cf.presets.keys() {
        if let Some(p) = cf.preset_settings(name) {
            let mut w = |msg: String| warn(format!("presets.{name}: {msg}"));
            validate_ranges(&p, &mut w);
        }
    }
    for (i, r) in cf.device.iter().enumerate() {
        if let Some(v) = r.click_lock.hold_ms {
            if v <= 0.0 {
                warn(format!("device[{i}].click_lock.hold_ms should be > 0"));
            }
        }
        if let Some(v) = r.click_lock.release_ms {
            if v <= 0.0 {
                warn(format!("device[{i}].click_lock.release_ms should be > 0"));
            }
        }
        if r.click_lock.enabled != Some(true)
            && (r.click_lock.hold_ms.is_some() || r.click_lock.release_ms.is_some())
        {
            warn(format!(
                "device[{i}].click_lock timings are ignored until enabled = true"
            ));
        }
    }

    if warnings == 0 {
        println!("✓ {} is valid", path.display());
    } else {
        println!(
            "{} warning(s); config still loads (unknown keys/values are ignored or clamped)",
            warnings
        );
    }
    0
}

fn unknowns(table: &toml::Table, allowed: &[&str], prefix: &str, warn: &mut impl FnMut(String)) {
    for k in table.keys() {
        if !allowed.contains(&k.as_str()) {
            warn(format!("unknown key '{prefix}{k}'"));
        }
    }
}

fn validate_ranges(s: &Settings, warn: &mut impl FnMut(String)) {
    if s.dpi <= 0.0 {
        warn(format!("dpi must be > 0 (got {})", s.dpi));
    }
    if s.threshold_dps < 0.0 {
        warn("wheel.start_speed should be >= 0".into());
    }
    if s.accel < 0.0 {
        warn("wheel.strength should be >= 0".into());
    }
    if s.exponent <= 0.0 {
        warn("wheel.curve should be > 0".into());
    }
    if s.max_mult < 1.0 {
        warn("wheel.max_multiplier should be >= 1.0".into());
    }
    if s.min_mult <= 0.0 {
        warn("wheel.min_multiplier should be > 0".into());
    }
    if s.min_mult > s.max_mult {
        warn("wheel.min_multiplier is above max_multiplier (curve flattens to max)".into());
    }
    if s.step_start < 1.0 {
        warn("wheel.steps_above should be >= 1.0".into());
    }
    for (name, v) in [("smoothing_up", s.attack), ("smoothing_down", s.release)] {
        if v <= 0.0 || v > 1.0 {
            warn(format!("wheel.{name} should be in (0, 1] (got {v})"));
        }
    }
    if s.ptr_base <= 0.0 {
        warn("pointer.precision_gain should be > 0".into());
    }
    if s.ptr_max < s.ptr_base {
        warn("pointer.max_gain is below precision_gain (curve will invert)".into());
    }
    if s.ptr_mid <= 0.0 {
        warn("pointer.midpoint_speed should be > 0".into());
    }
    if s.ptr_width <= 0.0 {
        warn("pointer.transition_width should be > 0".into());
    }
    if s.ptr_tau <= 0.0 {
        warn("pointer.smoothing_ms should be > 0".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn default_is_mac_like() {
        let s = ConfigFile::default().resolve_global();
        assert!(s.wheel_enabled && s.pointer_accel);
        assert!(s.wheel_discrete_steps); // on by default so App Center-class apps accelerate
        assert!(approx(s.threshold_dps, 5.0));
        assert!(approx(s.accel, 0.06));
        assert!(approx(s.max_mult, 6.0));
        // Slight slow-speed de-amp + notches only past 1.5× (tuned by feel).
        assert!(approx(s.min_mult, 0.9));
        assert!(approx(s.step_start, 1.5));
        // dpi == reference, so the curve is unscaled
        assert!(approx(s.ptr_base, 0.5));
        assert!(approx(s.ptr_max, 2.5));
        assert!(approx(s.ptr_mid, 4000.0));
        assert!(approx(s.dpi, 1400.0));
    }

    #[test]
    fn wheel_min_and_step_overrides_apply() {
        let cf: ConfigFile = toml::from_str(
            "preset = \"mac-like\"\n[wheel]\nmin_multiplier = 0.8\nsteps_above = 2.0\n",
        )
        .unwrap();
        let s = cf.resolve_global();
        assert!(approx(s.min_mult, 0.8));
        assert!(approx(s.step_start, 2.0));
        // other wheel knobs still come from the preset
        assert!(approx(s.max_mult, 6.0));
    }

    #[test]
    fn preset_off_disables_both() {
        let cf: ConfigFile = toml::from_str("preset = \"off\"").unwrap();
        let s = cf.resolve_global();
        assert!(!s.wheel_enabled);
        assert!(!s.pointer_accel);
    }

    #[test]
    fn global_override_layers_on_preset() {
        let cf: ConfigFile = toml::from_str(
            "preset = \"mac-like\"\n[pointer]\nmax_gain = 4.0\n[wheel]\nenabled = false\n",
        )
        .unwrap();
        let s = cf.resolve_global();
        assert!(approx(s.ptr_max, 4.0)); // overridden
        assert!(approx(s.ptr_base, 0.5)); // preset default kept
        assert!(!s.wheel_enabled); // overridden
    }

    #[test]
    fn dpi_rescale_keeps_feel() {
        // k = 2800/1400 = 2: speed breakpoints double, gains halve.
        let cf: ConfigFile = toml::from_str("preset = \"mac-like\"\ndpi = 2800\n").unwrap();
        let s = cf.resolve_global();
        assert!(approx(s.ptr_mid, 8000.0));
        assert!(approx(s.ptr_width, 4000.0));
        assert!(approx(s.ptr_base, 0.25));
        assert!(approx(s.ptr_max, 1.25));
    }

    #[test]
    fn per_device_rule_matches_and_overrides() {
        let cf: ConfigFile = toml::from_str(
            "preset = \"mac-like\"\n\
             [[device]]\nmatch = \"Logitech\"\npreset = \"off\"\n\
             [[device]]\nmatch = \"Trackball\"\n[device.pointer]\nenabled = false\n",
        )
        .unwrap();
        // non-matching device falls back to the global preset
        assert!(cf.resolve("Razer DeathAdder").pointer_accel);
        // "Logitech" → off preset
        let log = cf.resolve("Logitech USB Receiver Mouse");
        assert!(!log.wheel_enabled && !log.pointer_accel);
        // "Trackball" → mac-like base, pointer disabled by the device override
        let tb = cf.resolve("Kensington Trackball");
        assert!(tb.wheel_enabled && !tb.pointer_accel);
    }

    #[test]
    fn unknown_preset_falls_back_without_panicking() {
        let cf: ConfigFile = toml::from_str("preset = \"bogus\"").unwrap();
        let s = cf.resolve_global();
        assert!(approx(s.threshold_dps, 5.0)); // mac-like fallback
    }

    #[test]
    fn shipped_example_is_valid() {
        let cf: ConfigFile = toml::from_str(DEFAULT_TEMPLATE).unwrap();
        let _ = cf.resolve_global();
        assert_eq!(cf.preset, "mac-like");
    }

    #[test]
    fn custom_preset_resolves_and_round_trips() {
        let cf: ConfigFile = toml::from_str(
            "preset = \"mine\"\n\
             [presets.mine]\ndpi = 1600\n\
             [presets.mine.pointer]\nmax_gain = 4.0\n\
             [[device]]\nmatch = \"Acer\"\npreset = \"mine\"\n",
        )
        .unwrap();
        assert!(cf.has_preset("mine"));
        assert!(cf.preset_names().iter().any(|n| n == "mine"));
        let s = cf.resolve_config_space("Acer BT Keyboard1 Mouse");
        assert!(approx(s.ptr_max, 4.0));
        assert!(approx(s.ptr_base, 0.5)); // inherited from mac-like
        assert!(approx(s.dpi, 1600.0));

        let text = toml::to_string_pretty(&cf).unwrap();
        let back: ConfigFile = toml::from_str(&text).unwrap();
        assert_eq!(back.presets.len(), 1);
    }

    #[test]
    fn per_device_click_lock_layers() {
        let cf: ConfigFile = toml::from_str(
            "[[device]]\nmatch = \"Acer\"\n\
             [device.click_lock]\nenabled = true\nhold_ms = 200\nrelease_ms = 450\n",
        )
        .unwrap();
        let s = cf.resolve("Acer BT Keyboard1 Mouse");
        assert!(s.click_lock);
        assert_eq!(s.click_lock_hold, Duration::from_millis(200));
        assert_eq!(s.click_lock_release, Duration::from_millis(450));
        let other = cf.resolve("Other Mouse");
        assert!(!other.click_lock);
        assert_eq!(other.click_lock_hold, CLICK_LOCK_HOLD);
        assert!(cf.click_lock_for("Acer BT Keyboard1 Mouse"));
        assert!(!cf.click_lock_for("Other Mouse"));
    }

    #[test]
    fn legacy_button_table_is_ignored() {
        let cf: ConfigFile = toml::from_str(
            "preset = \"mac-like\"\n\
             [[button]]\nmatch = \"BTN_SIDE\"\nkeys = [\"Super\"]\n",
        )
        .unwrap();
        assert_eq!(cf.preset, "mac-like");
    }
}
