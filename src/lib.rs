//! Game-agnostic force-feedback server for Linux (evdev) and Windows (DirectInput 8, untested).
//!
//! Games (mods, plugins, telemetry adapters) connect to `ws://127.0.0.1:47820/telemetry` and send
//! either vehicle telemetry (the server's model turns it into forces) or ready-made forces.
//! The server drives the wheel through evdev force feedback (DirectInput on Windows). Protocol: docs/FFB-SERVER.md.
//!
//! Used by V987 Wheel Studio (GUI) and by the `ffb-server` binary (headless).

pub mod cli;
pub mod config;
pub mod model;
pub mod protocol;
pub mod server;
pub mod wheel;

pub use config::Config;
pub use server::{FfbServer, Status};
