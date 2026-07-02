//! JoyClicks — merge a physical gamepad (left hand) and a mouse (right hand) into a single
//! virtual gamepad via uinput, so an emulator that only accepts one input device per pad
//! (e.g. RPCS3) gets controller movement + mouse aim at the same time.
//!
//! The mouse is relative (velocity); a stick is absolute (position). JoyClicks integrates
//! mouse motion into a right-stick deflection with a per-tick spring-back toward center, so
//! turn rate ends up ~proportional to mouse speed and recenters when you stop moving.

mod config;

use anyhow::{anyhow, Context, Result};
use config::{Config, Control};
use evdev::{
    uinput::VirtualDevice,
    AbsInfo, AbsoluteAxisCode, AbsoluteAxisEvent, AttributeSet, Device, EventSummary, KeyCode,
    KeyEvent, RelativeAxisCode, UinputAbsSetup,
};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const STICK_MIN: i32 = 0;
const STICK_MAX: i32 = 255;
const STICK_CENTER: i32 = 128;
const TRIGGER_MAX: i32 = 255;
// The right stick (mouse aim) uses a 16-bit symmetric range for fine granularity — a mouse
// deserves far more resolution than a physical stick's ~8 bits, else slow aim feels steppy.
const RSTICK_MAX: i32 = 32767;

/// The full set of virtual-pad buttons we declare and drive. Used as the single source of
/// truth for: the uinput key declaration, per-tick releases, and which DS4 keys we forward.
const DECLARED_KEYS: [KeyCode; 10] = [
    KeyCode::BTN_SOUTH,
    KeyCode::BTN_EAST,
    KeyCode::BTN_NORTH,
    KeyCode::BTN_WEST,
    KeyCode::BTN_TL,
    KeyCode::BTN_TR,
    KeyCode::BTN_SELECT,
    KeyCode::BTN_START,
    KeyCode::BTN_THUMBL,
    KeyCode::BTN_THUMBR,
];

/// Where a mapped mouse input lands on the virtual pad.
enum Target {
    Key(KeyCode),
    /// Analog trigger pressed to full: true = right trigger (R2), false = left trigger (L2).
    Trigger(bool),
    /// A hat axis override: (is_x_axis, value in {-1,0,1}).
    Hat(bool, i32),
}

fn control_target(c: Control) -> Option<Target> {
    Some(match c {
        Control::None => return None,
        Control::R2 => Target::Trigger(true),
        Control::L2 => Target::Trigger(false),
        Control::R1 => Target::Key(KeyCode::BTN_TR),
        Control::L1 => Target::Key(KeyCode::BTN_TL),
        Control::Cross => Target::Key(KeyCode::BTN_SOUTH),
        Control::Circle => Target::Key(KeyCode::BTN_EAST),
        Control::Square => Target::Key(KeyCode::BTN_WEST),
        Control::Triangle => Target::Key(KeyCode::BTN_NORTH),
        Control::L3 => Target::Key(KeyCode::BTN_THUMBL),
        Control::R3 => Target::Key(KeyCode::BTN_THUMBR),
        Control::DpadUp => Target::Hat(false, -1),
        Control::DpadDown => Target::Hat(false, 1),
        Control::DpadLeft => Target::Hat(true, -1),
        Control::DpadRight => Target::Hat(true, 1),
    })
}

/// Messages from the reader threads to the main integration loop.
enum Msg {
    MouseRel(i32, i32),
    MouseButton(KeyCode, bool),
    MouseWheel(i32), // +1 = up, -1 = down
    PadAbs(AbsoluteAxisCode, i32),
    PadKey(KeyCode, i32),
    TogglePs, // PS button pressed -> toggle grab/active
}

fn find_device(name_substr: &str, want_pad: bool) -> Result<(Device, String)> {
    for (_path, dev) in evdev::enumerate() {
        let name = dev.name().unwrap_or("").to_string();
        if !name.contains(name_substr) {
            continue;
        }
        let ok = if want_pad {
            dev.supported_keys()
                .map_or(false, |k| k.contains(KeyCode::BTN_SOUTH))
        } else {
            dev.supported_relative_axes()
                .map_or(false, |r| r.contains(RelativeAxisCode::REL_X))
                && dev
                    .supported_keys()
                    .map_or(false, |k| k.contains(KeyCode::BTN_LEFT))
        };
        if ok {
            return Ok((dev, name));
        }
    }
    Err(anyhow!(
        "no {} matching name '{}' with the required capabilities was found",
        if want_pad { "gamepad" } else { "mouse" },
        name_substr
    ))
}

fn build_virtual_pad() -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for k in DECLARED_KEYS {
        keys.insert(k);
    }

    let stick = AbsInfo::new(STICK_CENTER, STICK_MIN, STICK_MAX, 0, 0, 0);
    let rstick = AbsInfo::new(0, -RSTICK_MAX, RSTICK_MAX, 0, 0, 0);
    let trigger = AbsInfo::new(0, 0, TRIGGER_MAX, 0, 0, 0);
    let hat = AbsInfo::new(0, -1, 1, 0, 0, 0);

    let vdev = VirtualDevice::builder()?
        .name("JoyClicks Virtual Gamepad")
        .with_keys(&keys)?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, stick))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, stick))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_RX, rstick))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_RY, rstick))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_Z, trigger))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_RZ, trigger))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_HAT0X, hat))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_HAT0Y, hat))?
        .build()?;
    Ok(vdev)
}

/// Reader thread: polls a grabbed device and forwards normalized messages to the main loop.
/// Mice ungrab/grab themselves to follow `active` (so the desktop cursor is freed when
/// paused); the pad stays grabbed for its whole lifetime.
fn reader_thread(
    mut dev: Device,
    is_mouse: bool,
    tx: Sender<Msg>,
    quit: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
) {
    let timeout = PollTimeout::try_from(4u16).expect("4ms is a valid poll timeout");
    let mut grabbed = true; // both devices are grabbed before the thread starts
    loop {
        if quit.load(Ordering::Relaxed) {
            break;
        }
        // Mice follow the active flag so pausing frees the cursor.
        if is_mouse {
            let want = active.load(Ordering::Relaxed);
            if want != grabbed {
                let _ = if want { dev.grab() } else { dev.ungrab() };
                grabbed = want;
            }
        }

        let raw = dev.as_raw_fd();
        let bfd = unsafe { BorrowedFd::borrow_raw(raw) };
        let mut fds = [PollFd::new(bfd, PollFlags::POLLIN)];
        match poll(&mut fds, timeout) {
            Ok(0) => continue, // timed out; loop to re-check quit/active
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                eprintln!("joyclicks: poll error on {} device: {e}", dev_kind(is_mouse));
                break;
            }
        }

        let events = match dev.fetch_events() {
            Ok(ev) => ev,
            Err(e) => {
                eprintln!("joyclicks: read error on {} device: {e}", dev_kind(is_mouse));
                break;
            }
        };
        let mut disconnected = false;
        for ev in events {
            let msg = match ev.destructure() {
                EventSummary::RelativeAxis(_, code, val) if is_mouse => match code {
                    RelativeAxisCode::REL_X => Some(Msg::MouseRel(val, 0)),
                    RelativeAxisCode::REL_Y => Some(Msg::MouseRel(0, val)),
                    RelativeAxisCode::REL_WHEEL if val != 0 => {
                        Some(Msg::MouseWheel(val.signum()))
                    }
                    _ => None,
                },
                EventSummary::Key(_, code, val) if is_mouse => {
                    Some(Msg::MouseButton(code, val != 0))
                }
                EventSummary::Key(_, code, val) => {
                    if code == KeyCode::BTN_MODE {
                        if val == 1 {
                            Some(Msg::TogglePs)
                        } else {
                            None
                        }
                    } else {
                        Some(Msg::PadKey(code, val))
                    }
                }
                EventSummary::AbsoluteAxis(_, code, val) => Some(Msg::PadAbs(code, val)),
                _ => None,
            };
            if let Some(m) = msg {
                if tx.send(m).is_err() {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            break;
        }
    }
    // Best-effort release on the way out (also happens automatically when the fd closes).
    let _ = dev.ungrab();
}

fn dev_kind(is_mouse: bool) -> &'static str {
    if is_mouse {
        "mouse"
    } else {
        "pad"
    }
}

/// Current desired virtual-pad state, rebuilt each tick and diffed against what was last sent.
#[derive(Default)]
struct Desired {
    keys: HashMap<u16, i32>,
    abs: HashMap<u16, i32>,
}

fn main() -> Result<()> {
    let cfg = Config::load_or_create().context("loading config")?;

    let (mut pad, pad_name) = find_device(&cfg.devices.pad_name, true)
        .context("locating the gamepad (set devices.pad_name in config.toml)")?;
    let (mut mouse, mouse_name) = find_device(&cfg.devices.mouse_name, false)
        .context("locating the mouse (set devices.mouse_name in config.toml)")?;
    println!("joyclicks: pad   = {pad_name}");
    println!("joyclicks: mouse = {mouse_name}");

    pad.grab()
        .context("grabbing the gamepad (is another instance running?)")?;
    mouse.grab().context("grabbing the mouse")?;

    let mut vdev = build_virtual_pad().context("creating the virtual uinput gamepad")?;
    println!(
        "joyclicks: virtual gamepad created — select it in RPCS3's Pad settings (SDL/evdev handler)."
    );
    println!("joyclicks: PS button toggles mouse grab (pause); Ctrl-C to quit.");

    let quit = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicBool::new(true));
    {
        let quit = quit.clone();
        ctrlc::set_handler(move || quit.store(true, Ordering::Relaxed))
            .context("installing Ctrl-C handler")?;
    }

    // Live-reloadable aim shaping: a watcher thread reloads config.toml when it changes and
    // swaps these values in, so tuning takes effect without restarting or reconnecting the pad.
    // Only the [aim] shaping is hot-reloaded; device names, tick_hz, and the [buttons] map are
    // read once at startup and need a restart to change.
    let aim = Arc::new(Mutex::new(cfg.aim));
    {
        let aim = aim.clone();
        let quit = quit.clone();
        thread::spawn(move || config_watcher(aim, quit));
    }

    let (tx, rx) = mpsc::channel::<Msg>();
    let pad_handle = {
        let (tx, quit, active) = (tx.clone(), quit.clone(), active.clone());
        thread::spawn(move || reader_thread(pad, false, tx, quit, active))
    };
    let mouse_handle = {
        let (tx, quit, active) = (tx.clone(), quit.clone(), active.clone());
        thread::spawn(move || reader_thread(mouse, true, tx, quit, active))
    };
    drop(tx); // only the threads keep senders now

    // --- integration state ---
    let tick = Duration::from_micros(1_000_000 / cfg.aim.tick_hz.max(1) as u64);
    let tap_ticks = (cfg.buttons.tap_ms as u64 * cfg.aim.tick_hz as u64 / 1000).max(1);
    // How long after the last mouse movement the low-end boost keeps applying (~60 ms).
    let boost_hold_ticks = (60u64 * cfg.aim.tick_hz as u64 / 1000).max(1);

    let mut accum_x = 0f32; // unconsumed mouse deltas
    let mut accum_y = 0f32;
    let mut rx_f = 0f32; // right-stick position, -1..1
    let mut ry_f = 0f32;

    // DS4-sourced state
    let mut ds4_lx = STICK_CENTER;
    let mut ds4_ly = STICK_CENTER;
    let mut ds4_lt = 0i32;
    let mut ds4_rt = 0i32;
    let mut ds4_hatx = 0i32;
    let mut ds4_haty = 0i32;
    let mut ds4_keys: HashMap<u16, i32> = HashMap::new();

    // mouse-sourced button state (Control as u8 -> pressed)
    let mut mouse_ctrls: HashMap<u8, bool> = HashMap::new();
    // pending wheel taps: (control, release_at_tick)
    let mut taps: Vec<(Control, u64)> = Vec::new();

    let mut last_sent = Desired::default();
    let mut last_tick = Instant::now();
    let mut tick_count: u64 = 0;
    let mut ticks_since_input: u64 = boost_hold_ticks; // start as "not moving"

    'main: loop {
        if quit.load(Ordering::Relaxed) {
            break;
        }

        // Drain messages until it's time for the next tick.
        loop {
            let now = Instant::now();
            let since = now.duration_since(last_tick);
            if since >= tick {
                break;
            }
            match rx.recv_timeout(tick - since) {
                Ok(msg) => apply_msg(
                    msg,
                    &cfg,
                    &active,
                    &mut accum_x,
                    &mut accum_y,
                    &mut rx_f,
                    &mut ry_f,
                    &mut ds4_lx,
                    &mut ds4_ly,
                    &mut ds4_lt,
                    &mut ds4_rt,
                    &mut ds4_hatx,
                    &mut ds4_haty,
                    &mut ds4_keys,
                    &mut mouse_ctrls,
                    &mut taps,
                    tick_count,
                    tap_ticks,
                ),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break 'main,
            }
        }

        last_tick = Instant::now();
        tick_count += 1;

        // Snapshot the (possibly hot-reloaded) aim shaping for this tick. Recover from a
        // poisoned lock rather than panicking — a panic in main would leave the reader threads
        // holding the physical devices grabbed (frozen desktop input).
        let aimv = *aim.lock().unwrap_or_else(|e| e.into_inner());

        // --- integrate mouse motion into the right stick ---
        let moved_this_tick = accum_x != 0.0 || accum_y != 0.0;
        rx_f += accum_x * aimv.sensitivity;
        let dy = if aimv.invert_y { -accum_y } else { accum_y };
        ry_f += dy * aimv.sensitivity * aimv.y_scale;
        accum_x = 0.0;
        accum_y = 0.0;
        rx_f *= aimv.decay;
        ry_f *= aimv.decay;
        let maxd = aimv.max_deflection.clamp(0.0, 1.0);
        rx_f = rx_f.clamp(-maxd, maxd);
        ry_f = ry_f.clamp(-maxd, maxd);

        // Track active movement for the low-end boost. Instead of a flat floor held for a fixed
        // window (which drifts then snaps when it disengages), ramp the boost DOWN across the
        // window: full boost while moving, fading to zero ~60 ms after the last movement. This
        // keeps flicker-immunity across sparse slow input while removing post-stop drift/snap.
        if moved_this_tick {
            ticks_since_input = 0;
        } else {
            ticks_since_input = ticks_since_input.saturating_add(1);
        }
        let hold_factor =
            (1.0 - ticks_since_input as f32 / boost_hold_ticks as f32).clamp(0.0, 1.0);
        let boost_now = aimv.low_end_boost * hold_factor;

        // expire wheel taps (>= so a tap lasts the full tap_ticks, incl. the tap_ticks==1 case)
        taps.retain(|(_, until)| *until >= tick_count);

        // If paused, hold the pad neutral and skip driving it from live input.
        let paused = !active.load(Ordering::Relaxed);

        let desired = if paused {
            neutral_state()
        } else {
            build_desired(
                rx_f, ry_f, aimv.deadzone, boost_now, ds4_lx, ds4_ly, ds4_lt, ds4_rt, ds4_hatx,
                ds4_haty, &ds4_keys, cfg_buttons(&cfg), &mouse_ctrls, &taps,
            )
        };

        emit_diff(&mut vdev, &last_sent, &desired);
        last_sent = desired;
    }

    // Shutdown: signal threads, wait, drop the virtual device (removes it from the system).
    quit.store(true, Ordering::Relaxed);
    let _ = pad_handle.join();
    let _ = mouse_handle.join();
    println!("\njoyclicks: stopped.");
    Ok(())
}

fn cfg_buttons(cfg: &Config) -> &config::Buttons {
    &cfg.buttons
}

/// Polls the config file's mtime once a second and hot-swaps the aim shaping when it changes,
/// so tuning takes effect live (device names / tick_hz still require a restart).
fn config_watcher(aim: Arc<Mutex<config::Aim>>, quit: Arc<AtomicBool>) {
    let path = match Config::path() {
        Ok(p) => p,
        Err(_) => return,
    };
    let mut last = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
    while !quit.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(1000));
        // If the file is missing/unreadable, keep the last-good aim (don't reset to defaults).
        // Only advance `last` after a successful reload, so a file caught mid-write is retried.
        let cur = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        if let Some(cur_mt) = cur {
            if Some(cur_mt) != last {
                match Config::load_or_create() {
                    Ok(newcfg) => {
                        *aim.lock().unwrap_or_else(|e| e.into_inner()) = newcfg.aim;
                        last = Some(cur_mt);
                        println!(
                            "joyclicks: config reloaded (sensitivity={}, y_scale={}, decay={})",
                            newcfg.aim.sensitivity, newcfg.aim.y_scale, newcfg.aim.decay
                        );
                    }
                    Err(e) => eprintln!("joyclicks: config reload failed: {e}"),
                }
            }
        }
    }
}

fn neutral_state() -> Desired {
    let mut d = Desired::default();
    d.abs.insert(AbsoluteAxisCode::ABS_X.0, STICK_CENTER);
    d.abs.insert(AbsoluteAxisCode::ABS_Y.0, STICK_CENTER);
    d.abs.insert(AbsoluteAxisCode::ABS_RX.0, 0); // right stick is 16-bit, center = 0
    d.abs.insert(AbsoluteAxisCode::ABS_RY.0, 0);
    d.abs.insert(AbsoluteAxisCode::ABS_Z.0, 0);
    d.abs.insert(AbsoluteAxisCode::ABS_RZ.0, 0);
    d.abs.insert(AbsoluteAxisCode::ABS_HAT0X.0, 0);
    d.abs.insert(AbsoluteAxisCode::ABS_HAT0Y.0, 0);
    // Release every button too, so nothing stays held while paused.
    for k in DECLARED_KEYS {
        d.keys.insert(k.0, 0);
    }
    d
}

#[allow(clippy::too_many_arguments)]
fn apply_msg(
    msg: Msg,
    cfg: &Config,
    active: &Arc<AtomicBool>,
    accum_x: &mut f32,
    accum_y: &mut f32,
    rx_f: &mut f32,
    ry_f: &mut f32,
    ds4_lx: &mut i32,
    ds4_ly: &mut i32,
    ds4_lt: &mut i32,
    ds4_rt: &mut i32,
    ds4_hatx: &mut i32,
    ds4_haty: &mut i32,
    ds4_keys: &mut HashMap<u16, i32>,
    mouse_ctrls: &mut HashMap<u8, bool>,
    taps: &mut Vec<(Control, u64)>,
    tick_count: u64,
    tap_ticks: u64,
) {
    match msg {
        Msg::MouseRel(dx, dy) => {
            *accum_x += dx as f32;
            *accum_y += dy as f32;
        }
        Msg::MouseButton(code, pressed) => {
            let ctrl = match code {
                KeyCode::BTN_LEFT => cfg.buttons.left_click,
                KeyCode::BTN_RIGHT => cfg.buttons.right_click,
                KeyCode::BTN_MIDDLE => cfg.buttons.middle_click,
                KeyCode::BTN_SIDE => cfg.buttons.side,
                KeyCode::BTN_EXTRA => cfg.buttons.extra,
                _ => Control::None,
            };
            if ctrl != Control::None {
                mouse_ctrls.insert(ctrl as u8, pressed);
            }
        }
        Msg::MouseWheel(dir) => {
            let ctrl = if dir > 0 {
                cfg.buttons.wheel_up
            } else {
                cfg.buttons.wheel_down
            };
            if ctrl != Control::None {
                taps.push((ctrl, tick_count + tap_ticks));
            }
        }
        Msg::PadAbs(code, val) => match code {
            AbsoluteAxisCode::ABS_X => *ds4_lx = val,
            AbsoluteAxisCode::ABS_Y => *ds4_ly = val,
            AbsoluteAxisCode::ABS_Z => *ds4_lt = val,
            AbsoluteAxisCode::ABS_RZ => *ds4_rt = val,
            AbsoluteAxisCode::ABS_HAT0X => *ds4_hatx = val,
            AbsoluteAxisCode::ABS_HAT0Y => *ds4_haty = val,
            // ABS_RX / ABS_RY (DS4 right stick) are intentionally ignored — the mouse owns aim.
            _ => {}
        },
        Msg::PadKey(code, val) => {
            // Only forward buttons we actually declared on the virtual pad, so an undeclared
            // DS4 key (e.g. the digital BTN_TL2/TR2 emitted alongside the analog triggers)
            // can never get stuck "on" in the virtual state.
            if DECLARED_KEYS.contains(&code) {
                ds4_keys.insert(code.0, val);
            }
        }
        Msg::TogglePs => {
            let was_active = active.fetch_xor(true, Ordering::Relaxed);
            // On any toggle, reset aim integration so it doesn't lurch on resume.
            *accum_x = 0.0;
            *accum_y = 0.0;
            *rx_f = 0.0;
            *ry_f = 0.0;
            println!(
                "joyclicks: {}",
                if was_active {
                    "paused (mouse released)"
                } else {
                    "active"
                }
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_desired(
    rx_f: f32,
    ry_f: f32,
    deadzone: f32,
    boost: f32,
    ds4_lx: i32,
    ds4_ly: i32,
    ds4_lt: i32,
    ds4_rt: i32,
    ds4_hatx: i32,
    ds4_haty: i32,
    ds4_keys: &HashMap<u16, i32>,
    buttons: &config::Buttons,
    mouse_ctrls: &HashMap<u8, bool>,
    taps: &[(Control, u64)],
) -> Desired {
    let _ = buttons; // reserved for future per-target tuning
    let mut d = Desired::default();

    // Left stick from the DS4.
    d.abs.insert(AbsoluteAxisCode::ABS_X.0, ds4_lx);
    d.abs.insert(AbsoluteAxisCode::ABS_Y.0, ds4_ly);

    // Right stick from integrated mouse motion: deadzone, then (while moving) the low-end boost.
    let (rxo, ryo) = apply_deadzone(rx_f, ry_f, deadzone);
    let (rxb, ryb) = apply_low_end_boost(rxo, ryo, boost);
    d.abs.insert(AbsoluteAxisCode::ABS_RX.0, frac_to_rstick(rxb));
    d.abs.insert(AbsoluteAxisCode::ABS_RY.0, frac_to_rstick(ryb));

    let mut lt = ds4_lt;
    let mut rt = ds4_rt;
    let mut hatx = ds4_hatx;
    let mut haty = ds4_haty;

    // Start virtual buttons from whatever the DS4 is holding.
    for (code, val) in ds4_keys {
        if *val != 0 {
            d.keys.insert(*code, 1);
        }
    }

    // Overlay active mouse controls and wheel taps onto keys / triggers / hat.
    let apply_ctrl = |c: Control, d: &mut Desired, lt: &mut i32, rt: &mut i32, hatx: &mut i32, haty: &mut i32| {
        if let Some(t) = control_target(c) {
            match t {
                Target::Key(k) => {
                    d.keys.insert(k.0, 1);
                }
                Target::Trigger(right) => {
                    if right {
                        *rt = TRIGGER_MAX;
                    } else {
                        *lt = TRIGGER_MAX;
                    }
                }
                Target::Hat(is_x, v) => {
                    if is_x {
                        *hatx = v;
                    } else {
                        *haty = v;
                    }
                }
            }
        }
    };

    for (ctrl_u8, pressed) in mouse_ctrls {
        if *pressed {
            apply_ctrl(control_from_u8(*ctrl_u8), &mut d, &mut lt, &mut rt, &mut hatx, &mut haty);
        }
    }
    for (ctrl, _) in taps {
        apply_ctrl(*ctrl, &mut d, &mut lt, &mut rt, &mut hatx, &mut haty);
    }

    d.abs.insert(AbsoluteAxisCode::ABS_Z.0, lt);
    d.abs.insert(AbsoluteAxisCode::ABS_RZ.0, rt);
    d.abs.insert(AbsoluteAxisCode::ABS_HAT0X.0, hatx);
    d.abs.insert(AbsoluteAxisCode::ABS_HAT0Y.0, haty);

    // Ensure every declared key has an explicit value so releases are emitted.
    for k in DECLARED_KEYS {
        d.keys.entry(k.0).or_insert(0);
    }

    d
}

fn apply_deadzone(x: f32, y: f32, dz: f32) -> (f32, f32) {
    if dz <= 0.0 {
        return (x, y);
    }
    let mag = (x * x + y * y).sqrt();
    if mag < dz {
        (0.0, 0.0)
    } else {
        (x, y)
    }
}

/// Anti-deadzone: while actively moving, remap the vector's magnitude so it's at least `boost`
/// (direction preserved), lifting tiny movements past the game's own internal deadzone. Gated
/// on `active` so a settling/springing-back stick isn't held out at the floor (no drift).
fn apply_low_end_boost(x: f32, y: f32, boost: f32) -> (f32, f32) {
    if boost <= 0.0 {
        return (x, y);
    }
    let mag = (x * x + y * y).sqrt();
    if mag <= 1e-4 {
        return (0.0, 0.0);
    }
    if mag >= 1.0 {
        return (x, y); // only lift the low end; leave the high end (incl. diagonals) untouched
    }
    let new_mag = boost + (1.0 - boost) * mag; // mag < 1 => new_mag < 1, no circular clamp needed
    let scale = new_mag / mag;
    (x * scale, y * scale)
}

/// Map a right-stick fraction (-1..1) to the 16-bit symmetric axis range (center 0).
fn frac_to_rstick(f: f32) -> i32 {
    ((f * RSTICK_MAX as f32).round() as i32).clamp(-RSTICK_MAX, RSTICK_MAX)
}

/// Reverse of `Control as u8` for the small set we store in the mouse-control map.
fn control_from_u8(v: u8) -> Control {
    match v {
        1 => Control::R2,
        2 => Control::L2,
        3 => Control::R1,
        4 => Control::L1,
        5 => Control::Cross,
        6 => Control::Circle,
        7 => Control::Square,
        8 => Control::Triangle,
        9 => Control::L3,
        10 => Control::R3,
        11 => Control::DpadUp,
        12 => Control::DpadDown,
        13 => Control::DpadLeft,
        14 => Control::DpadRight,
        _ => Control::None,
    }
}

fn emit_diff(vdev: &mut VirtualDevice, last: &Desired, cur: &Desired) {
    let mut events = Vec::new();
    for (code, val) in &cur.keys {
        if last.keys.get(code) != Some(val) {
            events.push(*KeyEvent::new(KeyCode(*code), *val));
        }
    }
    for (code, val) in &cur.abs {
        if last.abs.get(code) != Some(val) {
            events.push(*AbsoluteAxisEvent::new(AbsoluteAxisCode(*code), *val));
        }
    }
    if !events.is_empty() {
        if let Err(e) = vdev.emit(&events) {
            eprintln!("joyclicks: emit error: {e}");
        }
    }
}
