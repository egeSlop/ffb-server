//! Direct Linux force-feedback (EVIOCSFF) on the wheel's evdev node.
//!
//! Written against <linux/input.h> instead of a crate because the evdev crate sends damper
//! and inertia effects with zeroed coefficients, which would silently disable damping.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::Forces;

const EV_FF: u16 = 0x15;
const FF_CONSTANT: u16 = 0x52;
const FF_PERIODIC: u16 = 0x51;
const FF_SPRING: u16 = 0x53;
const FF_DAMPER: u16 = 0x55;
const FF_SINE: u16 = 0x5a;
const FF_GAIN: u16 = 0x60;
const FF_AUTOCENTER: u16 = 0x61;

/// Direction 0x4000 is "east": with a wheel, positive levels push one way, negative the other.
const DIRECTION: u16 = 0x4000;

const EVIOCSFF: libc::c_ulong = 0x4030_4580; // _IOW('E', 0x80, struct ff_effect) (48 bytes)
const EVIOCRMFF: libc::c_ulong = 0x4004_4581; // _IOW('E', 0x81, int)

#[repr(C)]
#[derive(Clone, Copy)]
struct Trigger { button: u16, interval: u16 }

#[repr(C)]
#[derive(Clone, Copy)]
struct Replay { length: u16, delay: u16 }

#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct Union([u8; 32]);

#[repr(C)]
#[derive(Clone, Copy)]
struct FfEffect {
    kind: u16,
    id: i16,
    direction: u16,
    trigger: Trigger,
    replay: Replay,
    u: Union,
}

const _: () = assert!(std::mem::size_of::<FfEffect>() == 48);

impl FfEffect {
    fn new(kind: u16, id: i16) -> Self {
        Self {
            kind,
            id,
            direction: DIRECTION,
            trigger: Trigger { button: 0, interval: 0 },
            replay: Replay { length: 0, delay: 0 }, // 0 = infinite
            u: Union([0; 32]),
        }
    }
    fn put_i16(&mut self, off: usize, v: i16) { self.u.0[off..off + 2].copy_from_slice(&v.to_ne_bytes()); }
    fn put_u16(&mut self, off: usize, v: u16) { self.u.0[off..off + 2].copy_from_slice(&v.to_ne_bytes()); }
}

pub struct Wheel {
    file: File,

    ids: [i16; 5], // spring, damper, constant, texture, engine
    last: Option<Forces>,
    invert: bool,
}

fn to_i16(x: f32) -> i16 {
    (x.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

impl Wheel {
    /// Force-feedback candidates under /dev/input/by-id, LiteStar ("FFB_WHEEL") wheels first.
    pub fn candidates() -> Result<Vec<PathBuf>> {
        let dir = Path::new("/dev/input/by-id");
        let mut v: Vec<(bool, PathBuf)> = Vec::new();
        for e in std::fs::read_dir(dir).context("reading /dev/input/by-id")? {
            let p = e?.path();
            let n = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            if n.ends_with("-event-joystick") {
                if let Ok(real) = std::fs::canonicalize(&p) {
                    v.push((!n.contains("FFB_WHEEL"), real));
                }
            }
        }
        v.sort();
        v.dedup_by(|a, b| a.1 == b.1);
        Ok(v.into_iter().map(|(_, p)| p).collect())
    }

    /// Opens `device` ("auto" or an evdev path). With "auto", the first joystick that accepts
    /// force-feedback effects wins.
    pub fn open_device(device: &str, invert: bool) -> Result<(PathBuf, Self)> {
        if device != "auto" {
            let p = PathBuf::from(device);
            return Ok((p.clone(), Self::open(&p, invert)?));
        }
        let mut last = None;
        for p in Self::candidates()? {
            match Self::open(&p, invert) {
                Ok(w) => return Ok((p, w)),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("no force-feedback wheel found (is it plugged in?)")))
    }

    /// The device's own name (EVIOCGNAME).
    pub fn name(&self) -> String {
        let mut buf = [0u8; 256];
        const EVIOCGNAME_256: libc::c_ulong = 0x8100_4506;
        let n = unsafe { libc::ioctl(self.file.as_raw_fd(), EVIOCGNAME_256 as _, buf.as_mut_ptr()) };
        if n <= 0 {
            return String::new();
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(n as usize);
        String::from_utf8_lossy(&buf[..end]).trim().to_string()
    }

    pub fn set_invert(&mut self, invert: bool) {
        if self.invert != invert {
            self.invert = invert;
            self.last = None; // re-send everything with the new sign
        }
    }

    pub fn open(path: &Path, invert: bool) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)
            .with_context(|| format!("opening {} (permissions?)", path.display()))?;
        let mut w = Self { file, ids: [-1; 5], last: None, invert };
        w.ids[0] = w.upload(FfEffect::new(FF_SPRING, -1))?;
        w.ids[1] = w.upload(FfEffect::new(FF_DAMPER, -1))?;
        w.ids[2] = w.upload(FfEffect::new(FF_CONSTANT, -1))?;
        let mut tex = FfEffect::new(FF_PERIODIC, -1);
        tex.put_u16(0, FF_SINE);
        tex.put_u16(2, 50);
        w.ids[3] = w.upload(tex)?;
        w.ids[4] = w.upload(tex)?;
        for id in w.ids {
            w.play(id, true)?;
        }
        w.set_autocenter(0)?;
        Ok(w)
    }

    fn upload(&mut self, mut e: FfEffect) -> Result<i16> {
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), EVIOCSFF as _, &mut e as *mut FfEffect) };
        if r < 0 {
            bail!("EVIOCSFF failed: {}", std::io::Error::last_os_error());
        }
        Ok(e.id)
    }

    fn event(&mut self, code: u16, value: i32) -> Result<()> {
        let ev = libc::input_event {
            time: libc::timeval { tv_sec: 0, tv_usec: 0 },
            type_: EV_FF,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(&ev as *const _ as *const u8, std::mem::size_of::<libc::input_event>())
        };
        self.file.write_all(bytes)?;
        Ok(())
    }

    fn play(&mut self, id: i16, on: bool) -> Result<()> {
        self.event(id as u16, on as i32)
    }

    /// Master strength for everything, 0..1.
    pub fn set_gain(&mut self, g: f32) -> Result<()> {
        self.event(FF_GAIN, (g.clamp(0.0, 1.0) * 65535.0) as i32)
    }

    fn set_autocenter(&mut self, v: u16) -> Result<()> {
        // Most PID wheels ignore this (the V987 reports no autocenter control); harmless.
        let _ = self.event(FF_AUTOCENTER, v as i32);
        Ok(())
    }

    /// Pushes only the effects whose values changed noticeably since last time.
    pub fn apply(&mut self, f: &Forces) -> Result<()> {
        let prev = self.last.unwrap_or(Forces { spring: -1.0, damper: -1.0, constant: 9.0, texture: -1.0, engine: -1.0, ..Default::default() });
        let near = |a: f32, b: f32| (a - b).abs() < 0.01;

        if !near(f.spring, prev.spring) {
            let mut e = FfEffect::new(FF_SPRING, self.ids[0]);
            cond(&mut e, f.spring);
            self.upload(e)?;
        }
        if !near(f.damper, prev.damper) {
            let mut e = FfEffect::new(FF_DAMPER, self.ids[1]);
            cond(&mut e, f.damper);
            self.upload(e)?;
        }
        if !near(f.constant, prev.constant) {
            let mut e = FfEffect::new(FF_CONSTANT, self.ids[2]);
            let c = if self.invert { -f.constant } else { f.constant };
            e.put_i16(0, to_i16(c));
            self.upload(e)?;
        }
        let period_changed = |p: u16, q: u16| (p as f32 - q as f32).abs() > (q as f32 * 0.1).max(2.0);
        if !near(f.texture, prev.texture) || period_changed(f.texture_period_ms, prev.texture_period_ms) {
            self.periodic(3, f.texture, f.texture_period_ms)?;
        }
        if !near(f.engine, prev.engine) || period_changed(f.engine_period_ms, prev.engine_period_ms) {
            self.periodic(4, f.engine, f.engine_period_ms)?;
        }
        self.last = Some(*f);
        Ok(())
    }

    fn periodic(&mut self, slot: usize, amp: f32, period_ms: u16) -> Result<()> {
        let mut e = FfEffect::new(FF_PERIODIC, self.ids[slot]);
        e.put_u16(0, FF_SINE);
        e.put_u16(2, period_ms.max(5));
        e.put_i16(4, to_i16(amp));
        self.upload(e).map(|_| ())
    }

    /// Everything to zero: used when the game is paused, in a menu, or telemetry stops.
    pub fn idle(&mut self) -> Result<()> {
        self.apply(&Forces { texture_period_ms: 50, engine_period_ms: 40, ..Default::default() })
    }
}

/// Condition effect (spring / damper) with the same coefficient on both sides, full saturation.
fn cond(e: &mut FfEffect, coeff: f32) {
    let c = to_i16(coeff.clamp(0.0, 1.0));
    for axis in 0..2 {
        let o = axis * 12;
        e.put_u16(o, 0xFFFF); // right_saturation
        e.put_u16(o + 2, 0xFFFF); // left_saturation
        e.put_i16(o + 4, c); // right_coeff
        e.put_i16(o + 6, c); // left_coeff
        e.put_u16(o + 8, 0); // deadband
        e.put_i16(o + 10, 0); // center
    }
}

impl Drop for Wheel {
    fn drop(&mut self) {
        for id in self.ids {
            if id >= 0 {
                let _ = self.play(id, false);
                unsafe { libc::ioctl(self.file.as_raw_fd(), EVIOCRMFF as _, id as libc::c_int) };
            }
        }
    }
}
