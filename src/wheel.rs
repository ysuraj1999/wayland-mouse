//! Wheel acceleration: a smoothed, velocity-dependent multiplier on the
//! high-resolution scroll stream. Slow scrolling stays 1:1; faster scrolling
//! ramps up to `max_multiplier`.

use std::time::SystemTime;

use evdev::{EventType, InputEvent, RelativeAxisCode as RelativeAxisType};

use crate::config::Settings;
use crate::util::dt_secs;

/// Kernel convention: 120 high-resolution units == one wheel detent ("click").
pub const HIRES_PER_DETENT: f64 = 120.0;

/// Per-axis smoothing state (one for vertical, one for horizontal).
pub struct Axis {
    last: Option<SystemTime>,
    smoothed: f64, // detents/sec
    carry: f64,
    last_mult: f64,
}

impl Axis {
    pub fn new() -> Self {
        Axis {
            last: None,
            smoothed: 0.0,
            carry: 0.0,
            last_mult: 1.0,
        }
    }
    /// Latest smoothed wheel speed (detents/sec), for telemetry.
    pub fn dps(&self) -> f64 {
        self.smoothed
    }
    /// Latest applied multiplier, for telemetry.
    pub fn mult(&self) -> f64 {
        self.last_mult
    }
}

fn mult_for_speed(c: &Settings, dps: f64) -> f64 {
    let over = dps - c.threshold_dps;
    if over <= 0.0 {
        return 1.0;
    }
    (1.0 + c.accel * over.powf(c.exponent)).min(c.max_mult)
}

/// EV_SYN / SYN_REPORT frame delimiter.
fn syn() -> InputEvent {
    InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0)
}

/// Emit `outv` hi-res units as whole-detent notches, each closed by its own
/// SYN_REPORT, so apps that scroll a fixed amount *per wheel event* rather than
/// by delta magnitude (Flutter/Electron, e.g. the Ubuntu App Center) see the
/// acceleration as more steps. Magnitude-summing apps (Chrome/Firefox) land on
/// the same total distance either way. Each full detent carries the matching
/// low-res `REL_WHEEL`/`REL_HWHEEL` tick too, mirroring real hi-res hardware. A
/// sub-detent remainder is left unsynced; the caller's frame SYN closes it.
fn emit_notches(
    out: &mut Vec<InputEvent>,
    hi: RelativeAxisType,
    lo: RelativeAxisType,
    outv: i32,
) {
    let step = HIRES_PER_DETENT as i32;
    let sign = if outv < 0 { -1 } else { 1 };
    let mut mag = outv.abs();
    while mag >= step {
        out.push(InputEvent::new(EventType::RELATIVE.0, hi.0, sign * step));
        out.push(InputEvent::new(EventType::RELATIVE.0, lo.0, sign));
        out.push(syn());
        mag -= step;
    }
    if mag != 0 {
        out.push(InputEvent::new(EventType::RELATIVE.0, hi.0, sign * mag));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HI: RelativeAxisType = RelativeAxisType(0x0b); // REL_WHEEL_HI_RES
    const LO: RelativeAxisType = RelativeAxisType(0x08); // REL_WHEEL

    fn split(outv: i32) -> Vec<InputEvent> {
        let mut out = Vec::new();
        emit_notches(&mut out, HI, LO, outv);
        out
    }

    // The emitted hi-res deltas must sum back to exactly the accelerated value,
    // so a magnitude-summing app (Chrome/Firefox) scrolls the same distance.
    #[test]
    fn notches_preserve_total_magnitude() {
        for outv in [-909, -840, -120, -69, 1, 120, 245, 960] {
            let sum: i32 = split(outv)
                .iter()
                .filter(|e| e.event_type().0 == EventType::RELATIVE.0 && e.code() == HI.0)
                .map(|e| e.value())
                .sum();
            assert_eq!(sum, outv, "hi-res sum mismatch for outv={outv}");
        }
    }

    // One SYN frame + one low-res tick per whole detent, so a fixed-step app
    // (Flutter/Electron) sees |outv|/120 discrete scroll events.
    #[test]
    fn notches_emit_one_frame_per_detent() {
        let out = split(-909); // 7 full detents (840) + 69 remainder
        let syns = out
            .iter()
            .filter(|e| e.event_type().0 == EventType::SYNCHRONIZATION.0)
            .count();
        let ticks: i32 = out
            .iter()
            .filter(|e| e.event_type().0 == EventType::RELATIVE.0 && e.code() == LO.0)
            .map(|e| e.value())
            .sum();
        assert_eq!(syns, 7);
        assert_eq!(ticks, -7); // signed like the scroll direction
    }

    // A pure sub-detent delta carries no SYN of its own (the caller's frame SYN
    // closes it) and no low-res tick.
    #[test]
    fn sub_detent_remainder_has_no_frame() {
        let out = split(-69);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code(), HI.0);
        assert_eq!(out[0].value(), -69);
    }
}

/// Accelerate one hi-res wheel event and push the result onto `out`.
#[allow(clippy::too_many_arguments)]
pub fn scroll(
    c: &Settings,
    ax: &mut Axis,
    hires_in: i32,
    ts: SystemTime,
    out_code: RelativeAxisType,
    lo_code: RelativeAxisType,
    out: &mut Vec<InputEvent>,
    label: char,
) {
    let dt = dt_secs(ax.last, ts);
    ax.last = Some(ts);

    let detents = (hires_in.abs() as f64) / HIRES_PER_DETENT;
    let inst = if dt > c.reset_gap.as_secs_f64() {
        ax.smoothed = 0.0;
        0.0
    } else if dt <= 0.0 {
        ax.smoothed
    } else {
        detents / dt
    };
    let a = if inst > ax.smoothed {
        c.attack
    } else {
        c.release
    };
    ax.smoothed += a * (inst - ax.smoothed);

    let mult = mult_for_speed(c, ax.smoothed);
    ax.last_mult = mult;
    ax.carry += (hires_in as f64) * mult;
    let outv = ax.carry.trunc() as i32;
    ax.carry -= outv as f64;

    if c.debug {
        let steps = if c.wheel_discrete_steps {
            outv.abs() / HIRES_PER_DETENT as i32
        } else {
            0
        };
        eprintln!(
            "{label} in={hires_in:+5} dps={:6.1} mult={mult:4.2} out={outv:+6} steps={steps}",
            ax.smoothed
        );
    }
    if outv == 0 {
        return;
    }
    if c.wheel_discrete_steps {
        emit_notches(out, out_code, lo_code, outv);
    } else {
        out.push(InputEvent::new(EventType::RELATIVE.0, out_code.0, outv));
    }
}
