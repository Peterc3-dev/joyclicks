//! KeyStick configuration: loaded from ~/.config/keystick/config.toml (created with
//! sensible defaults on first run). All aim-feel tuning and the mouse button map live here.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A PS3/virtual-pad control that a mouse input can be mapped to.
/// Deserializes directly from a string in the TOML (e.g. side = "Cross").
// Discriminants are explicit and MUST stay in sync with `control_from_u8` in main.rs
// (the mouse-control map round-trips through `Control as u8`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Control {
    None = 0,
    R2 = 1,
    L2 = 2,
    R1 = 3,
    L1 = 4,
    Cross = 5,
    Circle = 6,
    Square = 7,
    Triangle = 8,
    L3 = 9,
    R3 = 10,
    DpadUp = 11,
    DpadDown = 12,
    DpadLeft = 13,
    DpadRight = 14,
}

impl Default for Control {
    fn default() -> Self {
        Control::None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Devices {
    /// Substring matched against the gamepad's evdev name.
    pub pad_name: String,
    /// Substring matched against the mouse's evdev name.
    pub mouse_name: String,
}

impl Default for Devices {
    fn default() -> Self {
        Devices {
            pad_name: "Wireless Controller".to_string(),
            mouse_name: "Gaming Mouse".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct Aim {
    /// Mouse counts -> right-stick fraction added per count (raw gain before the spring).
    pub sensitivity: f32,
    /// Extra gain on the VERTICAL axis only, relative to horizontal. Compensates for games
    /// whose pitch speed differs from yaw (console shooters usually pitch slower, so a
    /// hand-drawn circle becomes a wide oval — raise this above 1.0 to round it out).
    /// 1.0 = symmetric.
    pub y_scale: f32,
    /// Per-tick spring-back toward center, 0..1. Higher = the stick holds deflection longer
    /// (slower recenter). Lower = snappier stop. Turn rate ends up ~proportional to mouse speed.
    pub decay: f32,
    /// Maximum right-stick deflection as a fraction, 0..1.
    pub max_deflection: f32,
    /// Ignore right-stick deflection below this fraction (0..1) after integration.
    pub deadzone: f32,
    /// Low-end boost (anti-deadzone): while the mouse is ACTIVELY moving, guarantee the stick
    /// output magnitude is at least this fraction (0..~0.9), so the smallest slow movements
    /// punch through the GAME's own internal deadzone instead of producing nothing. It's gated
    /// on active movement, so it does not hold the stick out (drift) after you stop. 0 = off.
    /// Higher = smaller movements register, but with more initial "snap".
    pub low_end_boost: f32,
    /// Invert the vertical aim axis.
    pub invert_y: bool,
    /// Integration/emit tick rate in Hz. 500 is a good default.
    pub tick_hz: u32,
}

impl Default for Aim {
    fn default() -> Self {
        Aim {
            sensitivity: 0.030,
            y_scale: 1.0,
            decay: 0.85,
            max_deflection: 1.0,
            deadzone: 0.0,
            low_end_boost: 0.0,
            invert_y: false,
            tick_hz: 500,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Buttons {
    pub left_click: Control,
    pub right_click: Control,
    pub middle_click: Control,
    pub side: Control,
    pub extra: Control,
    pub wheel_up: Control,
    pub wheel_down: Control,
    /// How long a wheel "tap" is held, in milliseconds, before auto-release.
    pub tap_ms: u32,
}

impl Default for Buttons {
    fn default() -> Self {
        Buttons {
            left_click: Control::R2,    // fire
            right_click: Control::L2,   // aim down sights
            middle_click: Control::R3,  // melee / stick click
            side: Control::Cross,       // jump
            extra: Control::Square,     // reload / interact
            wheel_up: Control::DpadUp,  // next weapon
            wheel_down: Control::DpadDown,
            tap_ms: 50,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub devices: Devices,
    pub aim: Aim,
    pub buttons: Buttons,
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .context("neither XDG_CONFIG_HOME nor HOME is set")?;
        Ok(base.join("keystick").join("config.toml"))
    }

    /// Clamp aim values to sane ranges so a bad config can't pin or freeze the stick.
    pub fn sanitize(&mut self) {
        let a = &mut self.aim;
        if !a.sensitivity.is_finite() || a.sensitivity < 0.0 {
            a.sensitivity = Aim::default().sensitivity;
        }
        a.sensitivity = a.sensitivity.clamp(0.0, 10.0);
        if !a.y_scale.is_finite() || a.y_scale <= 0.0 {
            a.y_scale = 1.0;
        }
        a.y_scale = a.y_scale.clamp(0.05, 20.0);
        if !a.decay.is_finite() {
            a.decay = Aim::default().decay;
        }
        a.decay = a.decay.clamp(0.0, 0.999); // < 1 guarantees spring-back to center
        if !a.max_deflection.is_finite() {
            a.max_deflection = 1.0;
        }
        a.max_deflection = a.max_deflection.clamp(0.0, 1.0);
        if !a.deadzone.is_finite() || a.deadzone < 0.0 {
            a.deadzone = 0.0;
        }
        a.deadzone = a.deadzone.clamp(0.0, 1.0);
        if !a.low_end_boost.is_finite() || a.low_end_boost < 0.0 {
            a.low_end_boost = 0.0;
        }
        // Cap the boost floor at 0.9 and never above the deflection ceiling it would exceed.
        a.low_end_boost = a.low_end_boost.clamp(0.0, 0.9).min(a.max_deflection);
        if a.tick_hz == 0 {
            a.tick_hz = Aim::default().tick_hz;
        }
    }

    /// Load the config, or write out the defaults and return them if it doesn't exist yet.
    pub fn load_or_create() -> Result<Config> {
        let path = Self::path()?;
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mut cfg: Config = toml::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?;
            cfg.sanitize();
            Ok(cfg)
        } else {
            let cfg = Config::default();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            let text = toml::to_string_pretty(&cfg).context("serializing default config")?;
            std::fs::write(&path, text)
                .with_context(|| format!("writing {}", path.display()))?;
            eprintln!("keystick: wrote default config to {}", path.display());
            Ok(cfg)
        }
    }
}
