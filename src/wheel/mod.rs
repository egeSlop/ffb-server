//! The wheel backend: force feedback on the physical device.
//!
//! - Linux: raw evdev force feedback (EVIOCSFF) on `/dev/input/eventN` (`linux.rs`).
//! - Windows: DirectInput 8 (`windows.rs`). UNTESTED on real hardware.
//!
//! Both expose the same `Wheel` API; `Forces` is shared.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Wheel;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::Wheel;

#[cfg(not(any(target_os = "linux", windows)))]
compile_error!("ffb-server supports Linux (evdev) and Windows (DirectInput) only");

/// The effects the bridge keeps loaded on the wheel. Each is uploaded once, then updated in place.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Forces {
    /// Self-centering stiffness, 0..1.
    pub spring: f32,
    /// Resistance proportional to steering speed, 0..1.
    pub damper: f32,
    /// Transient push, -1..1 (bumps, impacts).
    pub constant: f32,
    /// Surface texture vibration amplitude 0..1, and its period in ms.
    pub texture: f32,
    pub texture_period_ms: u16,
    /// Engine vibration amplitude 0..1, and its period in ms.
    pub engine: f32,
    pub engine_period_ms: u16,
}
