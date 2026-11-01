//! Wire format. Every message is one JSON object in a WebSocket text frame.
//! The full, user-facing description lives in docs/FFB-SERVER.md; keep the two in sync.

use serde::{Deserialize, Serialize};

pub use crate::model::{Telemetry, WheelT};

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_LISTEN: &str = "127.0.0.1:47820";

/// Optional first message from a game: who is talking. Shown in the app and in /stream.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Hello {
    /// Human-readable game name, e.g. "Farming Simulator 22".
    pub game: String,
    /// What is sending: mod or plugin name and version, e.g. "FS22_WheelFFB 1.0".
    pub source: String,
    /// Protocol version the client was written for (1).
    pub protocol: u32,
}

/// Forces computed by the game itself. The server only clamps them, applies the master gain
/// and the watchdog. Use this when the game already knows what the wheel should feel.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ForcesFrame {
    /// false = player is in a menu / paused: forces off.
    pub active: bool,
    /// Self-centering stiffness 0..1.
    pub spring: f32,
    /// Resistance to steering speed (weight) 0..1.
    pub damper: f32,
    /// Push, -1..1. Positive turns the wheel one way, negative the other.
    pub constant: f32,
    /// Vibration channel 1 (road texture) amplitude 0..1 and frequency in Hz.
    pub texture: f32,
    pub texture_hz: f32,
    /// Vibration channel 2 (engine) amplitude 0..1 and frequency in Hz.
    pub engine: f32,
    pub engine_hz: f32,
}

impl Default for ForcesFrame {
    fn default() -> Self {
        Self { active: true, spring: 0.0, damper: 0.0, constant: 0.0, texture: 0.0, texture_hz: 30.0, engine: 0.0, engine_hz: 25.0 }
    }
}

#[derive(Debug, Clone)]
pub enum Inbound {
    Hello(Hello),
    Telemetry(Box<Telemetry>),
    Forces(ForcesFrame),
}

/// Parses one client message. A message without "type" is telemetry (protocol v0 compatibility).
pub fn parse(text: &str) -> Result<Inbound, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    if !v.is_object() {
        return Err("message must be a JSON object".into());
    }
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("telemetry").to_string();
    let ty = ty.as_str();
    let err = |e: serde_json::Error| format!("bad {ty} message: {e}");
    match ty {
        "hello" => serde_json::from_value(v).map(Inbound::Hello).map_err(err),
        "telemetry" => serde_json::from_value(v).map(|t| Inbound::Telemetry(Box::new(t))).map_err(err),
        "forces" => serde_json::from_value(v).map(Inbound::Forces).map_err(err),
        other => Err(format!("unknown message type \"{other}\" (expected hello, telemetry or forces)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untyped_is_telemetry_and_types_route() {
        assert!(matches!(parse(r#"{"speed":3,"engine":true,"wheels":[]}"#), Ok(Inbound::Telemetry(_))));
        assert!(matches!(parse(r#"{"type":"hello","game":"X"}"#), Ok(Inbound::Hello(h)) if h.game == "X"));
        match parse(r#"{"type":"forces","spring":0.5}"#) {
            Ok(Inbound::Forces(f)) => assert!(f.active && f.spring == 0.5 && f.texture_hz == 30.0),
            other => panic!("{other:?}"),
        }
        match parse(r#"{"type":"telemetry","speed":1}"#) {
            Ok(Inbound::Telemetry(t)) => assert!(t.active, "active defaults to true"),
            other => panic!("{other:?}"),
        }
        assert!(parse(r#"{"type":"nope"}"#).is_err());
        assert!(parse("[1]").is_err());
        assert!(parse("{").is_err());
    }
}
