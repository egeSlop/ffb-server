//! Telemetry format (see docs/FFB-SERVER.md) and the physics that turns it into wheel forces.

use serde::{Deserialize, Serialize};

use crate::wheel::Forces;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WheelT {
    pub i: u32,
    pub steered: bool,
    pub steer: Option<f32>,
    pub steer_max: Option<f32>,
    pub y: Option<f32>,
    pub x: Option<f32>,
    pub surface: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Telemetry {
    /// Optional; same meaning as Hello.game, for clients that can't send a hello.
    pub game: Option<String>,
    pub seq: u64,
    pub t: f64,
    /// false = menu / pause / not in a vehicle. Missing means true.
    #[serde(default = "yes")]
    pub active: bool,
    pub speed: f32,
    pub steer_input: Option<f32>,
    pub engine: bool,
    pub rpm: Option<f32>,
    pub max_rpm: Option<f32>,
    pub load: Option<f32>,
    pub v_lat: Option<f32>,
    pub v_up: Option<f32>,
    pub v_fwd: Option<f32>,
    pub yaw_rate: Option<f32>,
    pub mass: Option<f32>,
    pub wheels: Vec<WheelT>,
}

/// Tuning, all 0..1. Lives in the [feel] table of the server settings.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Feel {
    /// Self-centering at speed.
    pub centering: f32,
    /// Self-centering when (nearly) stopped.
    pub centering_parked: f32,
    /// Base steering weight.
    pub damping: f32,
    /// Steering weight with the engine off (no power steering).
    pub damping_engine_off: f32,
    pub bumps: f32,
    pub impacts: f32,
    pub texture: f32,
    pub engine: f32,
}

impl Default for Feel {
    fn default() -> Self {
        Self {
            centering: 0.55,
            centering_parked: 0.08,
            damping: 0.30,
            damping_engine_off: 0.65,
            bumps: 0.60,
            impacts: 0.80,
            texture: 0.25,
            engine: 0.10,
        }
    }
}

/// Per-surface roughness (texture strength) and grip (how much centering survives).
/// Unknown names count as a generic firm surface, so a game can send its own names safely.
pub fn surface_props(s: &str) -> (f32, f32) {
    match s {
        "road" | "asphalt" | "concrete" | "tarmac" => (0.15, 1.00),
        "hard" | "gravel" | "dirt" | "cobbles" | "kerb" => (0.45, 0.85),
        "soft" | "grass" | "sand" => (0.35, 0.60),
        "field" | "mud" => (0.70, 0.70),
        "snow" => (0.30, 0.40),
        "ice" => (0.10, 0.20),
        "air" => (0.0, 0.0), // wheels off the ground
        _ => (0.35, 0.85),
    }
}

fn yes() -> bool {
    true
}

fn smoothstep(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

#[derive(Default)]
pub struct Model {
    prev: Option<Telemetry>,
    impulse: f32,
    bump: f32,
    flip: bool,
}

impl Model {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn compute(&mut self, tel: &Telemetry, feel: &Feel) -> Forces {
        let v = tel.speed.abs();
        let kmh = v * 3.6;
        let s = smoothstep(kmh / 40.0);

        let steered: Vec<&WheelT> = tel.wheels.iter().filter(|w| w.steered).collect();
        let contact: Vec<&WheelT> = if steered.is_empty() { tel.wheels.iter().collect() } else { steered.clone() };
        let surface = dominant_surface(&contact);
        let (rough, grip) = surface_props(&surface);

        // Steering weight.
        let parked = feel.centering_parked;
        let spring = (parked + (feel.centering - parked) * s * grip).max(parked * grip.max(0.3));
        let mut damper = feel.damping * (0.6 + 0.4 * s);
        if !tel.engine && kmh < 3.0 {
            damper = damper.max(feel.damping_engine_off);
        }

        // Transients need a time step; ignore gaps and duplicates.
        let dt = self.prev.as_ref().map(|p| (tel.t - p.t) as f32).unwrap_or(0.0);
        let have_dt = (0.004..0.25).contains(&dt);

        if have_dt {
            let prev = self.prev.as_ref().unwrap();
            // Bumps: left vs right suspension speed on the steered axle twists the wheel.
            let (mut left, mut right, mut nl, mut nr) = (0.0f32, 0.0f32, 0, 0);
            for w in &contact {
                let (Some(y), Some(py)) = (w.y, prev.wheels.iter().find(|p| p.i == w.i).and_then(|p| p.y)) else { continue };
                let vy = (y - py) / dt;
                if w.x.unwrap_or(if w.i % 2 == 1 { 1.0 } else { -1.0 }) >= 0.0 { left += vy; nl += 1 } else { right += vy; nr += 1 }
            }
            if nl > 0 && nr > 0 {
                let diff = left / nl as f32 - right / nr as f32;
                let target = (diff * 1.2).clamp(-1.0, 1.0) * feel.bumps;
                self.bump += (target - self.bump) * (dt / 0.03).min(1.0);
            }

            // Impacts: sudden longitudinal deceleration (hitting something, a hard landing).
            if let (Some(vf), Some(pvf)) = (tel.v_fwd, prev.v_fwd) {
                let a = (vf - pvf) / dt;
                if a.abs() > 12.0 {
                    let k = ((a.abs() - 12.0) / 30.0).min(1.0) * feel.impacts;
                    let sign = match tel.yaw_rate {
                        Some(y) if y.abs() > 0.05 => y.signum(),
                        _ => { self.flip = !self.flip; if self.flip { 1.0 } else { -1.0 } }
                    };
                    self.impulse = (self.impulse + sign * k).clamp(-1.0, 1.0);
                }
            }
            // Vertical jolts (landing after a bump) give a short kick too.
            if let (Some(vu), Some(pvu)) = (tel.v_up, prev.v_up) {
                let a = (vu - pvu) / dt;
                if a.abs() > 25.0 {
                    let k = ((a.abs() - 25.0) / 60.0).min(1.0) * feel.bumps * 0.5;
                    self.flip = !self.flip;
                    self.impulse = (self.impulse + if self.flip { k } else { -k }).clamp(-1.0, 1.0);
                }
            }
            self.impulse *= (-dt / 0.15).exp();
            self.bump *= (-dt / 0.25).exp();
        }

        let constant = (self.bump + self.impulse).clamp(-1.0, 1.0);

        // Surface texture: stronger on rough ground, faster with speed.
        let texture = feel.texture * rough * (kmh / 15.0).clamp(0.0, 1.0);
        let tex_hz = (8.0 + kmh * 0.6).clamp(8.0, 45.0);

        // Engine: 4-stroke firing frequency ~ rpm/30 Hz, felt mostly when slow.
        let (engine, eng_hz) = match (tel.engine, tel.rpm) {
            (true, Some(rpm)) if rpm > 100.0 => {
                let load = tel.load.unwrap_or(0.3).clamp(0.0, 1.0);
                (feel.engine * (0.5 + 0.5 * load) * (1.0 - 0.7 * s), (rpm / 30.0).clamp(10.0, 80.0))
            }
            _ => (0.0, 30.0),
        };

        self.prev = Some(tel.clone());
        Forces {
            spring,
            damper,
            constant,
            texture,
            texture_period_ms: (1000.0 / tex_hz) as u16,
            engine,
            engine_period_ms: (1000.0 / eng_hz) as u16,
        }
    }
}

fn dominant_surface(ws: &[&WheelT]) -> String {
    let mut best = ("air".to_string(), 0);
    for w in ws {
        let n = ws.iter().filter(|o| o.surface == w.surface).count();
        if n > best.1 || (n == best.1 && w.surface != "air" && best.0 == "air") {
            best = (w.surface.clone(), n);
        }
    }
    best.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tel(t: f64, speed: f32, surface: &str, yl: f32, yr: f32) -> Telemetry {
        Telemetry {
            t, active: true, speed, engine: true, rpm: Some(1200.0), load: Some(0.5),
            v_fwd: Some(speed), v_up: Some(0.0), yaw_rate: Some(0.0),
            wheels: vec![
                WheelT { i: 1, steered: true, y: Some(yl), x: Some(1.0), surface: surface.into(), ..Default::default() },
                WheelT { i: 2, steered: true, y: Some(yr), x: Some(-1.0), surface: surface.into(), ..Default::default() },
                WheelT { i: 3, surface: surface.into(), ..Default::default() },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn centering_grows_with_speed_and_falls_on_fields() {
        let f = Feel::default();
        let parked = Model::default().compute(&tel(0.0, 0.0, "road", 0.0, 0.0), &f);
        let road = Model::default().compute(&tel(0.0, 15.0, "road", 0.0, 0.0), &f);
        let field = Model::default().compute(&tel(0.0, 15.0, "field", 0.0, 0.0), &f);
        assert!(road.spring > parked.spring * 3.0);
        assert!(field.spring < road.spring);
        assert!(field.texture > road.texture);
    }

    #[test]
    fn one_sided_bump_kicks_the_wheel() {
        let f = Feel::default();
        let mut m = Model::default();
        m.compute(&tel(0.000, 8.0, "road", 0.00, 0.00), &f);
        let out = m.compute(&tel(0.016, 8.0, "road", 0.05, 0.00), &f);
        assert!(out.constant > 0.1, "left wheel rising should push: {}", out.constant);
        let mut m2 = Model::default();
        m2.compute(&tel(0.000, 8.0, "road", 0.00, 0.00), &f);
        let sym = m2.compute(&tel(0.016, 8.0, "road", 0.05, 0.05), &f);
        assert!(sym.constant.abs() < 0.01, "both wheels together shouldn't twist");
    }

    #[test]
    fn engine_off_makes_steering_heavy() {
        let f = Feel::default();
        let mut t = tel(0.0, 0.0, "road", 0.0, 0.0);
        t.engine = false;
        let off = Model::default().compute(&t, &f);
        let on = Model::default().compute(&tel(0.0, 0.0, "road", 0.0, 0.0), &f);
        assert!(off.damper > on.damper);
        assert_eq!(off.engine, 0.0);
    }
}
