//! Device handling: discover wheel mice, grab each one, and re-emit its events
//! through a per-device virtual uinput device with acceleration applied.

use std::collections::HashSet;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// evdev 0.13 renamed Key -> KeyCode and RelativeAxisType -> RelativeAxisCode;
// we alias them back to the old names to keep the code below unchanged.
use evdev::uinput::VirtualDevice;
use evdev::{
    AttributeSet, Device, EventType, InputEvent, KeyCode as Key,
    RelativeAxisCode as RelativeAxisType,
};

use crate::clicklock::ClickLock;
use crate::ipc::{DeviceInfo, Shared};
use crate::pointer::{accel_pointer, Pointer};
use crate::wheel::{scroll, Axis};

/// Virtual-device name prefix for devices we create. `is_target_mouse` skips
/// anything carrying this so we never grab our own output.
pub const VIRT_PREFIX: &str = "wayland-mouse";

const REL_WHEEL_HI: RelativeAxisType = RelativeAxisType(0x0b);
const REL_HWHEEL_HI: RelativeAxisType = RelativeAxisType(0x0c);

/// Daemon entry point: enumerate, grab new wheel mice, and watch for hotplug.
pub fn run(shared: Arc<Shared>) {
    let handled: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));

    // Control socket for the live-tuning UI.
    {
        let shared = shared.clone();
        thread::spawn(move || crate::ipc::serve(shared));
    }

    loop {
        let cfg = shared.current();
        let name_filter = cfg.name_filter.clone();
        let mut snapshot = Vec::new();

        for (path, dev) in evdev::enumerate() {
            let name = dev.name().unwrap_or("").to_string();
            let grabbed = handled.lock().map(|s| s.contains(&path)).unwrap_or(false);
            if !name.contains(VIRT_PREFIX) {
                let info = device_info(&path, &dev, grabbed);
                if info.has_wheel || info.has_pointer {
                    snapshot.push(info);
                }
            }

            {
                let s = handled.lock().unwrap();
                if s.contains(&path) {
                    continue;
                }
            }
            let click_lock = cfg.click_lock_for(&name);
            let managed = cfg.rule_for(&name).is_some();
            if is_target_mouse(&dev, &name_filter, click_lock, managed) {
                drop(dev);
                handled.lock().unwrap().insert(path.clone());
                let shared = shared.clone();
                let handled = handled.clone();
                let p = path.clone();
                thread::spawn(move || {
                    if let Err(e) = run_device(p.clone(), shared, handled.clone()) {
                        eprintln!("device {p:?} error: {e}");
                        if let Ok(mut s) = handled.lock() {
                            s.remove(&p);
                        }
                    }
                });
            }
        }

        shared.set_devices(snapshot);
        thread::sleep(Duration::from_secs(3));
    }
}

fn device_info(path: &Path, dev: &Device, grabbed: bool) -> DeviceInfo {
    let axes = dev.supported_relative_axes();
    let has_wheel =
        axes.is_some_and(|s| s.contains(RelativeAxisType::REL_WHEEL) || s.contains(REL_WHEEL_HI));
    let has_pointer = axes.is_some_and(|s| {
        s.contains(RelativeAxisType::REL_X) && s.contains(RelativeAxisType::REL_Y)
    });
    let has_left = dev
        .supported_keys()
        .is_some_and(|k| k.contains(Key::BTN_LEFT));
    DeviceInfo {
        name: dev.name().unwrap_or("mouse").to_string(),
        path: path.to_string_lossy().into_owned(),
        grabbed,
        has_wheel,
        has_pointer,
        has_left,
    }
}

/// A device worth grabbing: a wheel mouse, or a pointer node that has click
/// lock enabled for it (so a trackpad exposed as a plain mouse can use it too).
fn is_target_mouse(dev: &Device, filter: &str, click_lock: bool, managed: bool) -> bool {
    let name = dev.name().unwrap_or("");
    if name.contains(VIRT_PREFIX) {
        return false;
    }
    let axes = dev.supported_relative_axes();
    let has_wheel =
        axes.is_some_and(|s| s.contains(RelativeAxisType::REL_WHEEL) || s.contains(REL_WHEEL_HI));
    let has_pointer = axes.is_some_and(|s| {
        s.contains(RelativeAxisType::REL_X) && s.contains(RelativeAxisType::REL_Y)
    });
    let has_left = dev
        .supported_keys()
        .is_some_and(|k| k.contains(Key::BTN_LEFT));
    if !(has_wheel || ((click_lock || managed) && has_pointer && has_left)) {
        return false;
    }
    filter.is_empty() || name.to_lowercase().contains(&filter.to_lowercase())
}

fn run_device(
    path: PathBuf,
    shared: Arc<Shared>,
    handled: Arc<Mutex<HashSet<PathBuf>>>,
) -> io::Result<()> {
    let mut dev = Device::open(&path)?;
    let name = dev.name().unwrap_or("mouse").to_string();
    // Resolve this device's effective settings; re-resolved live when the
    // config version changes (see the loop below).
    let mut settings = shared.current().resolve(&name);
    let mut managed = shared.current().rule_for(&name).is_some();
    let mut version = shared.version();
    let has_hires = dev
        .supported_relative_axes()
        .is_some_and(|s| s.contains(REL_WHEEL_HI));
    let has_wheel = dev
        .supported_relative_axes()
        .is_some_and(|s| s.contains(RelativeAxisType::REL_WHEEL) || s.contains(REL_WHEEL_HI));

    let mut axes = AttributeSet::<RelativeAxisType>::new();
    if let Some(s) = dev.supported_relative_axes() {
        for a in s.iter() {
            axes.insert(a);
        }
    }
    for a in [
        RelativeAxisType::REL_X,
        RelativeAxisType::REL_Y,
        RelativeAxisType::REL_WHEEL,
        RelativeAxisType::REL_HWHEEL,
        REL_WHEEL_HI,
        REL_HWHEEL_HI,
    ] {
        axes.insert(a);
    }
    let mut keys = AttributeSet::<Key>::new();
    if let Some(s) = dev.supported_keys() {
        for k in s.iter() {
            keys.insert(k);
        }
    }

    let vname = format!("{VIRT_PREFIX} {name}");
    let mut vdev = VirtualDevice::builder()?
        .name(&vname)
        .with_relative_axes(&axes)?
        .with_keys(&keys)?
        .build()?;

    dev.grab()?;
    eprintln!(
        "handling {path:?}  {name:?}  hi-res={has_hires}  wheel={}  pointer={}  click_lock={}",
        settings.wheel_enabled, settings.pointer_accel, settings.click_lock
    );

    let mut vy = Axis::new();
    let mut hx = Axis::new();
    let mut ptr = Pointer::new();
    let mut cl = ClickLock::from_settings(&settings);
    let mut fdx = 0i32; // accumulated frame motion
    let mut fdy = 0i32;
    let mut pa = settings.pointer_accel;
    let mut we = settings.wheel_enabled;
    let mut out: Vec<InputEvent> = Vec::with_capacity(64);

    loop {
        // Pick up live config edits pushed by the tuner.
        let v = shared.version();
        if v != version {
            settings = shared.current().resolve(&name);
            managed = shared.current().rule_for(&name).is_some();
            version = v;
            pa = settings.pointer_accel;
            we = settings.wheel_enabled;
            if cl.apply(&settings) {
                emit_left_release(&mut vdev)?;
            }
        }

        // A pointer-only node is grabbed solely for click lock or an explicit
        // device rule; if neither applies, release it.
        if !has_wheel && !settings.click_lock && !managed {
            break;
        }

        let timeout = cl.next_timeout(Instant::now());
        let ms = timeout
            .map(|d| d.as_millis().min(i32::MAX as u128) as i32)
            .unwrap_or(-1);
        let mut pfd = libc::pollfd {
            fd: dev.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if rc == 0 {
            if cl.on_timeout(Instant::now()) {
                emit_left_release(&mut vdev)?;
            }
            continue;
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            break;
        }
        if (pfd.revents & libc::POLLIN) == 0 {
            continue;
        }

        let events = match dev.fetch_events() {
            Ok(e) => e,
            Err(_) => break,
        };
        out.clear();
        for ev in events {
            let et = ev.event_type();
            if et == EventType::RELATIVE {
                let code = ev.code();
                if (code == RelativeAxisType::REL_X.0 || code == RelativeAxisType::REL_Y.0)
                    && ev.value() != 0
                {
                    cl.on_motion(Instant::now());
                }
                if code == REL_WHEEL_HI.0 {
                    if we {
                        scroll(
                            &settings,
                            &mut vy,
                            ev.value(),
                            ev.timestamp(),
                            REL_WHEEL_HI,
                            RelativeAxisType::REL_WHEEL,
                            &mut out,
                            'V',
                        );
                    } else {
                        out.push(InputEvent::new(et.0, code, ev.value()));
                    }
                } else if code == REL_HWHEEL_HI.0 {
                    if we {
                        scroll(
                            &settings,
                            &mut hx,
                            ev.value(),
                            ev.timestamp(),
                            REL_HWHEEL_HI,
                            RelativeAxisType::REL_HWHEEL,
                            &mut out,
                            'H',
                        );
                    } else {
                        out.push(InputEvent::new(et.0, code, ev.value()));
                    }
                } else if code == RelativeAxisType::REL_WHEEL.0 {
                    // Coarse wheel: only when there's no hi-res stream to carry it.
                    if !we {
                        out.push(InputEvent::new(et.0, code, ev.value()));
                    } else if !has_hires {
                        scroll(
                            &settings,
                            &mut vy,
                            ev.value() * 120,
                            ev.timestamp(),
                            REL_WHEEL_HI,
                            RelativeAxisType::REL_WHEEL,
                            &mut out,
                            'V',
                        );
                    }
                } else if code == RelativeAxisType::REL_HWHEEL.0 {
                    if !we {
                        out.push(InputEvent::new(et.0, code, ev.value()));
                    } else if !has_hires {
                        scroll(
                            &settings,
                            &mut hx,
                            ev.value() * 120,
                            ev.timestamp(),
                            REL_HWHEEL_HI,
                            RelativeAxisType::REL_HWHEEL,
                            &mut out,
                            'H',
                        );
                    }
                } else if code == RelativeAxisType::REL_X.0 {
                    if pa {
                        fdx += ev.value();
                    } else {
                        out.push(InputEvent::new(EventType::RELATIVE.0, code, ev.value()));
                    }
                } else if code == RelativeAxisType::REL_Y.0 {
                    if pa {
                        fdy += ev.value();
                    } else {
                        out.push(InputEvent::new(EventType::RELATIVE.0, code, ev.value()));
                    }
                } else {
                    out.push(InputEvent::new(EventType::RELATIVE.0, code, ev.value()));
                }
            } else if et == EventType::KEY {
                let code = ev.code();
                if code == Key::BTN_LEFT.code() && cl.enabled() {
                    if cl.on_left(ev.value(), Instant::now()) {
                        out.push(InputEvent::new(et.0, code, ev.value()));
                    }
                } else {
                    out.push(InputEvent::new(et.0, code, ev.value()));
                }
            } else if et == EventType::SYNCHRONIZATION && ev.code() == 0 {
                // SYN_REPORT: accelerate this frame's motion, then emit the SYN.
                if pa && (fdx != 0 || fdy != 0) {
                    accel_pointer(&settings, &mut ptr, fdx, fdy, ev.timestamp(), &mut out);
                    fdx = 0;
                    fdy = 0;
                }
                out.push(InputEvent::new(et.0, ev.code(), ev.value()));
            } else {
                out.push(InputEvent::new(et.0, ev.code(), ev.value()));
            }
        }
        if !out.is_empty() && vdev.emit(&out).is_err() {
            break;
        }
        if cl.on_timeout(Instant::now()) {
            emit_left_release(&mut vdev)?;
        }
        // Publish the latest measured speeds/gains for the tuner's live markers.
        shared.telemetry.set_pointer(ptr.speed(), ptr.gain());
        shared.telemetry.set_wheel(vy.dps(), vy.mult());
    }

    if cl.is_locked() {
        let _ = emit_left_release(&mut vdev);
    }
    let _ = dev.ungrab();
    if let Ok(mut s) = handled.lock() {
        s.remove(&path);
    }
    eprintln!("released {path:?}");
    Ok(())
}

fn emit_left_release(vdev: &mut VirtualDevice) -> io::Result<()> {
    vdev.emit(&[InputEvent::new(EventType::KEY.0, Key::BTN_LEFT.code(), 0)])
}
