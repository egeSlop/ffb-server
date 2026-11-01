//! Settings file: ~/.config/v987-wheel-studio/ffb-server.toml
//! (Windows: %APPDATA%\v987-wheel-studio\ffb-server.toml).
//! Shared by the app and the headless server; whichever is running reloads it when it changes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::Feel;
use crate::protocol::DEFAULT_LISTEN;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Overall strength 0..1, applied by the wheel itself (FF_GAIN / DIPROP_FFGAIN).
    pub master_gain: f32,
    /// Flip the direction of pushes if they feel backwards.
    pub invert: bool,
    /// No message for this long means the game stopped: forces go to zero.
    pub stale_ms: u64,
    /// Address to listen on. Keep it on 127.0.0.1 unless you know why.
    pub listen: String,
    /// "auto" (first force-feedback joystick, LiteStar wheels preferred), an evdev path on
    /// Linux, or "dinput:{instance-guid}" on Windows.
    pub device: String,
    /// Wheel Studio starts the server when it opens.
    pub autostart: bool,
    /// How telemetry turns into forces. Not used for games that send "forces" directly.
    pub feel: Feel,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            master_gain: 0.8,
            invert: false,
            stale_ms: 400,
            listen: DEFAULT_LISTEN.into(),
            device: "auto".into(),
            autostart: true,
            feel: Feel::default(),
        }
    }
}

impl Config {
    /// Keeps every value inside its documented range.
    pub fn sanitized(mut self) -> Self {
        let c = |x: &mut f32| *x = if x.is_finite() { x.clamp(0.0, 1.0) } else { 0.0 };
        c(&mut self.master_gain);
        let f = &mut self.feel;
        for x in [&mut f.centering, &mut f.centering_parked, &mut f.damping, &mut f.damping_engine_off, &mut f.bumps, &mut f.impacts, &mut f.texture, &mut f.engine] {
            c(x);
        }
        self.stale_ms = self.stale_ms.clamp(50, 5000);
        if self.listen.trim().is_empty() {
            self.listen = DEFAULT_LISTEN.into();
        }
        if self.device.trim().is_empty() {
            self.device = "auto".into();
        }
        self
    }
}

#[cfg(not(windows))]
fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"))
}

/// %APPDATA% (roaming), e.g. C:\Users\me\AppData\Roaming.
#[cfg(windows)]
fn config_home() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join("AppData").join("Roaming"))
}

pub fn path() -> PathBuf {
    config_home().join("v987-wheel-studio").join("ffb-server.toml")
}

const HEADER: &str = "# V987 Wheel Studio - FFB server settings. Values are 0..1 unless noted.\n\
# The server reloads this file while running; Wheel Studio edits it for you.\n\n";

/// Loads the settings, creating the file (or migrating the old fs22-ffb one) on first use.
pub fn load() -> Config {
    let p = path();
    if let Ok(s) = std::fs::read_to_string(&p) {
        return match toml::from_str::<Config>(&s) {
            Ok(c) => c.sanitized(),
            Err(e) => {
                eprintln!("[ffb-server] {}: {e}; using defaults", p.display());
                Config::default()
            }
        };
    }
    let legacy = config_home().join("fs22-ffb").join("config.toml");
    let c = std::fs::read_to_string(&legacy)
        .ok()
        .and_then(|s| toml::from_str::<Config>(&s).ok())
        .map(Config::sanitized)
        .unwrap_or_default();
    let _ = save(&c);
    c
}

pub fn save(c: &Config) -> std::io::Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = toml::to_string_pretty(c).map_err(std::io::Error::other)?;
    let tmp = p.with_extension("toml.tmp");
    std::fs::write(&tmp, format!("{HEADER}{body}"))?;
    std::fs::rename(tmp, p)
}

pub fn modified() -> Option<std::time::SystemTime> {
    std::fs::metadata(path()).and_then(|m| m.modified()).ok()
}
