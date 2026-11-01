//! The running server: a WebSocket listener plus one engine thread that owns the wheel.
//!
//!   /telemetry (or /)  games send hello / telemetry / forces messages
//!   /stream            anyone can watch: status twice a second, live frames while driving

use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use serde_json::json;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::{Error as WsError, Message};

use crate::config::{self, Config};
use crate::model::{Model, Telemetry};
use crate::protocol::{self, ForcesFrame, Inbound, PROTOCOL_VERSION};
use crate::wheel::{Forces, Wheel};

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub id: u64,
    pub peer: String,
    pub game: String,
    pub source: String,
    /// "telemetry" or "forces" once the first frame arrived, "" before.
    pub kind: String,
    pub frames: u64,
    pub errors: u64,
    pub last_error: Option<String>,
    pub connected_ms: u64,
    pub last_frame_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForcesOut {
    pub spring: f32,
    pub damper: f32,
    pub constant: f32,
    pub texture: f32,
    pub texture_hz: f32,
    pub engine: f32,
    pub engine_hz: f32,
}

impl From<&Forces> for ForcesOut {
    fn from(f: &Forces) -> Self {
        let hz = |ms: u16| if ms == 0 { 0.0 } else { (1000.0 / ms as f32).round() };
        Self {
            spring: f.spring,
            damper: f.damper,
            constant: f.constant,
            texture: f.texture,
            texture_hz: hz(f.texture_period_ms),
            engine: f.engine,
            engine_hz: hz(f.engine_period_ms),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WheelInfo {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub server: String,
    pub protocol: u32,
    pub listen: String,
    pub wheel: Option<WheelInfo>,
    pub wheel_error: Option<String>,
    /// "idle", "telemetry", "forces" or "test".
    pub mode: String,
    pub test_step: Option<String>,
    /// Game of the client currently driving the wheel.
    pub game: Option<String>,
    pub forces: ForcesOut,
    pub clients: Vec<ClientInfo>,
    pub subscribers: usize,
    pub config_path: String,
    /// The listen address in the settings differs from the running one.
    pub restart_needed: bool,
}

enum Frame {
    Telemetry(Box<Telemetry>),
    Forces(ForcesFrame),
}

struct Latest {
    at: Instant,
    client: u64,
    frame: Arc<Frame>,
}

struct Client {
    info: ClientInfo,
    since: Instant,
    last: Option<Instant>,
}

struct State {
    config: Config,
    config_changed: bool,
    clients: Vec<Client>,
    next_client: u64,
    latest: Option<Latest>,
    frame_no: u64,
    subscribers: Vec<Sender<String>>,
    test: Option<(String, Forces)>,
    // Written by the engine.
    wheel: Option<WheelInfo>,
    wheel_error: Option<String>,
    mode: &'static str,
    forces: Forces,
}

struct Shared {
    name: String,
    listen: String,
    started: Instant,
    stop: AtomicBool,
    state: Mutex<State>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

pub struct FfbServer {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl FfbServer {
    /// Binds and starts serving. `name` identifies the host program in welcome/status messages.
    pub fn start(name: &str, cfg: Config) -> anyhow::Result<Self> {
        let cfg = cfg.sanitized();
        let listener = TcpListener::bind(&cfg.listen).map_err(|e| {
            if e.kind() == ErrorKind::AddrInUse {
                anyhow::anyhow!("{} is already in use: another FFB server is running (for example the one the game launcher starts)", cfg.listen)
            } else {
                anyhow::anyhow!("can't listen on {}: {e}", cfg.listen)
            }
        })?;
        listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            name: name.to_string(),
            listen: cfg.listen.clone(),
            started: Instant::now(),
            stop: AtomicBool::new(false),
            state: Mutex::new(State {
                config: cfg,
                config_changed: false,
                clients: Vec::new(),
                next_client: 1,
                latest: None,
                frame_no: 0,
                subscribers: Vec::new(),
                test: None,
                wheel: None,
                wheel_error: None,
                mode: "idle",
                forces: Forces::default(),
            }),
        });
        eprintln!("[ffb-server] listening on ws://{} (/telemetry, /stream)", shared.listen);
        let a = {
            let s = shared.clone();
            thread::Builder::new().name("ffb-accept".into()).spawn(move || accept_loop(listener, s))?
        };
        let e = {
            let s = shared.clone();
            thread::Builder::new().name("ffb-engine".into()).spawn(move || engine(s))?
        };
        Ok(Self { shared, threads: vec![a, e] })
    }

    pub fn status(&self) -> Status {
        status_of(&self.shared, &self.shared.lock())
    }

    pub fn config(&self) -> Config {
        self.shared.lock().config.clone()
    }

    /// Applies new settings right away and saves them to the settings file.
    pub fn set_config(&self, cfg: Config) -> std::io::Result<Config> {
        let cfg = cfg.sanitized();
        config::save(&cfg)?;
        let mut st = self.shared.lock();
        st.config = cfg.clone();
        st.config_changed = true;
        Ok(cfg)
    }

    /// Runs the ~12 s hardware check on the wheel. Returns false if one is already running.
    pub fn run_test(&self) -> bool {
        let mut st = self.shared.lock();
        if st.test.is_some() {
            return false;
        }
        st.test = Some(("starting".into(), Forces::default()));
        drop(st);
        let s = self.shared.clone();
        thread::spawn(move || {
            for (name, f, ms) in test_steps() {
                if s.stopped() {
                    break;
                }
                s.lock().test = Some((name.to_string(), f));
                thread::sleep(Duration::from_millis(ms));
            }
            s.lock().test = None;
        });
        true
    }

    /// Receives the same lines a /stream WebSocket client gets.
    pub fn subscribe(&self) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel();
        self.shared.lock().subscribers.push(tx);
        rx
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for FfbServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn test_steps() -> Vec<(&'static str, Forces, u64)> {
    let base = Forces { texture_period_ms: 50, engine_period_ms: 40, ..Default::default() };
    vec![
        ("push one way", Forces { constant: 0.4, ..base }, 2000),
        ("push the other way", Forces { constant: -0.4, ..base }, 2000),
        ("strong centering", Forces { spring: 0.8, ..base }, 3000),
        ("heavy steering - turn the wheel", Forces { damper: 0.8, ..base }, 3000),
        ("vibration", Forces { texture: 0.5, texture_period_ms: 40, ..base }, 2000),
    ]
}

fn status_of(sh: &Shared, st: &State) -> Status {
    let now = Instant::now();
    let game = st.latest.as_ref().and_then(|l| st.clients.iter().find(|c| c.info.id == l.client)).map(|c| c.info.game.clone()).filter(|g| !g.is_empty());
    Status {
        server: sh.name.clone(),
        protocol: PROTOCOL_VERSION,
        listen: sh.listen.clone(),
        wheel: st.wheel.clone(),
        wheel_error: st.wheel_error.clone(),
        mode: st.mode.to_string(),
        test_step: st.test.as_ref().map(|t| t.0.clone()),
        game: if st.mode == "idle" || st.mode == "test" { None } else { game },
        forces: ForcesOut::from(&st.forces),
        clients: st
            .clients
            .iter()
            .map(|c| ClientInfo {
                connected_ms: now.duration_since(c.since).as_millis() as u64,
                last_frame_ms: c.last.map(|l| now.duration_since(l).as_millis() as u64),
                ..c.info.clone()
            })
            .collect(),
        subscribers: st.subscribers.len(),
        config_path: config::path().display().to_string(),
        restart_needed: st.config.listen != sh.listen,
    }
}

fn broadcast(st: &mut State, line: &str) {
    st.subscribers.retain(|tx| tx.send(line.to_string()).is_ok());
}

// ---------------------------------------------------------------- network

fn accept_loop(listener: TcpListener, sh: Arc<Shared>) {
    while !sh.stopped() {
        match listener.accept() {
            Ok((stream, _)) => {
                let s = sh.clone();
                let _ = stream.set_nonblocking(false);
                thread::spawn(move || handle(stream, s));
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(40)),
            Err(_) => thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn is_timeout(e: &WsError) -> bool {
    matches!(e, WsError::Io(io) if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut))
}

fn handle(stream: TcpStream, sh: Arc<Shared>) {
    let _ = stream.set_nodelay(true);
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let mut path = String::new();
    let cb = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        path = req.uri().path().trim_end_matches('/').to_string();
        if matches!(path.as_str(), "" | "/telemetry" | "/stream") {
            Ok(resp)
        } else {
            let mut r = ErrorResponse::new(Some("unknown path; use /telemetry or /stream".into()));
            *r.status_mut() = tungstenite::http::StatusCode::NOT_FOUND;
            Err(r)
        }
    };
    let Ok(mut ws) = tungstenite::accept_hdr(stream, cb) else { return };
    let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(400)));

    if path == "/stream" {
        let (tx, rx) = mpsc::channel();
        let first = {
            let mut st = sh.lock();
            st.subscribers.push(tx);
            status_line(&sh, &st)
        };
        if ws.send(Message::text(first)).is_err() {
            return;
        }
        // Reads only drain pings/close; never let them hold up sending.
        let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(1)));
        let mut drained = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_millis(400)) {
                Ok(line) => {
                    if ws.send(Message::text(line)).is_err() {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if sh.stopped() {
                let _ = ws.close(None);
                break;
            }
            if drained.elapsed() > Duration::from_millis(250) {
                drained = Instant::now();
                match ws.read() {
                    Ok(Message::Close(_)) => break,
                    Err(e) if !is_timeout(&e) => break,
                    _ => {}
                }
            }
        }
        return;
    }

    // Game connection.
    let id = {
        let mut st = sh.lock();
        let id = st.next_client;
        st.next_client += 1;
        st.clients.push(Client { info: ClientInfo { id, peer: peer.clone(), ..Default::default() }, since: Instant::now(), last: None });
        id
    };
    eprintln!("[ffb-server] game connected ({peer}, client {id})");
    let wheel = sh.lock().wheel.as_ref().map(|w| w.name.clone());
    let welcome = json!({ "type": "welcome", "server": sh.name, "protocol": PROTOCOL_VERSION, "client": id, "wheel": wheel });
    let _ = ws.send(Message::text(welcome.to_string()));
    let mut replies = 0;

    while !sh.stopped() {
        let text = match ws.read() {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(e) if is_timeout(&e) => continue,
            Err(_) => break,
        };
        let parsed = protocol::parse(&text);
        let mut st = sh.lock();
        let t_now = sh.started.elapsed().as_secs_f64();
        let Some(ci) = st.clients.iter().position(|c| c.info.id == id) else { break };
        match parsed {
            Ok(Inbound::Hello(h)) => {
                let c = &mut st.clients[ci].info;
                c.game = h.game.trim().to_string();
                c.source = h.source.trim().to_string();
                eprintln!("[ffb-server] client {id}: {} ({})", c.game, c.source);
            }
            Ok(Inbound::Telemetry(mut t)) => {
                if t.t == 0.0 {
                    t.t = t_now;
                }
                let c = &mut st.clients[ci];
                if let Some(g) = t.game.as_ref().filter(|g| c.info.game.is_empty() && !g.is_empty()) {
                    c.info.game = g.clone();
                }
                c.info.kind = "telemetry".into();
                c.info.frames += 1;
                c.last = Some(Instant::now());
                st.frame_no += 1;
                st.latest = Some(Latest { at: Instant::now(), client: id, frame: Arc::new(Frame::Telemetry(t)) });
            }
            Ok(Inbound::Forces(f)) => {
                let c = &mut st.clients[ci];
                c.info.kind = "forces".into();
                c.info.frames += 1;
                c.last = Some(Instant::now());
                st.frame_no += 1;
                st.latest = Some(Latest { at: Instant::now(), client: id, frame: Arc::new(Frame::Forces(f)) });
            }
            Err(e) => {
                let c = &mut st.clients[ci].info;
                c.errors += 1;
                c.last_error = Some(e.clone());
                drop(st);
                if replies < 5 {
                    replies += 1;
                    let _ = ws.send(Message::text(json!({ "type": "error", "message": e }).to_string()));
                }
            }
        }
    }
    sh.lock().clients.retain(|c| c.info.id != id);
    eprintln!("[ffb-server] game disconnected (client {id})");
}

fn status_line(sh: &Shared, st: &State) -> String {
    let mut v = serde_json::to_value(status_of(sh, st)).unwrap_or_default();
    v["type"] = json!("status");
    v.to_string()
}

// ---------------------------------------------------------------- engine

fn direct(f: &ForcesFrame) -> Forces {
    let u = |x: f32| if x.is_finite() { x.clamp(0.0, 1.0) } else { 0.0 };
    let period = |hz: f32| if hz.is_finite() && hz > 0.0 { (1000.0 / hz.clamp(1.0, 200.0)) as u16 } else { 40 };
    Forces {
        spring: u(f.spring),
        damper: u(f.damper),
        constant: if f.constant.is_finite() { f.constant.clamp(-1.0, 1.0) } else { 0.0 },
        texture: u(f.texture),
        texture_period_ms: period(f.texture_hz),
        engine: u(f.engine),
        engine_period_ms: period(f.engine_hz),
    }
}

fn engine(sh: Arc<Shared>) {
    let mut cfg = sh.lock().config.clone();
    let mut wheel: Option<Wheel> = None;
    let mut retry_at = Instant::now();
    let mut model = Model::default();
    let mut last_frame = 0u64;
    let mut driving = false;
    let mut last_frame_out = Instant::now();
    let mut last_status = Instant::now() - Duration::from_secs(1);
    let mut last_check = Instant::now();
    let mut file_time: Option<SystemTime> = config::modified();
    let mut was_test = false;

    while !sh.stopped() {
        thread::sleep(Duration::from_millis(5));

        // Settings: from set_config() or from someone editing the file.
        if last_check.elapsed() > Duration::from_millis(500) {
            last_check = Instant::now();
            let m = config::modified();
            if m.is_some() && m != file_time {
                file_time = m;
                let c = config::load();
                let mut st = sh.lock();
                if c != st.config {
                    st.config = c;
                    st.config_changed = true;
                }
            }
        }
        {
            let mut st = sh.lock();
            if st.config_changed {
                st.config_changed = false;
                let new = st.config.clone();
                drop(st);
                if new.device != cfg.device {
                    wheel = None;
                    retry_at = Instant::now();
                }
                if let Some(w) = wheel.as_mut() {
                    let _ = w.set_gain(new.master_gain);
                    w.set_invert(new.invert);
                }
                cfg = new;
                file_time = config::modified();
            }
        }

        if wheel.is_none() && Instant::now() >= retry_at {
            retry_at = Instant::now() + Duration::from_secs(2);
            match Wheel::open_device(&cfg.device, cfg.invert) {
                Ok((path, mut w)) => {
                    let _ = w.set_gain(cfg.master_gain);
                    let info = WheelInfo { name: w.name(), path: path.display().to_string() };
                    eprintln!("[ffb-server] wheel: {} ({})", info.name, info.path);
                    let mut st = sh.lock();
                    st.wheel = Some(info);
                    st.wheel_error = None;
                    drop(st);
                    let _ = w.idle();
                    wheel = Some(w);
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    let mut st = sh.lock();
                    if st.wheel_error.as_deref() != Some(&msg) {
                        eprintln!("[ffb-server] {msg}");
                    }
                    st.wheel = None;
                    st.wheel_error = Some(msg);
                }
            }
        }

        // What should the wheel do now?
        let (test, live) = {
            let st = sh.lock();
            let live = st
                .latest
                .as_ref()
                .filter(|l| l.at.elapsed() < Duration::from_millis(cfg.stale_ms))
                .map(|l| (st.frame_no, l.frame.clone()));
            (st.test.clone(), live)
        };

        let mut out: Option<(Forces, &'static str, Option<Arc<Frame>>)> = None;
        if let Some((_, f)) = test {
            out = Some((f, "test", None));
            was_test = true;
        } else {
            if was_test {
                was_test = false;
                last_frame = 0; // re-apply the game's forces right after a test
                if let Some(w) = wheel.as_mut() {
                    let _ = w.idle();
                }
            }
            let active = |fr: &Frame| match fr {
                Frame::Telemetry(t) => t.active,
                Frame::Forces(f) => f.active,
            };
            match live {
                Some((n, fr)) if active(&fr) => {
                    if n != last_frame {
                        last_frame = n;
                        if !driving {
                            eprintln!("[ffb-server] driving: forces on");
                            driving = true;
                        }
                        let (f, mode) = match &*fr {
                            Frame::Telemetry(t) => {
                                let mut f = model.compute(t, &cfg.feel);
                                // Soft lock: computed for a non-inverted wheel, so undo `invert` here.
                                let push = crate::model::soft_lock_push(t, &cfg.feel);
                                if push != 0.0 {
                                    f.constant = (f.constant + if cfg.invert { -push } else { push }).clamp(-1.0, 1.0);
                                }
                                (f, "telemetry")
                            }
                            Frame::Forces(ff) => (direct(ff), "forces"),
                        };
                        out = Some((f, mode, Some(fr)));
                    }
                }
                _ => {
                    if driving {
                        eprintln!("[ffb-server] not driving (menu, pause, or the game stopped sending): forces off");
                        driving = false;
                        model.reset();
                        if let Some(w) = wheel.as_mut() {
                            let _ = w.idle();
                        }
                        let mut st = sh.lock();
                        st.mode = "idle";
                        st.forces = Forces::default();
                    }
                }
            }
        }

        if let Some((f, mode, frame)) = &out {
            if let Some(w) = wheel.as_mut() {
                if let Err(e) = w.apply(f) {
                    eprintln!("[ffb-server] wheel lost: {e}");
                    wheel = None;
                    let mut st = sh.lock();
                    st.wheel = None;
                    st.wheel_error = Some(format!("wheel lost: {e}"));
                }
            }
            let mut st = sh.lock();
            st.mode = mode;
            st.forces = *f;
            if last_frame_out.elapsed() > Duration::from_millis(33) && !st.subscribers.is_empty() {
                last_frame_out = Instant::now();
                let tel = match frame.as_deref() {
                    Some(Frame::Telemetry(t)) => serde_json::to_value(t).unwrap_or_default(),
                    _ => serde_json::Value::Null,
                };
                let game = st.latest.as_ref().and_then(|l| st.clients.iter().find(|c| c.info.id == l.client)).map(|c| c.info.game.clone());
                let line = json!({ "type": "frame", "mode": mode, "game": game, "telemetry": tel, "forces": ForcesOut::from(f) }).to_string();
                broadcast(&mut st, &line);
            }
        } else if test.is_none() && !driving {
            let mut st = sh.lock();
            if st.mode == "test" {
                st.mode = "idle";
                st.forces = Forces::default();
            }
        }

        if last_status.elapsed() > Duration::from_millis(500) {
            last_status = Instant::now();
            let mut st = sh.lock();
            if !st.subscribers.is_empty() {
                let line = status_line(&sh, &st);
                broadcast(&mut st, &line);
            }
        }
    }
    eprintln!("[ffb-server] stopping");
    // Dropping the wheel removes every effect, so it returns to its own idle behaviour.
    drop(wheel);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_forces_are_clamped() {
        let f = direct(&ForcesFrame { spring: 3.0, damper: -1.0, constant: f32::NAN, texture_hz: 0.0, engine_hz: 1000.0, ..Default::default() });
        assert_eq!((f.spring, f.damper, f.constant), (1.0, 0.0, 0.0));
        assert_eq!(f.texture_period_ms, 40);
        assert_eq!(f.engine_period_ms, 5);
    }
}
