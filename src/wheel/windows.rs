//! Windows force feedback through DirectInput 8.
//!
//! UNTESTED on real hardware: written against the DirectInput 8 documentation and the
//! `windows` crate bindings, cross-compiled from Linux. Mirrors `linux.rs`: five effects
//! (spring, damper, constant, two sines) are created once, started with infinite duration,
//! and then only their type-specific parameters are updated when values change.
//!
//! Devices are identified by a synthetic path `dinput:{instance-guid}` so that
//! `device = "dinput:{...}"` in the settings file selects one; "auto" picks the first
//! force-feedback game controller, LiteStar "FFB WHEEL" products first.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::{Context, Result, anyhow, bail};
use ::windows::Win32::Devices::HumanInterfaceDevice::*;
use ::windows::Win32::Foundation::{HINSTANCE, HWND};
use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
use ::windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WS_OVERLAPPED,
};
use ::windows::core::{BOOL, GUID, Interface, w};

use super::Forces;

/// `INFINITE` from winbase.h (kept local so the Win32_System_Threading feature isn't needed).
const INFINITE: u32 = 0xFFFF_FFFF;
/// `DIDFT_OPTIONAL` from dinput.h (not exported by the `windows` crate).
const DIDFT_OPTIONAL: u32 = 0x8000_0000;
/// `DIJOFS_X`: offset of `lX` in `DIJOYSTATE2`, i.e. the X axis in our data format.
const DIJOFS_X: u32 = 0;

/// Predefined DirectInput properties are not real GUIDs: dinput.h defines them as
/// `MAKEDIPROP(n)` = `(REFGUID)n`, a small integer cast to a pointer, and DirectInput checks
/// the pointer value itself. The `windows` crate exposes them as `GUID::from_u128(n)`, so
/// passing `&DIPROP_FFGAIN` would hand DirectInput a stack address and fail. Convert the
/// integer back into the pointer the API expects.
fn make_diprop(g: GUID) -> *const GUID {
    g.to_u128() as usize as *const GUID
}

fn hr<T>(r: ::windows::core::Result<T>, what: &str) -> Result<T> {
    r.map_err(|e| anyhow!("{what} failed: {e} (HRESULT 0x{:08X})", e.code().0 as u32))
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end]).trim().to_string()
}

fn path_of(guid: &GUID) -> PathBuf {
    PathBuf::from(format!("dinput:{{{guid:?}}}"))
}

/// Parses `dinput:{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}` (braces optional).
fn guid_of(path: &Path) -> Result<GUID> {
    let s = path.to_string_lossy();
    let rest = s.trim().strip_prefix("dinput:").ok_or_else(|| {
        anyhow!("device {s:?}: on Windows use \"auto\" or \"dinput:{{instance-guid}}\" (see `ffb-server test` output)")
    })?;
    let g = rest.trim().trim_start_matches('{').trim_end_matches('}').to_ascii_uppercase();
    GUID::try_from(g.as_str()).map_err(|_| anyhow!("device {s:?}: not a valid DirectInput instance GUID"))
}

fn dinput() -> Result<IDirectInput8W> {
    unsafe {
        let module = hr(GetModuleHandleW(None), "GetModuleHandleW")?;
        let mut raw: *mut c_void = ptr::null_mut();
        hr(
            DirectInput8Create(HINSTANCE::from(module), DIRECTINPUT_VERSION, &IDirectInput8W::IID, &mut raw, None),
            "DirectInput8Create",
        )?;
        if raw.is_null() {
            bail!("DirectInput8Create returned no interface");
        }
        // DirectInput8Create hands back an owned reference to the requested IID.
        Ok(IDirectInput8W::from_raw(raw))
    }
}

struct Found {
    guid: GUID,
    product: String,
}

unsafe extern "system" fn enum_cb(inst: *mut DIDEVICEINSTANCEW, ctx: *mut c_void) -> BOOL {
    // SAFETY: EnumDevices passes back the `ctx` we gave it (a live &mut Vec<Found>) and a valid
    // DIDEVICEINSTANCEW for the duration of the call.
    unsafe {
        if let (Some(inst), Some(out)) = (inst.as_ref(), (ctx as *mut Vec<Found>).as_mut()) {
            out.push(Found { guid: inst.guidInstance, product: wide_to_string(&inst.tszProductName) });
        }
    }
    BOOL(DIENUM_CONTINUE as i32)
}

fn enumerate(di: &IDirectInput8W) -> Result<Vec<Found>> {
    let mut v: Vec<Found> = Vec::new();
    unsafe {
        hr(
            di.EnumDevices(
                DI8DEVCLASS_GAMECTRL,
                Some(enum_cb),
                &mut v as *mut Vec<Found> as *mut c_void,
                DIEDFL_ATTACHEDONLY | DIEDFL_FORCEFEEDBACK,
            ),
            "EnumDevices",
        )?;
    }
    // LiteStar wheels ("FFB WHEEL") first; stable otherwise.
    v.sort_by_key(|f| !f.product.to_ascii_uppercase().contains("FFB WHEEL"));
    Ok(v)
}

/// A hidden message-only window: SetCooperativeLevel needs an HWND, and exclusive access
/// (required for force feedback) needs one even in the background. Same approach as SDL's
/// DirectInput haptic backend (a HWND_MESSAGE helper window with DISCL_EXCLUSIVE | DISCL_BACKGROUND).
struct HiddenWindow(HWND);

impl HiddenWindow {
    fn new() -> Result<Self> {
        unsafe {
            let module = hr(GetModuleHandleW(None), "GetModuleHandleW")?;
            // The predefined "STATIC" class needs no registration.
            let hwnd = hr(
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("ffb-server DirectInput"),
                    WS_OVERLAPPED,
                    0,
                    0,
                    0,
                    0,
                    Some(HWND_MESSAGE),
                    None,
                    Some(HINSTANCE::from(module)),
                    None,
                ),
                "CreateWindowExW",
            )?;
            Ok(Self(hwnd))
        }
    }
}

impl Drop for HiddenWindow {
    fn drop(&mut self) {
        // DestroyWindow must run on the creating thread; Wheel is !Send (HWND and the COM
        // interfaces hold raw pointers), so it never leaves the thread that opened it.
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

/// `c_dfDIJoystick2` rebuilt by hand: the `windows` crate does not export that data symbol
/// (it lives in dinput8.lib's static part, not in the DLL). Same 164 objects, same offsets
/// into DIJOYSTATE2 (272 bytes), every object DIDFT_OPTIONAL, DIDF_ABSAXIS, as in dinput.h /
/// Wine's data_formats.c. We never read state; the format exists because DirectInput requires
/// one before Acquire and because DIEFF_OBJECTOFFSETS axis offsets (DIJOFS_X = 0) refer to it.
struct DataFormat {
    objects: Vec<DIOBJECTDATAFORMAT>,
    format: DIDATAFORMAT,
}

static AXIS_GUIDS: [GUID; 8] = [
    GUID_XAxis, GUID_YAxis, GUID_ZAxis, GUID_RxAxis, GUID_RyAxis, GUID_RzAxis, GUID_Slider, GUID_Slider,
];
static POV_GUID: GUID = GUID_POV;
const DIJOYSTATE2_SIZE: u32 = 272;
const _: () = assert!(std::mem::size_of::<DIJOYSTATE2>() == DIJOYSTATE2_SIZE as usize);

impl DataFormat {
    fn joystick2() -> Box<Self> {
        let axis = DIDFT_OPTIONAL | DIDFT_AXIS | DIDFT_ANYINSTANCE;
        let mut o: Vec<DIOBJECTDATAFORMAT> = Vec::with_capacity(164);
        let axes = |o: &mut Vec<DIOBJECTDATAFORMAT>, base: u32, aspect: u32| {
            // lX lY lZ lRx lRy lRz rglSlider[0] rglSlider[1]
            for (i, g) in AXIS_GUIDS.iter().enumerate() {
                o.push(DIOBJECTDATAFORMAT { pguid: g, dwOfs: base + 4 * i as u32, dwType: axis, dwFlags: aspect });
            }
        };
        axes(&mut o, 0, DIDOI_ASPECTPOSITION);
        for i in 0..4u32 {
            o.push(DIOBJECTDATAFORMAT {
                pguid: &POV_GUID,
                dwOfs: 32 + 4 * i,
                dwType: DIDFT_OPTIONAL | DIDFT_POV | DIDFT_ANYINSTANCE,
                dwFlags: 0,
            });
        }
        for i in 0..128u32 {
            o.push(DIOBJECTDATAFORMAT {
                pguid: ptr::null(),
                dwOfs: 48 + i,
                dwType: DIDFT_OPTIONAL | DIDFT_BUTTON | DIDFT_ANYINSTANCE,
                dwFlags: 0,
            });
        }
        axes(&mut o, 176, DIDOI_ASPECTVELOCITY);
        axes(&mut o, 208, DIDOI_ASPECTACCEL);
        axes(&mut o, 240, DIDOI_ASPECTFORCE);
        debug_assert_eq!(o.len(), 164);

        let mut b = Box::new(Self { objects: o, format: DIDATAFORMAT::default() });
        b.format = DIDATAFORMAT {
            dwSize: std::mem::size_of::<DIDATAFORMAT>() as u32,
            dwObjSize: std::mem::size_of::<DIOBJECTDATAFORMAT>() as u32,
            dwFlags: DIDF_ABSAXIS,
            dwDataSize: DIJOYSTATE2_SIZE,
            dwNumObjs: b.objects.len() as u32,
            rgodf: b.objects.as_mut_ptr(),
        };
        b
    }
}

/// Type-specific parameters for one effect.
enum Params {
    Condition(DICONDITION),
    Constant(DICONSTANTFORCE),
    Periodic(DIPERIODIC),
}

impl Params {
    fn raw(&mut self) -> (u32, *mut c_void) {
        match self {
            Params::Condition(c) => (std::mem::size_of::<DICONDITION>() as u32, c as *mut _ as *mut c_void),
            Params::Constant(c) => (std::mem::size_of::<DICONSTANTFORCE>() as u32, c as *mut _ as *mut c_void),
            Params::Periodic(p) => (std::mem::size_of::<DIPERIODIC>() as u32, p as *mut _ as *mut c_void),
        }
    }
}

fn to_di(x: f32) -> i32 {
    (x.clamp(-1.0, 1.0) * DI_FFNOMINALMAX as f32).round() as i32
}

/// Spring / damper with the same coefficient on both sides and full saturation.
fn cond(coeff: f32) -> Params {
    let c = to_di(coeff.clamp(0.0, 1.0));
    Params::Condition(DICONDITION {
        lOffset: 0,
        lPositiveCoefficient: c,
        lNegativeCoefficient: c,
        dwPositiveSaturation: DI_FFNOMINALMAX,
        dwNegativeSaturation: DI_FFNOMINALMAX,
        lDeadBand: 0,
    })
}

fn sine(amp: f32, period_ms: u16) -> Params {
    Params::Periodic(DIPERIODIC {
        dwMagnitude: to_di(amp.clamp(0.0, 1.0)) as u32,
        lOffset: 0,
        dwPhase: 0,
        dwPeriod: period_ms.max(5) as u32 * 1000, // microseconds
    })
}

/// Runs `f` with a fully filled single-X-axis DIEFFECT. The axis/direction arrays and the
/// type-specific block live on this stack frame, which outlives the DirectInput call.
fn with_effect<T>(p: &mut Params, f: impl FnOnce(*mut DIEFFECT) -> T) -> T {
    let mut axes = [DIJOFS_X];
    let mut dir = [0i32]; // single axis: direction comes from the sign of the force
    let (cb, lp) = p.raw();
    let mut e = DIEFFECT {
        dwSize: std::mem::size_of::<DIEFFECT>() as u32,
        dwFlags: DIEFF_CARTESIAN | DIEFF_OBJECTOFFSETS,
        dwDuration: INFINITE,
        dwSamplePeriod: 0,
        dwGain: DI_FFNOMINALMAX,
        dwTriggerButton: DIEB_NOTRIGGER,
        dwTriggerRepeatInterval: 0,
        cAxes: 1,
        rgdwAxes: axes.as_mut_ptr(),
        rglDirection: dir.as_mut_ptr(),
        lpEnvelope: ptr::null_mut(),
        cbTypeSpecificParams: cb,
        lpvTypeSpecificParams: lp,
        dwStartDelay: 0,
    };
    f(&mut e)
}

pub struct Wheel {
    // Field order is drop order: effects, then the device, then DirectInput, then the window.
    effects: Vec<IDirectInputEffect>, // spring, damper, constant, texture, engine
    device: IDirectInputDevice8W,
    _di: IDirectInput8W,
    _format: Box<DataFormat>,
    _window: HiddenWindow,

    name: String,
    last: Option<Forces>,
    invert: bool,
}

impl Wheel {
    /// Attached force-feedback game controllers as `dinput:{guid}` paths, "FFB WHEEL" first.
    pub fn candidates() -> Result<Vec<PathBuf>> {
        let di = dinput()?;
        Ok(enumerate(&di)?.iter().map(|f| path_of(&f.guid)).collect())
    }

    /// Opens `device` ("auto" or `dinput:{instance-guid}`). With "auto", the first
    /// force-feedback controller that accepts our effects wins.
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
        Err(last.unwrap_or_else(|| anyhow!("no force-feedback wheel found (is it plugged in?)")))
    }

    /// The product name DirectInput reports for the device.
    pub fn name(&self) -> String {
        self.name.clone()
    }

    pub fn set_invert(&mut self, invert: bool) {
        if self.invert != invert {
            self.invert = invert;
            self.last = None; // re-send everything with the new sign
        }
    }

    pub fn open(path: &Path, invert: bool) -> Result<Self> {
        let guid = guid_of(path)?;
        let window = HiddenWindow::new()?;
        let di = dinput()?;
        let format = DataFormat::joystick2();
        let ctx = || format!("opening {}", path.display());
        unsafe {
            let mut dev: Option<IDirectInputDevice8W> = None;
            hr(di.CreateDevice(&guid, &mut dev, None), "CreateDevice").with_context(ctx)?;
            let device = dev.ok_or_else(|| anyhow!("CreateDevice returned no device")).with_context(ctx)?;

            let mut inst = DIDEVICEINSTANCEW { dwSize: std::mem::size_of::<DIDEVICEINSTANCEW>() as u32, ..Default::default() };
            let name = match device.GetDeviceInfo(&mut inst) {
                Ok(()) => wide_to_string(&inst.tszProductName),
                Err(_) => String::new(),
            };

            hr(device.SetDataFormat(&format.format as *const _ as *mut DIDATAFORMAT), "SetDataFormat").with_context(ctx)?;
            hr(device.SetCooperativeLevel(window.0, DISCL_EXCLUSIVE | DISCL_BACKGROUND), "SetCooperativeLevel")
                .with_context(ctx)?;
            // Most PID wheels ignore autocenter; set before Acquire as in the DirectX FFConst sample.
            let _ = set_dword(&device, DIPROP_AUTOCENTER, DIPROPAUTOCENTER_OFF);
            hr(device.Acquire(), "Acquire (another program may hold the wheel exclusively)").with_context(ctx)?;

            let mut w = Self {
                effects: Vec::with_capacity(5),
                device,
                _di: di,
                _format: format,
                _window: window,
                name,
                last: None,
                invert,
            };
            w.create(&GUID_Spring, cond(0.0)).with_context(ctx)?;
            w.create(&GUID_Damper, cond(0.0)).with_context(ctx)?;
            w.create(&GUID_ConstantForce, Params::Constant(DICONSTANTFORCE { lMagnitude: 0 })).with_context(ctx)?;
            w.create(&GUID_Sine, sine(0.0, 50)).with_context(ctx)?;
            w.create(&GUID_Sine, sine(0.0, 50)).with_context(ctx)?;
            for e in &w.effects {
                hr(e.Start(1, 0), "IDirectInputEffect::Start").with_context(ctx)?;
            }
            Ok(w)
        }
    }

    fn create(&mut self, kind: &GUID, mut p: Params) -> Result<()> {
        let mut out: Option<IDirectInputEffect> = None;
        let device = &self.device;
        unsafe {
            with_effect(&mut p, |e| hr(device.CreateEffect(kind, e, &mut out, None), "CreateEffect"))?;
        }
        self.effects.push(out.ok_or_else(|| anyhow!("CreateEffect returned no effect"))?);
        Ok(())
    }

    /// Updates one effect's magnitude/coefficients in place. If the device dropped our
    /// acquisition (DIERR_NOTEXCLUSIVEACQUIRED / DIERR_INPUTLOST, e.g. after another program
    /// grabbed it or the device reset), re-acquire once and re-download + restart the effect.
    fn update(&mut self, slot: usize, mut p: Params) -> Result<()> {
        let eff = &self.effects[slot];
        unsafe {
            let first = with_effect(&mut p, |e| eff.SetParameters(e, DIEP_TYPESPECIFICPARAMS));
            if first.is_ok() {
                return Ok(());
            }
            let _ = self.device.Acquire();
            hr(
                with_effect(&mut p, |e| eff.SetParameters(e, DIEP_TYPESPECIFICPARAMS | DIEP_START)),
                "IDirectInputEffect::SetParameters",
            )
        }
    }

    /// Master strength for everything, 0..1 (DIPROP_FFGAIN, 0..10000).
    pub fn set_gain(&mut self, g: f32) -> Result<()> {
        let v = (g.clamp(0.0, 1.0) * DI_FFNOMINALMAX as f32).round() as u32;
        unsafe { set_dword(&self.device, DIPROP_FFGAIN, v) }
    }

    /// Pushes only the effects whose values changed noticeably since last time.
    pub fn apply(&mut self, f: &Forces) -> Result<()> {
        let prev = self.last.unwrap_or(Forces { spring: -1.0, damper: -1.0, constant: 9.0, texture: -1.0, engine: -1.0, ..Default::default() });
        let near = |a: f32, b: f32| (a - b).abs() < 0.01;

        if !near(f.spring, prev.spring) {
            self.update(0, cond(f.spring))?;
        }
        if !near(f.damper, prev.damper) {
            self.update(1, cond(f.damper))?;
        }
        if !near(f.constant, prev.constant) {
            let c = if self.invert { -f.constant } else { f.constant };
            self.update(2, Params::Constant(DICONSTANTFORCE { lMagnitude: to_di(c) }))?;
        }
        let period_changed = |p: u16, q: u16| (p as f32 - q as f32).abs() > (q as f32 * 0.1).max(2.0);
        if !near(f.texture, prev.texture) || period_changed(f.texture_period_ms, prev.texture_period_ms) {
            self.update(3, sine(f.texture, f.texture_period_ms))?;
        }
        if !near(f.engine, prev.engine) || period_changed(f.engine_period_ms, prev.engine_period_ms) {
            self.update(4, sine(f.engine, f.engine_period_ms))?;
        }
        self.last = Some(*f);
        Ok(())
    }

    /// Everything to zero: used when the game is paused, in a menu, or telemetry stops.
    pub fn idle(&mut self) -> Result<()> {
        self.apply(&Forces { texture_period_ms: 50, engine_period_ms: 40, ..Default::default() })
    }
}

/// Sets a device-wide DWORD property (DIPH_DEVICE).
unsafe fn set_dword(device: &IDirectInputDevice8W, prop: GUID, value: u32) -> Result<()> {
    let mut p = DIPROPDWORD {
        diph: DIPROPHEADER {
            dwSize: std::mem::size_of::<DIPROPDWORD>() as u32,
            dwHeaderSize: std::mem::size_of::<DIPROPHEADER>() as u32,
            dwObj: 0,
            dwHow: DIPH_DEVICE,
        },
        dwData: value,
    };
    unsafe { hr(device.SetProperty(make_diprop(prop), &mut p.diph), "SetProperty") }
}

impl Drop for Wheel {
    fn drop(&mut self) {
        unsafe {
            for e in &self.effects {
                let _ = e.Stop();
                let _ = e.Unload();
            }
            self.effects.clear(); // release the effect objects before the device
            let _ = self.device.Unacquire();
        }
    }
}
