//! `tune` — a live terminal UI for profiles and device bindings.
//!
//! The sidebar always shows the profiles and the devices they are bound to.
//! Edits apply to the selected profile immediately through the daemon socket;
//! `s` writes the whole config to disk.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Chart, Clear, Dataset, GraphType, Paragraph, Tabs, Wrap,
};
use ratatui::{DefaultTerminal, Frame};

use crate::config::{ClickLockCfg, ConfigFile, DeviceRule, PresetDef, Settings, REFERENCE_DPI};
use crate::ipc::{DeviceInfo, Request, Response, TelemetrySample, SOCKET_PATH};

const ACCENT: Color = Color::Cyan;
const CURVE: Color = Color::LightGreen;
const MARKER: Color = Color::LightRed;
const BAR_FILL: Color = Color::Cyan;
const BAR_EMPTY: Color = Color::DarkGray;
const KEY: Color = Color::Yellow;

// ---------------------------------------------------------------------------
// Socket client
// ---------------------------------------------------------------------------

struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    fn connect() -> Result<Self, String> {
        let stream = UnixStream::connect(SOCKET_PATH).map_err(|e| e.to_string())?;
        let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        Ok(Client {
            writer: stream,
            reader,
        })
    }

    fn call(&mut self, req: &Request) -> Result<Response, String> {
        let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .map_err(|e| e.to_string())?;
        let mut resp = String::new();
        let n = self
            .reader
            .read_line(&mut resp)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("daemon closed the connection".into());
        }
        serde_json::from_str(&resp).map_err(|e| e.to_string())
    }

    fn get_config(&mut self) -> Result<ConfigFile, String> {
        match self.call(&Request::GetConfig)? {
            Response::Config { config } => Ok(*config),
            Response::Error { message } => Err(message),
            _ => Err("unexpected response".into()),
        }
    }

    fn set_config(&mut self, cfg: &ConfigFile) -> Result<(), String> {
        match self.call(&Request::SetConfig {
            config: Box::new(cfg.clone()),
        })? {
            Response::Ok => Ok(()),
            Response::Error { message } => Err(message),
            _ => Err("unexpected response".into()),
        }
    }

    fn save(&mut self) -> Result<(), String> {
        match self.call(&Request::Save)? {
            Response::Ok => Ok(()),
            Response::Error { message } => Err(message),
            _ => Err("unexpected response".into()),
        }
    }

    fn telemetry(&mut self) -> Result<TelemetrySample, String> {
        match self.call(&Request::GetTelemetry)? {
            Response::Telemetry(t) => Ok(t),
            Response::Error { message } => Err(message),
            _ => Err("unexpected response".into()),
        }
    }

    fn devices(&mut self) -> Result<Vec<DeviceInfo>, String> {
        match self.call(&Request::ListDevices)? {
            Response::Devices { devices } => Ok(devices),
            Response::Error { message } => Err(message),
            _ => Err("unexpected response".into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Editable fields
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Field {
    PtrEnabled,
    Precision,
    MaxGain,
    Midpoint,
    Width,
    PtrSmoothing,
    WheelEnabled,
    StartSpeed,
    Strength,
    Curve,
    MinMult,
    MaxMult,
    StepStart,
    SmoothUp,
    SmoothDown,
    ResetMs,
    Dpi,
}

const PTR_FIELDS: [Field; 7] = [
    Field::PtrEnabled,
    Field::Precision,
    Field::MaxGain,
    Field::Midpoint,
    Field::Width,
    Field::PtrSmoothing,
    Field::Dpi,
];
const WHEEL_FIELDS: [Field; 11] = [
    Field::WheelEnabled,
    Field::StartSpeed,
    Field::Strength,
    Field::Curve,
    Field::MinMult,
    Field::MaxMult,
    Field::StepStart,
    Field::SmoothUp,
    Field::SmoothDown,
    Field::ResetMs,
    Field::Dpi,
];

impl Field {
    fn label(self) -> &'static str {
        match self {
            Field::PtrEnabled => "Pointer accel",
            Field::Precision => "Precision (slow)",
            Field::MaxGain => "Reach (fast)",
            Field::Midpoint => "Knee speed",
            Field::Width => "Transition width",
            Field::PtrSmoothing => "Smoothing",
            Field::WheelEnabled => "Wheel accel",
            Field::StartSpeed => "Start speed",
            Field::Strength => "Strength",
            Field::Curve => "Curve",
            Field::MinMult => "Min multiplier",
            Field::MaxMult => "Max multiplier",
            Field::StepStart => "Steps above",
            Field::SmoothUp => "Speed-up smooth",
            Field::SmoothDown => "Slow-down smooth",
            Field::ResetMs => "Reset pause",
            Field::Dpi => "Mouse DPI",
        }
    }

    fn is_bool(self) -> bool {
        matches!(self, Field::PtrEnabled | Field::WheelEnabled)
    }

    fn bounds(self) -> (f64, f64) {
        match self {
            Field::Precision => (0.1, 3.0),
            Field::MaxGain => (0.5, 8.0),
            Field::Midpoint => (200.0, 12000.0),
            Field::Width => (100.0, 8000.0),
            Field::PtrSmoothing => (1.0, 60.0),
            Field::StartSpeed => (0.0, 40.0),
            Field::Strength => (0.0, 1.0),
            Field::Curve => (0.5, 2.5),
            Field::MinMult => (0.2, 2.0),
            Field::MaxMult => (1.0, 20.0),
            Field::StepStart => (1.0, 10.0),
            Field::SmoothUp | Field::SmoothDown => (0.05, 1.0),
            Field::ResetMs => (40.0, 600.0),
            Field::Dpi => (200.0, 6400.0),
            Field::PtrEnabled | Field::WheelEnabled => (0.0, 1.0),
        }
    }

    fn step(self) -> f64 {
        match self {
            Field::Precision => 0.05,
            Field::MaxGain => 0.1,
            Field::Midpoint => 250.0,
            Field::Width => 250.0,
            Field::PtrSmoothing => 1.0,
            Field::StartSpeed => 1.0,
            Field::Strength => 0.01,
            Field::Curve => 0.05,
            Field::MinMult => 0.05,
            Field::MaxMult => 0.5,
            Field::StepStart => 0.5,
            Field::SmoothUp | Field::SmoothDown => 0.05,
            Field::ResetMs => 10.0,
            Field::Dpi => 50.0,
            Field::PtrEnabled | Field::WheelEnabled => 1.0,
        }
    }

    fn decimals(self) -> usize {
        match self {
            Field::Precision
            | Field::MaxGain
            | Field::Strength
            | Field::Curve
            | Field::MinMult
            | Field::SmoothUp
            | Field::SmoothDown => 2,
            Field::MaxMult | Field::StepStart => 1,
            _ => 0,
        }
    }

    fn unit(self) -> &'static str {
        match self {
            Field::Midpoint | Field::Width => " cnt/s",
            Field::StartSpeed => " det/s",
            Field::PtrSmoothing | Field::ResetMs => " ms",
            Field::MaxGain | Field::MaxMult | Field::MinMult | Field::StepStart => "×",
            _ => "",
        }
    }
}

fn preset_value(p: &PresetDef, f: Field) -> f64 {
    let s = p.to_settings();
    match f {
        Field::PtrEnabled => s.pointer_accel as i32 as f64,
        Field::Precision => s.ptr_base,
        Field::MaxGain => s.ptr_max,
        Field::Midpoint => s.ptr_mid,
        Field::Width => s.ptr_width,
        Field::PtrSmoothing => s.ptr_tau * 1000.0,
        Field::WheelEnabled => s.wheel_enabled as i32 as f64,
        Field::StartSpeed => s.threshold_dps,
        Field::Strength => s.accel,
        Field::Curve => s.exponent,
        Field::MinMult => s.min_mult,
        Field::MaxMult => s.max_mult,
        Field::StepStart => s.step_start,
        Field::SmoothUp => s.attack,
        Field::SmoothDown => s.release,
        Field::ResetMs => s.reset_gap.as_secs_f64() * 1000.0,
        Field::Dpi => s.dpi,
    }
}

fn preset_set(p: &mut PresetDef, f: Field, v: f64) {
    match f {
        Field::Precision => p.pointer.precision_gain = Some(v),
        Field::MaxGain => p.pointer.max_gain = Some(v),
        Field::Midpoint => p.pointer.midpoint_speed = Some(v),
        Field::Width => p.pointer.transition_width = Some(v),
        Field::PtrSmoothing => p.pointer.smoothing_ms = Some(v),
        Field::StartSpeed => p.wheel.start_speed = Some(v),
        Field::Strength => p.wheel.strength = Some(v),
        Field::Curve => p.wheel.curve = Some(v),
        Field::MinMult => p.wheel.min_multiplier = Some(v),
        Field::MaxMult => p.wheel.max_multiplier = Some(v),
        Field::StepStart => p.wheel.steps_above = Some(v),
        Field::SmoothUp => p.wheel.smoothing_up = Some(v),
        Field::SmoothDown => p.wheel.smoothing_down = Some(v),
        Field::ResetMs => p.wheel.reset_after_ms = Some(v),
        Field::Dpi => p.dpi = Some(v),
        Field::PtrEnabled | Field::WheelEnabled => {}
    }
}

fn preset_adjust(p: &mut PresetDef, f: Field, dir: f64) {
    if f.is_bool() {
        let on = preset_value(p, f) > 0.5;
        match f {
            Field::PtrEnabled => p.pointer.enabled = Some(!on),
            Field::WheelEnabled => p.wheel.enabled = Some(!on),
            _ => {}
        }
        return;
    }
    let (lo, hi) = f.bounds();
    let step = f.step();
    let raw = preset_value(p, f) + dir * step;
    let snapped = (raw / step).round() * step;
    preset_set(p, f, snapped.clamp(lo, hi));
}

// ---------------------------------------------------------------------------
// Curve math (mirrors pointer.rs / wheel.rs, config-space)
// ---------------------------------------------------------------------------

fn pointer_gain(s: &Settings, v: f64) -> f64 {
    let sig = 1.0 / (1.0 + (-(v - s.ptr_mid) / s.ptr_width).exp());
    s.ptr_base + (s.ptr_max - s.ptr_base) * sig
}

fn wheel_mult(s: &Settings, dps: f64) -> f64 {
    let over = dps - s.threshold_dps;
    let grown = if over <= 0.0 {
        s.min_mult
    } else {
        s.min_mult + s.accel * over.powf(s.exponent)
    };
    grown.min(s.max_mult)
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Pointer,
    Wheel,
}

impl Tab {
    const ALL: [Tab; 2] = [Tab::Pointer, Tab::Wheel];
    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
    fn title(self) -> &'static str {
        match self {
            Tab::Pointer => "Pointer",
            Tab::Wheel => "Wheel",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Profiles,
    Devices,
    Knobs,
}

impl Focus {
    fn next(self) -> Focus {
        match self {
            Focus::Profiles => Focus::Devices,
            Focus::Devices => Focus::Knobs,
            Focus::Knobs => Focus::Profiles,
        }
    }
    fn prev(self) -> Focus {
        match self {
            Focus::Profiles => Focus::Knobs,
            Focus::Devices => Focus::Profiles,
            Focus::Knobs => Focus::Devices,
        }
    }
}

#[derive(Clone)]
enum DeviceRow {
    Global,
    Discovered {
        info: DeviceInfo,
        rule: Option<usize>,
    },
    RuleOnly(usize),
}

enum Modal {
    None,
    NewProfile { input: String },
    DeleteProfile { name: String, refs: Vec<String> },
}

struct App {
    client: Client,
    cfg: ConfigFile,
    saved: ConfigFile,
    devices: Vec<DeviceInfo>,
    last_devices: Instant,
    tab: Tab,
    focus: Focus,
    profile_sel: usize,
    device_sel: usize,
    sel: usize,
    tel: TelemetrySample,
    modal: Modal,
    status: String,
    confirm_quit: bool,
    quit: bool,
}

impl App {
    fn new(client: Client, cfg: ConfigFile, devices: Vec<DeviceInfo>) -> Self {
        App {
            saved: cfg.clone(),
            client,
            cfg,
            devices,
            last_devices: Instant::now(),
            tab: Tab::Pointer,
            focus: Focus::Profiles,
            profile_sel: 0,
            device_sel: 0,
            sel: 0,
            tel: TelemetrySample::default(),
            modal: Modal::None,
            status: "select a profile, tune it, then assign it to a device with a".into(),
            confirm_quit: false,
            quit: false,
        }
    }

    /// Fold legacy global/device overrides into custom profiles so the UI can
    /// show exactly what the daemon is using. The change is live but not saved
    /// until the user presses `s`.
    fn import_legacy_overrides(&mut self) {
        let mut imported = Vec::new();

        // Import device overrides first, while global overrides are still in
        // effect, so their effective values are preserved.
        for i in 0..self.cfg.device.len() {
            let rule = &self.cfg.device[i];
            let has_overrides =
                !rule.wheel.is_empty() || !rule.pointer.is_empty() || rule.dpi.is_some();
            if !has_overrides || rule.match_.is_empty() {
                continue;
            }
            let match_ = rule.match_.clone();
            let eff = self.cfg.resolve_config_space(&match_);
            let name = self.unique_profile_name(&format!("{match_} profile"));
            self.cfg
                .presets
                .insert(name.clone(), PresetDef::from_settings(&eff));
            let rule = &mut self.cfg.device[i];
            rule.preset = Some(name.clone());
            rule.wheel = Default::default();
            rule.pointer = Default::default();
            rule.dpi = None;
            imported.push(name);
        }

        let has_global =
            !self.cfg.wheel.is_empty() || !self.cfg.pointer.is_empty() || self.cfg.dpi.is_some();
        if has_global {
            let eff = self.cfg.resolve_config_space("");
            let name = self.unique_profile_name(&format!("{} custom", self.cfg.preset));
            self.cfg
                .presets
                .insert(name.clone(), PresetDef::from_settings(&eff));
            self.cfg.preset = name.clone();
            self.cfg.wheel = Default::default();
            self.cfg.pointer = Default::default();
            self.cfg.dpi = None;
            imported.push(name);
        }

        if !imported.is_empty() {
            if let Some(i) = self.profiles().iter().position(|p| p == &self.cfg.preset) {
                self.profile_sel = i;
            }
            self.clamp_selections();
            self.status = format!(
                "imported existing overrides into {} profile(s) — s saves",
                imported.len()
            );
            self.touch("");
        }
    }

    fn dirty(&self) -> bool {
        self.cfg != self.saved
    }

    fn profiles(&self) -> Vec<String> {
        self.cfg.preset_names()
    }

    fn selected_profile(&self) -> Option<String> {
        self.profiles().get(self.profile_sel).cloned()
    }

    fn selected_settings(&self) -> Settings {
        self.selected_profile()
            .and_then(|n| self.cfg.preset_settings(&n))
            .unwrap_or_else(|| self.cfg.resolve_config_space(""))
    }

    fn knob_fields(&self) -> &'static [Field] {
        match self.tab {
            Tab::Pointer => &PTR_FIELDS,
            Tab::Wheel => &WHEEL_FIELDS,
        }
    }

    fn device_rows(&self) -> Vec<DeviceRow> {
        let mut rows = vec![DeviceRow::Global];
        let mut used = vec![false; self.cfg.device.len()];
        for info in &self.devices {
            let rule = self.cfg.rule_index(&info.name);
            if let Some(i) = rule {
                used[i] = true;
            }
            rows.push(DeviceRow::Discovered {
                info: info.clone(),
                rule,
            });
        }
        for (i, _) in self.cfg.device.iter().enumerate() {
            if !used[i] {
                rows.push(DeviceRow::RuleOnly(i));
            }
        }
        rows
    }

    fn selected_device(&self) -> Option<DeviceRow> {
        self.device_rows().get(self.device_sel).cloned()
    }

    fn target_name(&self) -> Option<String> {
        match self.selected_device()? {
            DeviceRow::Global => None,
            DeviceRow::Discovered { info, .. } => Some(info.name),
            DeviceRow::RuleOnly(i) => self.cfg.device.get(i).map(|r| r.match_.clone()),
        }
    }

    fn target_label(&self) -> String {
        match self.selected_device() {
            Some(DeviceRow::Global) | None => "Global".into(),
            Some(DeviceRow::Discovered { info, .. }) => info.name,
            Some(DeviceRow::RuleOnly(i)) => self
                .cfg
                .device
                .get(i)
                .map(|r| r.match_.clone())
                .unwrap_or_else(|| "device".into()),
        }
    }

    fn lock_label(&self, row: &DeviceRow) -> String {
        let enabled = match row {
            DeviceRow::Global => false,
            DeviceRow::Discovered { rule, .. } => rule
                .and_then(|i| self.cfg.device.get(i))
                .and_then(|r| r.click_lock.enabled)
                .unwrap_or(false),
            DeviceRow::RuleOnly(i) => self
                .cfg
                .device
                .get(*i)
                .and_then(|r| r.click_lock.enabled)
                .unwrap_or(false),
        };
        if enabled {
            "  lock:on".into()
        } else {
            "  lock:off".into()
        }
    }

    fn effective_preset_for(&self, row: &DeviceRow) -> String {
        match row {
            DeviceRow::Global => self.cfg.preset.clone(),
            DeviceRow::Discovered { rule, .. } => rule
                .and_then(|i| self.cfg.device.get(i))
                .and_then(|r| r.preset.clone())
                .unwrap_or_else(|| self.cfg.preset.clone()),
            DeviceRow::RuleOnly(i) => self
                .cfg
                .device
                .get(*i)
                .and_then(|r| r.preset.clone())
                .unwrap_or_else(|| self.cfg.preset.clone()),
        }
    }

    fn sync_profile_to_device(&mut self) {
        let Some(row) = self.selected_device() else {
            return;
        };
        let name = self.effective_preset_for(&row);
        if let Some(i) = self.profiles().iter().position(|p| p == &name) {
            self.profile_sel = i;
        }
    }

    fn clamp_selections(&mut self) {
        let np = self.profiles().len().max(1);
        self.profile_sel = self.profile_sel.min(np - 1);
        let nd = self.device_rows().len().max(1);
        self.device_sel = self.device_sel.min(nd - 1);
        let nk = self.knob_fields().len().max(1);
        self.sel = self.sel.min(nk - 1);
    }

    fn unique_profile_name(&self, base: &str) -> String {
        let base = if base.trim().is_empty() {
            "profile"
        } else {
            base.trim()
        };
        if !self.cfg.presets.contains_key(base) {
            return base.to_string();
        }
        for n in 2..1000 {
            let candidate = format!("{base} {n}");
            if !self.cfg.presets.contains_key(&candidate) {
                return candidate;
            }
        }
        format!("{base} copy")
    }

    fn bind_profile(&mut self, device_name: &str, profile: &str) {
        if let Some(i) = self.cfg.rule_index(device_name) {
            let r = &mut self.cfg.device[i];
            r.preset = Some(profile.to_string());
            r.wheel = Default::default();
            r.pointer = Default::default();
            r.dpi = None;
        } else {
            self.cfg.device.push(DeviceRule {
                match_: device_name.to_string(),
                preset: Some(profile.to_string()),
                ..Default::default()
            });
        }
    }

    /// Make sure the selected profile is a custom one we can edit. Built-ins and
    /// targets carrying legacy overrides are folded into a fresh custom profile.
    fn ensure_editable_profile(&mut self) -> Option<String> {
        let name = self.selected_profile()?;
        let target = self.target_name();
        let is_custom = self.cfg.presets.contains_key(&name);
        let global_overrides =
            !self.cfg.wheel.is_empty() || !self.cfg.pointer.is_empty() || self.cfg.dpi.is_some();
        let device_overrides = target
            .as_deref()
            .and_then(|t| self.cfg.rule_for(t))
            .map(|r| !r.wheel.is_empty() || !r.pointer.is_empty() || r.dpi.is_some())
            .unwrap_or(false);
        let needs_fork = !is_custom
            || (target.is_none() && global_overrides)
            || (target.is_some() && device_overrides);
        if !needs_fork {
            return Some(name);
        }

        let effective = if let Some(t) = target.as_deref() {
            if device_overrides {
                self.cfg.resolve_config_space(t)
            } else {
                self.cfg
                    .preset_settings(&name)
                    .unwrap_or_else(|| self.cfg.resolve_config_space(t))
            }
        } else if global_overrides {
            self.cfg.resolve_config_space("")
        } else {
            self.cfg
                .preset_settings(&name)
                .unwrap_or_else(|| self.cfg.resolve_config_space(""))
        };
        let base = if target.is_some() {
            format!("{} profile", self.target_label())
        } else if is_custom {
            format!("{name} custom")
        } else {
            format!("{name} copy")
        };
        let new = self.unique_profile_name(&base);
        self.cfg
            .presets
            .insert(new.clone(), PresetDef::from_settings(&effective));
        if let Some(t) = target {
            self.bind_profile(&t, &new);
        } else {
            self.cfg.preset = new.clone();
            self.cfg.wheel = Default::default();
            self.cfg.pointer = Default::default();
            self.cfg.dpi = None;
        }
        if let Some(i) = self.profiles().iter().position(|p| p == &new) {
            self.profile_sel = i;
        }
        self.status = format!("created profile '{new}' from current settings (live)");
        self.touch("");
        Some(new)
    }

    fn focus_next(&mut self) {
        self.focus = self.focus.next();
    }
    fn focus_prev(&mut self) {
        self.focus = self.focus.prev();
    }

    fn move_sel(&mut self, dir: i32) {
        match self.focus {
            Focus::Profiles => {
                let n = self.profiles().len();
                if n > 0 {
                    let i = self.profile_sel as i32 + dir;
                    self.profile_sel = i.clamp(0, n as i32 - 1) as usize;
                }
            }
            Focus::Devices => {
                let n = self.device_rows().len();
                if n > 0 {
                    let i = self.device_sel as i32 + dir;
                    self.device_sel = i.clamp(0, n as i32 - 1) as usize;
                    self.sync_profile_to_device();
                }
            }
            Focus::Knobs => {
                let n = self.knob_fields().len();
                if n > 0 {
                    let i = self.sel as i32 + dir;
                    self.sel = i.clamp(0, n as i32 - 1) as usize;
                }
            }
        }
    }

    fn adjust(&mut self, dir: f64, big: bool) {
        if self.focus != Focus::Knobs {
            return;
        }
        let Some(name) = self.ensure_editable_profile() else {
            return;
        };
        let Some(field) = self.knob_fields().get(self.sel).copied() else {
            return;
        };
        let Some(p) = self.cfg.presets.get_mut(&name) else {
            return;
        };
        let mult = if field.is_bool() || !big { 1.0 } else { 5.0 };
        preset_adjust(p, field, dir * mult);
        self.touch("");
    }

    fn toggle(&mut self) {
        match self.focus {
            Focus::Profiles => self.focus = Focus::Knobs,
            Focus::Devices => self.focus = Focus::Profiles,
            Focus::Knobs => {
                if self
                    .knob_fields()
                    .get(self.sel)
                    .is_some_and(|f| f.is_bool())
                {
                    self.adjust(1.0, false);
                }
            }
        }
    }

    fn start_new_profile(&mut self) {
        let base = self.selected_profile().unwrap_or_else(|| "profile".into());
        let input = self.unique_profile_name(&format!("{base} copy"));
        self.modal = Modal::NewProfile { input };
        self.status = "name the new profile and press Enter".into();
    }

    fn create_profile(&mut self, input: &str) {
        let name = input.trim().to_string();
        if name.is_empty() {
            self.status = "profile name cannot be empty".into();
            return;
        }
        if ConfigFile::is_builtin_preset(&name) {
            self.status = "that name is reserved by a built-in profile".into();
            return;
        }
        if self.cfg.presets.contains_key(&name) {
            self.status = format!("profile '{name}' already exists");
            return;
        }
        let target = self.target_name();
        let effective = self
            .selected_profile()
            .and_then(|p| self.cfg.preset_settings(&p))
            .unwrap_or_else(|| {
                self.cfg
                    .resolve_config_space(target.as_deref().unwrap_or(""))
            });
        self.cfg
            .presets
            .insert(name.clone(), PresetDef::from_settings(&effective));
        if let Some(t) = target {
            self.bind_profile(&t, &name);
        } else {
            self.cfg.preset = name.clone();
            self.cfg.wheel = Default::default();
            self.cfg.pointer = Default::default();
            self.cfg.dpi = None;
        }
        if let Some(i) = self.profiles().iter().position(|p| p == &name) {
            self.profile_sel = i;
        }
        self.modal = Modal::None;
        self.status = format!("created and assigned profile '{name}' (live)");
        self.touch("");
    }

    fn update_profile(&mut self) {
        let Some(name) = self.selected_profile() else {
            return;
        };
        if !self.cfg.presets.contains_key(&name) {
            self.status = format!("'{name}' is built-in; use n to save a copy");
            return;
        }
        let target_profile = self
            .selected_device()
            .map(|r| self.effective_preset_for(&r))
            .unwrap_or_else(|| self.cfg.preset.clone());
        if target_profile != name {
            self.status = "select the profile bound to the target (or press a to bind it)".into();
            return;
        }
        let target = self.target_name();
        let effective = self
            .selected_profile()
            .and_then(|p| self.cfg.preset_settings(&p))
            .unwrap_or_else(|| {
                self.cfg
                    .resolve_config_space(target.as_deref().unwrap_or(""))
            });
        self.cfg
            .presets
            .insert(name.clone(), PresetDef::from_settings(&effective));
        if let Some(t) = target {
            self.bind_profile(&t, &name);
        } else {
            self.cfg.wheel = Default::default();
            self.cfg.pointer = Default::default();
            self.cfg.dpi = None;
        }
        self.status = format!("updated profile '{name}' from current settings (live)");
        self.touch("");
    }

    fn start_delete_profile(&mut self) {
        let Some(name) = self.selected_profile() else {
            return;
        };
        if !self.cfg.presets.contains_key(&name) {
            self.status = format!("'{name}' is built-in and cannot be deleted");
            return;
        }
        let mut refs = Vec::new();
        if self.cfg.preset == name {
            refs.push("global".to_string());
        }
        for r in &self.cfg.device {
            if r.preset.as_deref() == Some(name.as_str()) {
                refs.push(format!("{:?}", r.match_));
            }
        }
        self.modal = Modal::DeleteProfile { name, refs };
    }

    fn delete_profile(&mut self, name: &str) {
        self.cfg.presets.remove(name);
        if self.cfg.preset == name {
            self.cfg.preset = "mac-like".into();
        }
        for r in &mut self.cfg.device {
            if r.preset.as_deref() == Some(name) {
                r.preset = Some("mac-like".into());
            }
        }
        self.clamp_selections();
        self.status = format!("deleted profile '{name}'; references reassigned to mac-like");
        self.touch("");
    }

    fn assign_profile(&mut self) {
        let Some(profile) = self.selected_profile() else {
            return;
        };
        let Some(row) = self.selected_device() else {
            return;
        };
        match row {
            DeviceRow::Global => {
                self.cfg.preset = profile.clone();
                self.cfg.wheel = Default::default();
                self.cfg.pointer = Default::default();
                self.cfg.dpi = None;
                self.status = format!("global fallback → '{profile}' (live)");
            }
            DeviceRow::Discovered { info, .. } => {
                self.bind_profile(&info.name, &profile);
                self.status = format!("{} → '{profile}' (live)", info.name);
            }
            DeviceRow::RuleOnly(i) => {
                if let Some(r) = self.cfg.device.get_mut(i) {
                    r.preset = Some(profile.clone());
                    r.wheel = Default::default();
                    r.pointer = Default::default();
                    r.dpi = None;
                    self.status = format!("{:?} → '{profile}' (live)", r.match_);
                }
            }
        }
        self.touch("");
    }

    fn toggle_click_lock(&mut self) {
        let Some(row) = self.selected_device() else {
            return;
        };
        match row {
            DeviceRow::Global => {
                self.status = "click lock is configured per device; select one".into();
            }
            DeviceRow::Discovered { info, rule } => {
                if let Some(i) = rule {
                    let lock = &mut self.cfg.device[i].click_lock;
                    let on = lock.enabled.unwrap_or(false);
                    lock.enabled = Some(!on);
                    self.status = format!(
                        "{} click lock {} (live)",
                        info.name,
                        if on { "off" } else { "on" }
                    );
                } else {
                    self.cfg.device.push(DeviceRule {
                        match_: info.name.clone(),
                        click_lock: ClickLockCfg {
                            enabled: Some(true),
                            ..Default::default()
                        },
                        ..Default::default()
                    });
                    self.status = format!("{} click lock on (live)", info.name);
                }
                self.touch("");
            }
            DeviceRow::RuleOnly(i) => {
                if let Some(r) = self.cfg.device.get_mut(i) {
                    let on = r.click_lock.enabled.unwrap_or(false);
                    r.click_lock.enabled = Some(!on);
                    self.status = format!(
                        "{:?} click lock {} (live)",
                        r.match_,
                        if on { "off" } else { "on" }
                    );
                    self.touch("");
                }
            }
        }
    }

    fn reset_profile(&mut self) {
        let Some(name) = self.selected_profile() else {
            return;
        };
        if let Some(p) = self.saved.presets.get(&name).cloned() {
            self.cfg.presets.insert(name.clone(), p);
            self.status = format!("reset '{name}' to its saved values");
            self.touch("");
        } else {
            self.status = format!("'{name}' has no saved custom copy to reset to");
        }
    }

    fn touch(&mut self, msg: &str) {
        if let Err(e) = self.client.set_config(&self.cfg) {
            self.status = format!("live-apply failed: {e}");
        } else if !msg.is_empty() {
            self.status = msg.to_string();
        }
    }

    fn do_save(&mut self) {
        match self.client.save() {
            Ok(()) => {
                self.saved = self.cfg.clone();
                self.confirm_quit = false;
                self.status = "saved to /etc/wayland-mouse/config.toml ✓".into();
            }
            Err(e) => self.status = format!("save failed: {e}"),
        }
    }

    fn refresh_devices(&mut self) {
        if let Ok(devices) = self.client.devices() {
            self.devices = devices;
            self.last_devices = Instant::now();
            self.clamp_selections();
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        while !self.quit {
            match self.client.telemetry() {
                Ok(t) => self.tel = t,
                Err(e) => self.status = format!("disconnected: {e}"),
            }
            if self.last_devices.elapsed() > Duration::from_secs(1) {
                self.refresh_devices();
            }
            terminal.draw(|f| ui(f, self))?;
            if event::poll(Duration::from_millis(33))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind != KeyEventKind::Release {
                        self.on_key(k.code, k.modifiers);
                    }
                }
            }
        }
        Ok(())
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let had_modal = !matches!(self.modal, Modal::None);
        match &mut self.modal {
            Modal::NewProfile { input } => match code {
                KeyCode::Char(c) => input.push(c),
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Esc => {
                    self.modal = Modal::None;
                    self.status = "cancelled".into();
                }
                KeyCode::Enter => {
                    let input = input.clone();
                    self.create_profile(&input);
                }
                _ => {}
            },
            Modal::DeleteProfile { name, .. } => match code {
                KeyCode::Enter => {
                    let name = name.clone();
                    self.modal = Modal::None;
                    self.delete_profile(&name);
                }
                KeyCode::Esc => {
                    self.modal = Modal::None;
                    self.status = "delete cancelled".into();
                }
                _ => {}
            },
            Modal::None => {}
        }
        if had_modal {
            return;
        }

        let big = mods.contains(KeyModifiers::SHIFT);
        if !matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
            self.confirm_quit = false;
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if self.dirty() && !self.confirm_quit {
                    self.confirm_quit = true;
                    self.status = "unsaved changes — press s to save, or q/Esc again".into();
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Tab => self.focus_next(),
            KeyCode::BackTab => self.focus_prev(),
            KeyCode::Char('1') => {
                self.tab = Tab::Pointer;
                self.sel = 0;
            }
            KeyCode::Char('2') => {
                self.tab = Tab::Wheel;
                self.sel = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('-') => self.adjust(-1.0, big),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('+') | KeyCode::Char('=') => {
                self.adjust(1.0, big)
            }
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle(),
            KeyCode::Char('n') => self.start_new_profile(),
            KeyCode::Char('u') => self.update_profile(),
            KeyCode::Char('d') => self.start_delete_profile(),
            KeyCode::Char('a') => self.assign_profile(),
            KeyCode::Char('t') => self.toggle_click_lock(),
            KeyCode::Char('r') => self.reset_profile(),
            KeyCode::Char('s') => self.do_save(),
            KeyCode::Char('R') => self.refresh_devices(),
            _ => {}
        }
    }
}

/// Entry point for the `tune` subcommand.
pub fn run() -> i32 {
    let mut client = match Client::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wayland-mouse tune: can't reach the daemon at {SOCKET_PATH}: {e}");
            eprintln!("Is the service running, and are you root?");
            eprintln!("  sudo systemctl start wayland-mouse  &&  sudo wayland-mouse tune");
            return 1;
        }
    };
    let cfg = match client.get_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wayland-mouse tune: {e}");
            return 1;
        }
    };
    let devices = client.devices().unwrap_or_default();

    let mut terminal = ratatui::init();
    let mut app = App::new(client, cfg, devices);
    app.import_legacy_overrides();
    let res = app.run(&mut terminal);
    ratatui::restore();
    match res {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("wayland-mouse tune: {e}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .split(f.area());

    render_header(f, app, chunks[0]);
    let cols = Layout::horizontal([
        Constraint::Length(if chunks[1].width >= 100 { 36 } else { 26 }),
        Constraint::Min(0),
    ])
    .split(chunks[1]);
    render_sidebar(f, app, cols[0]);
    render_main(f, app, cols[1]);
    render_footer(f, app, chunks[2]);
    if !matches!(app.modal, Modal::None) {
        render_modal(f, app);
    }
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let profile = app.selected_profile().unwrap_or_default();
    let kind = if app.cfg.presets.contains_key(&profile) {
        "custom"
    } else {
        "built-in"
    };
    let dirty = if app.dirty() {
        "  ●unsaved"
    } else {
        "  ✓saved"
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("Profile: ", Style::new().fg(Color::Gray)),
            Span::styled(profile, Style::new().fg(Color::Magenta).bold()),
            Span::styled(format!(" ({kind})"), Style::new().fg(Color::Gray)),
            Span::styled(
                dirty,
                Style::new().fg(if app.dirty() { MARKER } else { Color::Green }),
            ),
        ]),
        Line::from(vec![
            Span::styled("Target:  ", Style::new().fg(Color::Gray)),
            Span::styled(app.target_label(), Style::new().fg(ACCENT).bold()),
            Span::styled(
                "   (Tab changes pane · a assigns profile)",
                Style::new().fg(Color::DarkGray),
            ),
        ]),
    ];
    let block = Block::bordered().border_type(BorderType::Rounded).title(
        Span::from(" wayland-mouse · live profiles ")
            .fg(ACCENT)
            .bold(),
    );
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    lines.push(Line::from("PROFILES".fg(ACCENT).bold()));
    for (i, name) in app.profiles().iter().enumerate() {
        let selected = i == app.profile_sel;
        let focused = selected && app.focus == Focus::Profiles;
        let marker = if selected { "▶ " } else { "  " };
        let kind = if app.cfg.presets.contains_key(name) {
            ""
        } else {
            " · built-in"
        };
        let style = if focused {
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else if selected {
            Style::new().fg(Color::White)
        } else {
            Style::new().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::styled(name.clone(), style),
            Span::styled(kind, Style::new().fg(Color::DarkGray)),
        ]));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from("DEVICES".fg(ACCENT).bold()));
    for (i, row) in app.device_rows().iter().enumerate() {
        let selected = i == app.device_sel;
        let focused = selected && app.focus == Focus::Devices;
        let marker = if selected { "▶ " } else { "  " };
        let (label, preset) = match row {
            DeviceRow::Global => ("Global fallback".to_string(), app.cfg.preset.clone()),
            DeviceRow::Discovered { info, .. } => {
                (info.name.clone(), app.effective_preset_for(row))
            }
            DeviceRow::RuleOnly(j) => {
                let r = &app.cfg.device[*j];
                (r.match_.clone(), app.effective_preset_for(row))
            }
        };
        let style = if focused {
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else if selected {
            Style::new().fg(Color::White)
        } else {
            Style::new().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::styled(label, style),
        ]));
        let lock = app.lock_label(row);
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(format!("→ {preset}"), Style::new().fg(Color::DarkGray)),
            Span::styled(lock, Style::new().fg(Color::DarkGray)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(
        "n new · u update · d delete · a assign · t lock".fg(Color::DarkGray),
    ));

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::from(" Profiles & devices ").fg(ACCENT).bold());
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn render_main(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(area);
    render_tabs(f, app, chunks[0]);
    render_curve_tab(f, app, chunks[1], app.tab == Tab::Pointer);
}

fn render_tabs(f: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| Line::from(format!(" {} {} ", i + 1, t.title())))
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.tab.index())
        .highlight_style(
            Style::new()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::bordered().border_type(BorderType::Rounded));
    f.render_widget(tabs, area);
}

fn render_curve_tab(f: &mut Frame, app: &App, area: Rect, pointer: bool) {
    let cols =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).split(area);
    render_knobs(f, app, cols[0]);

    let right = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(cols[1]);
    render_chart(f, app, right[0], pointer);
    render_readout(f, app, right[1], pointer);
}

fn render_knobs(f: &mut Frame, app: &App, area: Rect) {
    let settings = app.selected_settings();
    let bar_w = (area.width as usize).saturating_sub(34).clamp(6, 28);
    let mut lines = Vec::new();
    for (i, field) in app.knob_fields().iter().enumerate() {
        let selected = i == app.sel && app.focus == Focus::Knobs;
        lines.push(knob_line(&settings, *field, selected, bar_w));
    }
    let profile = app.selected_profile().unwrap_or_default();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::from(format!(" Knobs · {profile} ")).fg(ACCENT).bold());
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn knob_line<'a>(s: &Settings, field: Field, selected: bool, bar_w: usize) -> Line<'a> {
    let marker = if selected { "▶ " } else { "  " };
    let base = if selected {
        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let label = Span::styled(format!("{marker}{:<18}", field.label()), base);
    if field.is_bool() {
        let on = match field {
            Field::PtrEnabled => s.pointer_accel,
            Field::WheelEnabled => s.wheel_enabled,
            _ => false,
        };
        let (txt, col) = if on {
            ("ON ", Color::Green)
        } else {
            ("OFF", BAR_EMPTY)
        };
        return Line::from(vec![label, Span::styled(txt, Style::new().fg(col).bold())]);
    }
    let v = match field {
        Field::Precision => s.ptr_base,
        Field::MaxGain => s.ptr_max,
        Field::Midpoint => s.ptr_mid,
        Field::Width => s.ptr_width,
        Field::PtrSmoothing => s.ptr_tau * 1000.0,
        Field::StartSpeed => s.threshold_dps,
        Field::Strength => s.accel,
        Field::Curve => s.exponent,
        Field::MinMult => s.min_mult,
        Field::MaxMult => s.max_mult,
        Field::StepStart => s.step_start,
        Field::SmoothUp => s.attack,
        Field::SmoothDown => s.release,
        Field::ResetMs => s.reset_gap.as_secs_f64() * 1000.0,
        Field::Dpi => s.dpi,
        Field::PtrEnabled | Field::WheelEnabled => 0.0,
    };
    let (lo, hi) = field.bounds();
    let frac = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
    let fill = (frac * bar_w as f64).round() as usize;
    let bar: String = "█".repeat(fill) + &"░".repeat(bar_w.saturating_sub(fill));
    let value = format!("{:>7.*}{}", field.decimals(), v, field.unit());
    Line::from(vec![
        label,
        Span::styled(format!("{value:>11}  "), Style::new().fg(Color::White)),
        Span::styled(bar, Style::new().fg(BAR_FILL)),
    ])
}

fn render_chart(f: &mut Frame, app: &App, area: Rect, pointer: bool) {
    let s = app.selected_settings();
    let (curve, marker, xmax, ymax, xlabel, ylabel, enabled) = if pointer {
        let enabled = s.pointer_accel;
        let k = (s.dpi / REFERENCE_DPI).max(0.0001);
        let mx = app.tel.pointer_speed / k;
        let my = app.tel.pointer_gain * k;
        let xmax = (s.ptr_mid * 2.0).max(2000.0);
        let ymax = s.ptr_max.max(1.0) * 1.15;
        let curve = if enabled {
            sample(xmax, |x| pointer_gain(&s, x))
        } else {
            sample(xmax, |_| 1.0)
        };
        (
            curve,
            (mx, my),
            xmax,
            ymax,
            "mouse speed →",
            "gain ×",
            enabled,
        )
    } else {
        let enabled = s.wheel_enabled;
        let mx = app.tel.wheel_dps;
        let my = app.tel.wheel_mult;
        let xmax = (s.threshold_dps * 2.0).max(50.0);
        let ymax = (s.max_mult * 1.08).max(1.5);
        let curve = if enabled {
            sample(xmax, |x| wheel_mult(&s, x))
        } else {
            sample(xmax, |_| 1.0)
        };
        (
            curve,
            (mx, my),
            xmax,
            ymax,
            "scroll speed →",
            "× mult",
            enabled,
        )
    };
    let marker_pt = [(marker.0.clamp(0.0, xmax), marker.1.clamp(0.0, ymax))];

    let (curve_color, title) = if enabled {
        (CURVE, Span::from(" curve ").fg(ACCENT).bold())
    } else {
        (
            BAR_EMPTY,
            Span::from(" curve — ACCEL OFF (1:1, press Space to enable) ")
                .fg(MARKER)
                .bold(),
        )
    };

    let datasets = vec![
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(curve_color))
            .data(&curve),
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Scatter)
            .style(Style::new().fg(MARKER).add_modifier(Modifier::BOLD))
            .data(&marker_pt),
    ];

    let chart = Chart::new(datasets)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(title),
        )
        .x_axis(
            Axis::default()
                .title(xlabel)
                .style(Style::new().fg(Color::Gray))
                .bounds([0.0, xmax])
                .labels(vec![Span::raw("0"), Span::raw(format!("{xmax:.0}"))]),
        )
        .y_axis(
            Axis::default()
                .title(ylabel)
                .style(Style::new().fg(Color::Gray))
                .bounds([0.0, ymax])
                .labels(vec![Span::raw("0"), Span::raw(format!("{ymax:.1}"))]),
        );
    f.render_widget(chart, area);
}

fn sample(xmax: f64, g: impl Fn(f64) -> f64) -> Vec<(f64, f64)> {
    const N: usize = 80;
    (0..=N)
        .map(|i| {
            let x = xmax * i as f64 / N as f64;
            (x, g(x))
        })
        .collect()
}

fn render_readout(f: &mut Frame, app: &App, area: Rect, pointer: bool) {
    let line = if pointer {
        Line::from(vec![
            Span::raw("live  "),
            Span::styled("speed ", Style::new().fg(Color::Gray)),
            Span::styled(
                format!("{:>6.0}", app.tel.pointer_speed),
                Style::new().fg(MARKER).bold(),
            ),
            Span::styled("  gain ", Style::new().fg(Color::Gray)),
            Span::styled(
                format!("{:>4.2}×", app.tel.pointer_gain),
                Style::new().fg(MARKER).bold(),
            ),
        ])
    } else {
        Line::from(vec![
            Span::raw("live  "),
            Span::styled("scroll ", Style::new().fg(Color::Gray)),
            Span::styled(
                format!("{:>5.1}", app.tel.wheel_dps),
                Style::new().fg(MARKER).bold(),
            ),
            Span::styled(" det/s  mult ", Style::new().fg(Color::Gray)),
            Span::styled(
                format!("{:>4.2}×", app.tel.wheel_mult),
                Style::new().fg(MARKER).bold(),
            ),
        ])
    };
    let block = Block::bordered().border_type(BorderType::Rounded);
    f.render_widget(Paragraph::new(line).block(block), area);
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);

    let key = |k: &'static str| Span::styled(k, Style::new().fg(KEY).bold());
    let keys = Line::from(vec![
        key("Tab"),
        Span::raw(" pane  "),
        key("1/2"),
        Span::raw(" curve  "),
        key("↑↓"),
        Span::raw(" move  "),
        key("←→"),
        Span::raw(" adjust  "),
        key("⎵"),
        Span::raw(" toggle  "),
        key("n"),
        Span::raw(" new  "),
        key("a"),
        Span::raw(" assign  "),
        key("t"),
        Span::raw(" lock  "),
        key("s"),
        Span::raw(" save  "),
        key("q"),
        Span::raw(" quit"),
    ]);
    f.render_widget(Paragraph::new(keys), rows[0]);

    let dirty = if app.dirty() {
        Span::styled("●unsaved", Style::new().fg(MARKER).bold())
    } else {
        Span::styled("✓saved", Style::new().fg(Color::Green))
    };
    let status = Line::from(vec![
        Span::styled(
            format!("profile {}  ", app.selected_profile().unwrap_or_default()),
            Style::new().fg(Color::Magenta),
        ),
        dirty,
        Span::raw("   "),
        Span::styled(app.status.clone(), Style::new().fg(Color::Gray)),
    ]);
    f.render_widget(Paragraph::new(status).alignment(Alignment::Left), rows[1]);
}

fn render_modal(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 40, f.area());
    let (title, lines) = match &app.modal {
        Modal::NewProfile { input } => (
            " New profile ",
            vec![
                Line::from("Name the profile (snapshot of current settings):".fg(Color::Gray)),
                Line::raw(""),
                Line::from(vec![
                    Span::raw("  > "),
                    Span::styled(format!("{input}▌"), Style::new().fg(ACCENT).bold()),
                ]),
                Line::raw(""),
                Line::from("Enter to create · Esc to cancel".fg(Color::Gray)),
            ],
        ),
        Modal::DeleteProfile { name, refs } => {
            let mut lines = vec![
                Line::from(vec![
                    Span::raw("Delete profile "),
                    Span::styled(name.clone(), Style::new().fg(MARKER).bold()),
                    Span::raw("?"),
                ]),
                Line::raw(""),
            ];
            if refs.is_empty() {
                lines.push(Line::from(
                    "It is not referenced by any device.".fg(Color::Gray),
                ));
            } else {
                lines.push(Line::from(
                    "References will be reassigned to mac-like:".fg(Color::Gray),
                ));
                for r in refs {
                    lines.push(Line::from(format!("  · {r}").fg(Color::Yellow)));
                }
            }
            lines.push(Line::raw(""));
            lines.push(Line::from(
                "Enter to delete · Esc to cancel".fg(Color::Gray),
            ));
            (" Delete profile ", lines)
        }
        Modal::None => return,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::from(title).fg(ACCENT).bold());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

/// A centered rectangle `px`×`py` percent of `area`.
fn centered_rect(px: u16, py: u16, area: Rect) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - py) / 2),
        Constraint::Percentage(py),
        Constraint::Percentage((100 - py) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - px) / 2),
        Constraint::Percentage(px),
        Constraint::Percentage((100 - px) / 2),
    ])
    .split(v[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_client() -> Client {
        let (a, _b) = UnixStream::pair().unwrap();
        let reader = BufReader::new(a.try_clone().unwrap());
        Client { writer: a, reader }
    }

    fn app() -> App {
        App::new(dummy_client(), ConfigFile::default(), Vec::new())
    }

    #[test]
    fn field_reads_preset_defaults() {
        let p = PresetDef::from_settings(&ConfigFile::default().resolve_config_space(""));
        assert!((preset_value(&p, Field::MaxGain) - 2.5).abs() < 1e-9);
        assert!(preset_value(&p, Field::PtrEnabled) > 0.5);
    }

    #[test]
    fn adjust_writes_override_and_snaps() {
        let mut p = PresetDef::from_settings(&ConfigFile::default().resolve_config_space(""));
        preset_adjust(&mut p, Field::MaxGain, 1.0);
        assert_eq!(p.pointer.max_gain, Some(2.6));
        preset_adjust(&mut p, Field::MaxGain, -1.0);
        assert!((p.pointer.max_gain.unwrap() - 2.5).abs() < 1e-9);
    }

    #[test]
    fn new_profile_is_created_and_assigned() {
        let mut app = app();
        app.create_profile("mine");
        assert!(app.cfg.presets.contains_key("mine"));
        assert_eq!(app.cfg.preset, "mine");
        assert_eq!(app.selected_profile().as_deref(), Some("mine"));
    }

    #[test]
    fn assign_creates_device_rule() {
        let mut app = app();
        app.devices = vec![DeviceInfo {
            name: "Acer BT Keyboard1 Mouse".into(),
            path: "/dev/input/event16".into(),
            grabbed: true,
            has_wheel: true,
            has_pointer: true,
            has_left: true,
        }];
        app.device_sel = 1; // Global is index 0
        app.sync_profile_to_device();
        app.assign_profile();
        assert_eq!(app.cfg.device.len(), 1);
        assert_eq!(app.cfg.device[0].match_, "Acer BT Keyboard1 Mouse");
    }

    #[test]
    fn delete_reassigns_references() {
        let mut app = app();
        app.create_profile("mine");
        app.start_delete_profile();
        app.delete_profile("mine");
        assert!(!app.cfg.presets.contains_key("mine"));
        assert_eq!(app.cfg.preset, "mac-like");
    }

    #[test]
    fn imports_legacy_global_overrides() {
        let mut app = app();
        app.cfg.wheel.strength = Some(0.5);
        app.import_legacy_overrides();
        assert!(app.cfg.presets.contains_key("mac-like custom"));
        assert_eq!(app.cfg.preset, "mac-like custom");
        assert!(app.cfg.wheel.is_empty());
    }

    #[test]
    fn dirty_clears_when_edit_is_reverted() {
        let mut app = app();
        assert!(!app.dirty());
        app.cfg.pointer.max_gain = Some(3.0);
        assert!(app.dirty());
        app.cfg.pointer.max_gain = None;
        assert!(!app.dirty());
    }

    #[test]
    fn renders_every_pane_and_modal_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = app();
        app.devices = vec![DeviceInfo {
            name: "Acer BT Keyboard1 Mouse".into(),
            path: "/dev/input/event16".into(),
            grabbed: true,
            has_wheel: true,
            has_pointer: true,
            has_left: true,
        }];
        app.tel = TelemetrySample {
            pointer_speed: 1500.0,
            pointer_gain: 1.8,
            wheel_dps: 10.0,
            wheel_mult: 2.5,
        };
        let mut term = Terminal::new(TestBackend::new(140, 40)).unwrap();
        for _ in 0..3 {
            term.draw(|f| ui(f, &app)).unwrap();
            app.focus_next();
        }
        app.tab = Tab::Wheel;
        term.draw(|f| ui(f, &app)).unwrap();
        app.modal = Modal::NewProfile {
            input: "test".into(),
        };
        term.draw(|f| ui(f, &app)).unwrap();
        app.modal = Modal::DeleteProfile {
            name: "test".into(),
            refs: vec!["global".into()],
        };
        term.draw(|f| ui(f, &app)).unwrap();
        app.modal = Modal::None;
        app.cfg.pointer.enabled = Some(false);
        app.cfg.wheel.enabled = Some(false);
        term.draw(|f| ui(f, &app)).unwrap();
        let mut tiny = Terminal::new(TestBackend::new(20, 8)).unwrap();
        tiny.draw(|f| ui(f, &app)).unwrap();
    }
}
