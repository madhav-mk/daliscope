//! Commissioning operations built on raw DALI frames: device discovery,
//! random-address addressing, parameter read/write and memory banks.
//!
//! Everything here goes through [`DaliBus`], so it runs against the real
//! interface and the simulator alike, and every frame shows up in the monitor.

use crate::dongle::{Dongle, DongleError, FrameResult, Result};
use crate::luba::TxMode;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

pub trait DaliBus {
    fn send(&self, bits: u8, value: u32, twice: bool) -> Result<()>;
    fn query(&self, bits: u8, value: u32) -> Result<FrameResult>;
    fn note(&self, _text: &str) {}
}

impl DaliBus for Dongle {
    fn send(&self, bits: u8, value: u32, twice: bool) -> Result<()> {
        let mode = if twice { TxMode::twice() } else { TxMode::plain() };
        self.transact(bits, value, mode).map(|_| ())
    }
    fn query(&self, bits: u8, value: u32) -> Result<FrameResult> {
        // One retry on a transmit-side failure (collision with another master).
        match self.transact(bits, value, TxMode::query()) {
            Err(DongleError::Bus(_)) => self.transact(bits, value, TxMode::query()),
            r => r,
        }
    }
    fn note(&self, text: &str) {
        Dongle::note(self, text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Proto {
    Gear,
    Device,
}

impl Proto {
    pub fn bits(self) -> u8 {
        match self {
            Proto::Gear => 16,
            Proto::Device => 24,
        }
    }
    pub fn prefix(self) -> &'static str {
        match self {
            Proto::Gear => "A",
            Proto::Device => "D",
        }
    }
}

// ---------------------------------------------------------------------------
// Frame builders
// ---------------------------------------------------------------------------

pub mod gear {
    pub fn short(a: u8) -> u8 {
        (a << 1) | 1
    }
    pub fn group(g: u8) -> u8 {
        0x80 | (g << 1) | 1
    }
    pub const BROADCAST: u8 = 0xFF;
    pub fn cmd(addr: u8, op: u8) -> u32 {
        ((addr as u32) << 8) | op as u32
    }
    pub fn dapc(addr: u8, level: u8) -> u32 {
        (((addr & 0xFE) as u32) << 8) | level as u32
    }
    pub fn special(a: u8, d: u8) -> u32 {
        ((a as u32) << 8) | d as u32
    }
    pub const DTR0: u8 = 0xA3;
    pub const DTR1: u8 = 0xC3;
    pub const DTR2: u8 = 0xC5;
    pub const ENABLE_DT: u8 = 0xC1;
}

pub mod dev {
    pub fn short(a: u8) -> u8 {
        (a << 1) | 1
    }
    pub fn group(g: u8) -> u8 {
        0x80 | (g << 1) | 1
    }
    pub const BROADCAST: u8 = 0xFF;
    pub const DEVICE: u8 = 0xFE;
    pub fn cmd(addr: u8, inst: u8, op: u8) -> u32 {
        ((addr as u32) << 16) | ((inst as u32) << 8) | op as u32
    }
    pub fn special(i: u8, d: u8) -> u32 {
        (0xC1 << 16) | ((i as u32) << 8) | d as u32
    }
    pub const DTR0: u8 = 0x30;
    pub const DTR1: u8 = 0x31;
    pub const DTR2: u8 = 0x32;
}

fn ans(r: FrameResult) -> Option<u8> {
    r.value()
}

// ---------------------------------------------------------------------------
// Addressing primitives shared by 16-bit and 24-bit
// ---------------------------------------------------------------------------

struct Special {
    bits: u8,
}

impl Special {
    fn frame(&self, which: Sp, d: u8) -> u32 {
        if self.bits == 16 {
            let a = match which {
                Sp::Terminate => 0xA1,
                Sp::Initialise => 0xA5,
                Sp::Randomise => 0xA7,
                Sp::Compare => 0xA9,
                Sp::Withdraw => 0xAB,
                Sp::SearchH => 0xB1,
                Sp::SearchM => 0xB3,
                Sp::SearchL => 0xB5,
                Sp::Program => 0xB7,
                Sp::Verify => 0xB9,
                Sp::QueryShort => 0xBB,
                Sp::Dtr0 => 0xA3,
                Sp::Dtr1 => 0xC3,
                Sp::Dtr2 => 0xC5,
            };
            gear::special(a, d)
        } else {
            let i = match which {
                Sp::Terminate => 0x00,
                Sp::Initialise => 0x01,
                Sp::Randomise => 0x02,
                Sp::Compare => 0x03,
                Sp::Withdraw => 0x04,
                Sp::SearchH => 0x05,
                Sp::SearchM => 0x06,
                Sp::SearchL => 0x07,
                Sp::Program => 0x08,
                Sp::Verify => 0x09,
                Sp::QueryShort => 0x0A,
                Sp::Dtr0 => 0x30,
                Sp::Dtr1 => 0x31,
                Sp::Dtr2 => 0x32,
            };
            dev::special(i, d)
        }
    }
    /// Encoded short-address argument for PROGRAM/VERIFY/SET SHORT ADDRESS.
    fn addr_arg(&self, a: u8) -> u8 {
        if self.bits == 16 {
            (a << 1) | 1
        } else {
            a
        }
    }
    fn init_arg(&self, scope: InitScope) -> u8 {
        match (self.bits, scope) {
            (16, InitScope::All) => 0x00,
            (16, InitScope::Unaddressed) => 0xFF,
            (_, InitScope::All) => 0xFF,
            (_, InitScope::Unaddressed) => 0x7F,
        }
    }
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
enum Sp {
    Terminate,
    Initialise,
    Randomise,
    Compare,
    Withdraw,
    SearchH,
    SearchM,
    SearchL,
    Program,
    Verify,
    QueryShort,
    Dtr0,
    Dtr1,
    Dtr2,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InitScope {
    All,
    Unaddressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressingMode {
    /// Delete all short addresses, then address everything.
    NewInstallation,
    /// Keep existing addresses, address only unaddressed devices.
    Extension,
}

#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub phase: String,
    pub message: String,
    pub fraction: f32,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AddressingReport {
    pub proto: Option<Proto>,
    pub existing: Vec<u8>,
    pub assigned: Vec<(u8, u32)>,
    pub failures: Vec<String>,
    pub out_of_addresses: bool,
    pub cancelled: bool,
}

pub struct Ctx<'a> {
    pub bus: &'a dyn DaliBus,
    pub cancel: &'a AtomicBool,
    pub progress: &'a dyn Fn(Progress),
}

impl Ctx<'_> {
    fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(DongleError::Other("cancelled".into()))
        } else {
            Ok(())
        }
    }
    fn report(&self, phase: &str, message: impl Into<String>, fraction: f32) {
        (self.progress)(Progress { phase: phase.into(), message: message.into(), fraction });
    }
}

/// Is anything with this short address present?
pub fn present(bus: &dyn DaliBus, proto: Proto, a: u8) -> Result<FrameResult> {
    match proto {
        Proto::Gear => bus.query(16, gear::cmd(gear::short(a), 0x91)),
        Proto::Device => bus.query(24, dev::cmd(dev::short(a), dev::DEVICE, 0x30)),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanEntry {
    pub proto: Proto,
    pub address: u8,
    /// Several devices answered at once: duplicate short address.
    pub duplicate: bool,
    /// Gear: device types (DT). Devices: instance types.
    pub types: Vec<u8>,
}

pub fn scan(ctx: &Ctx, proto: Proto) -> Result<Vec<ScanEntry>> {
    scan_range(ctx, proto, 0.0, 1.0)
}

/// Scan all 64 short addresses, reporting progress within `[start, start + span]`.
pub fn scan_range(ctx: &Ctx, proto: Proto, start: f32, span: f32) -> Result<Vec<ScanEntry>> {
    let mut out = Vec::new();
    let bus = ctx.bus;
    ctx.bus.note(&format!("Scanning {} addresses", if proto == Proto::Gear { "control gear" } else { "control device" }));
    for a in 0..64u8 {
        ctx.check()?;
        ctx.report("scan", format!("Scanning {}{a}", proto.prefix()), start + span * (a as f32 / 64.0));
        let duplicate = match present(bus, proto, a)? {
            FrameResult::Answer(_) => false,
            FrameResult::Collision => true,
            _ => continue,
        };
        // A light identification so the list can say what each address is.
        let mut types = Vec::new();
        if !duplicate {
            match proto {
                Proto::Gear => match ans(bus.query(16, gear::cmd(gear::short(a), 0x99))?) {
                    Some(0xFF) => {
                        for _ in 0..8 {
                            match ans(bus.query(16, gear::cmd(gear::short(a), 0xA7))?) {
                                Some(254) | None => break,
                                Some(t) => types.push(t),
                            }
                        }
                    }
                    Some(t) => types.push(t),
                    None => {}
                },
                Proto::Device => {
                    let n = ans(bus.query(24, dev::cmd(dev::short(a), dev::DEVICE, 0x35))?).unwrap_or(0);
                    for i in 0..n.min(8) {
                        if let Some(t) = ans(bus.query(24, dev::cmd(dev::short(a), i, 0x80))?) {
                            types.push(t);
                        }
                    }
                }
            }
        }
        out.push(ScanEntry { proto, address: a, duplicate, types });
    }
    Ok(out)
}

/// IEC 62386 random-address binary search. Returns the assigned addresses.
pub fn address(ctx: &Ctx, proto: Proto, mode: AddressingMode, reserved: &[u8]) -> Result<AddressingReport> {
    let sp = Special { bits: proto.bits() };
    let bits = proto.bits();
    let bus = ctx.bus;
    let mut rep = AddressingReport { proto: Some(proto), ..Default::default() };
    let name = if proto == Proto::Gear { "control gear" } else { "control devices" };

    let mut used: Vec<u8> = reserved.to_vec();
    match mode {
        AddressingMode::NewInstallation => {
            ctx.report("prepare", format!("Deleting all {name} short addresses"), 0.0);
            bus.note(&format!("Addressing {name}: new installation (delete all addresses)"));
            bus.send(bits, sp.frame(Sp::Dtr0, 0xFF), false)?;
            let del = match proto {
                Proto::Gear => gear::cmd(gear::BROADCAST, 0x80),
                Proto::Device => dev::cmd(dev::BROADCAST, dev::DEVICE, 0x14),
            };
            bus.send(bits, del, true)?;
        }
        AddressingMode::Extension => {
            bus.note(&format!("Addressing {name}: system extension"));
            for a in 0..64u8 {
                ctx.check()?;
                ctx.report("prepare", format!("Checking {}{a}", proto.prefix()), a as f32 / 640.0);
                if present(bus, proto, a)?.yes() {
                    used.push(a);
                    rep.existing.push(a);
                }
            }
        }
    }
    let scope = if mode == AddressingMode::NewInstallation { InitScope::All } else { InitScope::Unaddressed };
    ctx.check()?;
    bus.send(bits, sp.frame(Sp::Initialise, sp.init_arg(scope)), true)?;
    bus.send(bits, sp.frame(Sp::Randomise, 0), true)?;
    thread::sleep(Duration::from_millis(120));

    let compare = || -> Result<bool> { Ok(bus.query(bits, sp.frame(Sp::Compare, 0))?.yes()) };
    let mut cur: Option<u32> = None;
    let mut set_search = |v: u32| -> Result<()> {
        let parts = [(Sp::SearchH, 16), (Sp::SearchM, 8), (Sp::SearchL, 0)];
        for (s, sh) in parts {
            let b = (v >> sh) as u8;
            if cur.map(|c| (c >> sh) as u8) != Some(b) {
                bus.send(bits, sp.frame(s, b), false)?;
            }
        }
        cur = Some(v);
        Ok(())
    };

    let result = (|| -> Result<()> {
        let mut found = 0;
        loop {
            ctx.check()?;
            set_search(0xFF_FFFF)?;
            if !compare()? {
                break;
            }
            let (mut lo, mut hi) = (0u32, 0xFF_FFFFu32);
            while lo < hi {
                ctx.check()?;
                let mid = lo + (hi - lo) / 2;
                set_search(mid)?;
                if compare()? {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            set_search(lo)?;
            let Some(next) = (0..64u8).find(|a| !used.contains(a)) else {
                rep.out_of_addresses = true;
                bus.note("No free short addresses left");
                break;
            };
            bus.send(bits, sp.frame(Sp::Program, sp.addr_arg(next)), false)?;
            let ok = bus.query(bits, sp.frame(Sp::Verify, sp.addr_arg(next)))?.yes();
            if ok {
                used.push(next);
                rep.assigned.push((next, lo));
                found += 1;
                ctx.report("address", format!("{}{next} ← random 0x{lo:06X}", proto.prefix()), (found as f32 / 8.0).min(0.95));
                bus.note(&format!("Assigned {}{next} to random address 0x{lo:06X}", proto.prefix()));
            } else {
                rep.failures.push(format!("verify failed for random 0x{lo:06X}"));
                bus.note(&format!("Verify failed for random 0x{lo:06X}"));
            }
            bus.send(bits, sp.frame(Sp::Withdraw, 0), false)?;
        }
        Ok(())
    })();
    let _ = bus.send(bits, sp.frame(Sp::Terminate, 0), false);
    match result {
        Ok(()) => {}
        Err(DongleError::Other(m)) if m == "cancelled" => rep.cancelled = true,
        Err(e) => return Err(e),
    }
    ctx.report("done", format!("{} new {name}", rep.assigned.len()), 1.0);
    Ok(rep)
}

pub fn delete_address(bus: &dyn DaliBus, proto: Proto, a: u8) -> Result<()> {
    set_short_address(bus, proto, a, None)
}

/// Change (or delete with `None`) a short address via DTR0 + SET SHORT ADDRESS.
pub fn set_short_address(bus: &dyn DaliBus, proto: Proto, a: u8, new: Option<u8>) -> Result<()> {
    if let Some(n) = new {
        if n > 63 {
            return Err(DongleError::Other("address must be 0-63".into()));
        }
        if n != a && present(bus, proto, n)?.yes() {
            return Err(DongleError::Other(format!("{}{n} is already in use", proto.prefix())));
        }
    }
    match proto {
        Proto::Gear => {
            bus.send(16, gear::special(gear::DTR0, new.map(gear::short).unwrap_or(0xFF)), false)?;
            bus.send(16, gear::cmd(gear::short(a), 0x80), true)
        }
        Proto::Device => {
            bus.send(24, dev::special(dev::DTR0, new.unwrap_or(0xFF)), false)?;
            bus.send(24, dev::cmd(dev::short(a), dev::DEVICE, 0x14), true)
        }
    }
}

pub fn identify(bus: &dyn DaliBus, proto: Proto, a: u8) -> Result<()> {
    match proto {
        Proto::Gear => bus.send(16, gear::cmd(gear::short(a), 0x25), true),
        Proto::Device => bus.send(24, dev::cmd(dev::short(a), dev::DEVICE, 0x00), true),
    }
}

// ---------------------------------------------------------------------------
// Memory banks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Default)]
pub struct Bank0 {
    pub last_address: u8,
    pub last_bank: u8,
    pub gtin: Option<u64>,
    pub firmware: Option<(u8, u8)>,
    #[serde(serialize_with = "crate::luba::ser_opt_hex_u64")]
    pub identification: Option<u64>,
    pub hardware: Option<(u8, u8)>,
    pub version_101: Option<u8>,
    pub version_102: Option<u8>,
    pub version_103: Option<u8>,
    pub device_units: Option<u8>,
    pub gear_units: Option<u8>,
    pub unit_index: Option<u8>,
}

impl Bank0 {
    pub fn parse(b: &[Option<u8>]) -> Self {
        let get = |i: usize| b.get(i).copied().flatten();
        let many = |s: usize, n: usize| -> Option<u64> {
            (s..s + n).map(get).try_fold(0u64, |acc, x| x.map(|v| (acc << 8) | v as u64))
        };
        Self {
            last_address: get(0).unwrap_or(0),
            last_bank: get(2).unwrap_or(0),
            gtin: many(3, 6),
            firmware: get(9).zip(get(10)),
            identification: many(0x0B, 8),
            hardware: get(0x13).zip(get(0x14)),
            version_101: get(0x15),
            version_102: get(0x16),
            version_103: get(0x17),
            device_units: get(0x18),
            gear_units: get(0x19),
            unit_index: get(0x1A),
        }
    }
}

/// Read a memory bank with DTR1 = bank, DTR0 = 0 and repeated
/// READ MEMORY LOCATION (which auto-increments DTR0). Unanswered cells are None.
pub fn read_bank(bus: &dyn DaliBus, proto: Proto, a: u8, bank: u8, max_len: Option<u8>) -> Result<Vec<Option<u8>>> {
    let (bits, dtr1, dtr0, read) = match proto {
        Proto::Gear => (16, gear::special(gear::DTR1, bank), gear::special(gear::DTR0, 0), gear::cmd(gear::short(a), 0xC5)),
        Proto::Device => (24, dev::special(dev::DTR1, bank), dev::special(dev::DTR0, 0), dev::cmd(dev::short(a), dev::DEVICE, 0x3C)),
    };
    bus.send(bits, dtr1, false)?;
    bus.send(bits, dtr0, false)?;
    let Some(last) = ans(bus.query(bits, read)?) else {
        return Ok(Vec::new());
    };
    let last = max_len.map(|m| m.min(last)).unwrap_or(last);
    let mut out = vec![Some(last)];
    for _ in 1..=last {
        out.push(ans(bus.query(bits, read)?));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Control gear (102)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Default)]
pub struct GearInfo {
    pub address: u8,
    pub device_types: Vec<u8>,
    pub status: Option<u8>,
    pub version: Option<u8>,
    pub actual_level: Option<u8>,
    pub max_level: Option<u8>,
    pub min_level: Option<u8>,
    pub physical_min: Option<u8>,
    pub power_on_level: Option<u8>,
    pub system_failure_level: Option<u8>,
    pub fade_time: Option<u8>,
    pub fade_rate: Option<u8>,
    pub extended_fade: Option<u8>,
    pub operating_mode: Option<u8>,
    pub light_source_type: Option<u8>,
    pub groups: Option<u16>,
    pub scenes: Vec<Option<u8>>,
    pub random_address: Option<u32>,
    pub dt6_dimming_curve: Option<u8>,
    pub dt6_gear_type: Option<u8>,
    pub dt8_colour_type_features: Option<u8>,
    pub dt8_colour_temperature: Option<u16>,
    pub bank0: Option<Bank0>,
}

pub fn read_gear(ctx: &Ctx, a: u8, with_bank0: bool) -> Result<GearInfo> {
    let bus = ctx.bus;
    let q = |op: u8| -> Result<Option<u8>> { Ok(ans(bus.query(16, gear::cmd(gear::short(a), op))?)) };
    let mut g = GearInfo { address: a, ..Default::default() };
    ctx.report("read", format!("A{a}: basic"), 0.0);
    g.status = q(0x90)?;
    g.version = q(0x97)?;
    match q(0x99)? {
        Some(0xFF) => {
            for _ in 0..16 {
                match q(0xA7)? {
                    Some(254) | None => break,
                    Some(t) => g.device_types.push(t),
                }
            }
        }
        Some(t) => g.device_types.push(t),
        None => {}
    }
    g.actual_level = q(0xA0)?;
    g.max_level = q(0xA1)?;
    g.min_level = q(0xA2)?;
    g.physical_min = q(0x9A)?;
    g.power_on_level = q(0xA3)?;
    g.system_failure_level = q(0xA4)?;
    if let Some(v) = q(0xA5)? {
        g.fade_time = Some(v >> 4);
        g.fade_rate = Some(v & 0x0F);
    }
    g.extended_fade = q(0xA8)?;
    g.operating_mode = q(0x9E)?;
    g.light_source_type = q(0x9F)?;
    ctx.check()?;
    ctx.report("read", format!("A{a}: groups & scenes"), 0.3);
    if let (Some(lo), Some(hi)) = (q(0xC0)?, q(0xC1)?) {
        g.groups = Some((hi as u16) << 8 | lo as u16);
    }
    for s in 0..16u8 {
        ctx.check()?;
        let v = q(0xB0 + s)?;
        g.scenes.push(v.filter(|&x| x != 255));
    }
    if let (Some(h), Some(m), Some(l)) = (q(0xC2)?, q(0xC3)?, q(0xC4)?) {
        g.random_address = Some((h as u32) << 16 | (m as u32) << 8 | l as u32);
    }
    let dtq = |dt: u8, op: u8| -> Result<Option<u8>> {
        bus.send(16, gear::special(gear::ENABLE_DT, dt), false)?;
        Ok(ans(bus.query(16, gear::cmd(gear::short(a), op))?))
    };
    if g.device_types.contains(&6) {
        g.dt6_gear_type = dtq(6, 0xED)?;
        g.dt6_dimming_curve = dtq(6, 0xEE)?;
    }
    if g.device_types.contains(&8) {
        g.dt8_colour_type_features = dtq(8, 0xF9)?;
        // QUERY COLOUR VALUE with DTR0 = 2 (colour temperature Tc): MSB answered, LSB via DTR0.
        bus.send(16, gear::special(gear::DTR0, 2), false)?;
        if let Some(msb) = dtq(8, 0xFA)? {
            let lsb = ans(bus.query(16, gear::cmd(gear::short(a), 0x98))?).unwrap_or(0);
            let tc = (msb as u16) << 8 | lsb as u16;
            if tc != 0xFFFF {
                g.dt8_colour_temperature = Some(tc);
            }
        }
    }
    if with_bank0 {
        ctx.check()?;
        ctx.report("read", format!("A{a}: memory bank 0"), 0.8);
        let b = read_bank(bus, Proto::Gear, a, 0, Some(0x1A))?;
        if !b.is_empty() {
            g.bank0 = Some(Bank0::parse(&b));
        }
    }
    ctx.report("read", format!("A{a}: done"), 1.0);
    Ok(g)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "param", content = "index", rename_all = "snake_case")]
pub enum GearParam {
    MaxLevel,
    MinLevel,
    SystemFailureLevel,
    PowerOnLevel,
    FadeTime,
    FadeRate,
    ExtendedFadeTime,
    OperatingMode,
    Scene(u8),
    Group(u8),
    Dt6DimmingCurve,
}

/// Write one gear parameter; `value` of 255 removes from a scene, and for
/// groups any non-zero value means "member".
pub fn write_gear(bus: &dyn DaliBus, target: u8, param: GearParam, value: u8) -> Result<()> {
    let dtr0 = |v: u8| bus.send(16, gear::special(gear::DTR0, v), false);
    let set = |op: u8| bus.send(16, gear::cmd(target, op), true);
    match param {
        GearParam::MaxLevel => {
            dtr0(value)?;
            set(0x2A)
        }
        GearParam::MinLevel => {
            dtr0(value)?;
            set(0x2B)
        }
        GearParam::SystemFailureLevel => {
            dtr0(value)?;
            set(0x2C)
        }
        GearParam::PowerOnLevel => {
            dtr0(value)?;
            set(0x2D)
        }
        GearParam::FadeTime => {
            dtr0(value)?;
            set(0x2E)
        }
        GearParam::FadeRate => {
            dtr0(value)?;
            set(0x2F)
        }
        GearParam::ExtendedFadeTime => {
            dtr0(value)?;
            set(0x30)
        }
        GearParam::OperatingMode => {
            dtr0(value)?;
            set(0x23)
        }
        GearParam::Scene(n) if n < 16 => {
            if value == 255 {
                set(0x50 + n)
            } else {
                dtr0(value)?;
                set(0x40 + n)
            }
        }
        GearParam::Group(n) if n < 16 => set(if value != 0 { 0x60 + n } else { 0x70 + n }),
        GearParam::Dt6DimmingCurve => {
            dtr0(value)?;
            bus.send(16, gear::special(gear::ENABLE_DT, 6), false)?;
            set(0xE3)
        }
        _ => Err(DongleError::Other("invalid parameter index".into())),
    }
}

// ---------------------------------------------------------------------------
// Control devices (103 / 303)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Default)]
pub struct OccupancyParams {
    pub deadtime: Option<u8>,
    pub hold: Option<u8>,
    pub report: Option<u8>,
    pub catching: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct InstanceInfo {
    pub index: u8,
    pub instance_type: Option<u8>,
    pub resolution: Option<u8>,
    pub enabled: Option<bool>,
    pub event_scheme: Option<u8>,
    pub event_priority: Option<u8>,
    pub primary_group: Option<u8>,
    pub group1: Option<u8>,
    pub group2: Option<u8>,
    pub event_filter: Option<u32>,
    pub input_value: Option<u32>,
    pub error: Option<u8>,
    pub status: Option<u8>,
    pub occupancy: Option<OccupancyParams>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DeviceInfo103 {
    pub address: u8,
    pub status: Option<u8>,
    pub capabilities: Option<u8>,
    pub version: Option<u8>,
    pub instance_count: Option<u8>,
    pub operating_mode: Option<u8>,
    pub application_enabled: Option<bool>,
    pub power_cycle_notification: Option<bool>,
    pub groups: Option<u32>,
    pub random_address: Option<u32>,
    pub instances: Vec<InstanceInfo>,
    pub bank0: Option<Bank0>,
}

fn yes(r: FrameResult) -> Option<bool> {
    match r {
        FrameResult::Answer(_) | FrameResult::Collision => Some(true),
        FrameResult::NoAnswer => Some(false),
        FrameResult::Sent => None,
    }
}

pub fn read_device(ctx: &Ctx, a: u8, with_bank0: bool) -> Result<DeviceInfo103> {
    let bus = ctx.bus;
    let addr = dev::short(a);
    let dq = |op: u8| -> Result<FrameResult> { bus.query(24, dev::cmd(addr, dev::DEVICE, op)) };
    let iq = |i: u8, op: u8| -> Result<FrameResult> { bus.query(24, dev::cmd(addr, i, op)) };
    let mut d = DeviceInfo103 { address: a, ..Default::default() };
    ctx.report("read", format!("D{a}: device"), 0.0);
    d.status = ans(dq(0x30)?);
    d.capabilities = ans(dq(0x46)?);
    d.version = ans(dq(0x34)?);
    d.instance_count = ans(dq(0x35)?);
    d.operating_mode = ans(dq(0x3E)?);
    d.application_enabled = yes(dq(0x3D)?);
    d.power_cycle_notification = yes(dq(0x45)?);
    let gs: Vec<Option<u8>> = (0x41..=0x44).map(|op| dq(op).map(ans)).collect::<Result<_>>()?;
    if gs.iter().all(|g| g.is_some()) {
        d.groups = Some(gs.iter().enumerate().fold(0u32, |acc, (i, g)| acc | (g.unwrap() as u32) << (8 * i)));
    }
    if let (Some(h), Some(m), Some(l)) = (ans(dq(0x39)?), ans(dq(0x3A)?), ans(dq(0x3B)?)) {
        d.random_address = Some((h as u32) << 16 | (m as u32) << 8 | l as u32);
    }
    let n = d.instance_count.unwrap_or(0).min(32);
    for i in 0..n {
        ctx.check()?;
        ctx.report("read", format!("D{a}: instance {i}"), 0.2 + 0.6 * (i as f32 / n.max(1) as f32));
        let mut inst = InstanceInfo { index: i, ..Default::default() };
        inst.instance_type = ans(iq(i, 0x80)?);
        inst.resolution = ans(iq(i, 0x81)?);
        inst.enabled = yes(iq(i, 0x86)?);
        inst.event_scheme = ans(iq(i, 0x8B)?);
        inst.event_priority = ans(iq(i, 0x84)?);
        inst.primary_group = ans(iq(i, 0x88)?);
        inst.group1 = ans(iq(i, 0x89)?);
        inst.group2 = ans(iq(i, 0x8A)?);
        inst.error = ans(iq(i, 0x82)?);
        inst.status = ans(iq(i, 0x83)?);
        let f: Vec<Option<u8>> = (0x90..=0x92).map(|op| iq(i, op).map(ans)).collect::<Result<_>>()?;
        if let [Some(a0), b1, b2] = f.as_slice() {
            inst.event_filter = Some(*a0 as u32 | (b1.unwrap_or(0) as u32) << 8 | (b2.unwrap_or(0) as u32) << 16);
        }
        // Input value: first byte via QUERY INPUT VALUE, the rest via LATCH.
        if let Some(first) = ans(iq(i, 0x8C)?) {
            let bytes = inst.resolution.map(|r| (r as u32).div_ceil(8)).unwrap_or(1).clamp(1, 4);
            let mut v = first as u32;
            for _ in 1..bytes {
                v = (v << 8) | ans(iq(i, 0x8D)?).unwrap_or(0) as u32;
            }
            inst.input_value = Some(v);
        }
        if inst.instance_type == Some(3) {
            inst.occupancy = Some(OccupancyParams {
                deadtime: ans(iq(i, 0x2C)?),
                hold: ans(iq(i, 0x2D)?),
                report: ans(iq(i, 0x2E)?),
                catching: yes(iq(i, 0x2F)?),
            });
        }
        d.instances.push(inst);
    }
    if with_bank0 {
        ctx.check()?;
        ctx.report("read", format!("D{a}: memory bank 0"), 0.85);
        let b = read_bank(bus, Proto::Device, a, 0, Some(0x1A))?;
        if !b.is_empty() {
            d.bank0 = Some(Bank0::parse(&b));
        }
    }
    ctx.report("read", format!("D{a}: done"), 1.0);
    Ok(d)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "param", rename_all = "snake_case")]
pub enum DeviceParam {
    ApplicationController { enabled: bool },
    PowerCycleNotification { enabled: bool },
    Group { group: u8, member: bool },
    OperatingMode { value: u8 },
    InstanceEnabled { instance: u8, enabled: bool },
    EventScheme { instance: u8, value: u8 },
    EventPriority { instance: u8, value: u8 },
    PrimaryGroup { instance: u8, value: u8 },
    InstanceGroup1 { instance: u8, value: u8 },
    InstanceGroup2 { instance: u8, value: u8 },
    EventFilter { instance: u8, value: u32 },
    Deadtime { instance: u8, value: u8 },
    Hold { instance: u8, value: u8 },
    Report { instance: u8, value: u8 },
}

pub fn write_device(bus: &dyn DaliBus, a: u8, p: DeviceParam) -> Result<()> {
    let addr = dev::short(a);
    let dtr = |n: u8, v: u8| bus.send(24, dev::special(dev::DTR0 + n, v), false);
    let dcmd = |op: u8| bus.send(24, dev::cmd(addr, dev::DEVICE, op), true);
    let icmd = |i: u8, op: u8| bus.send(24, dev::cmd(addr, i, op), true);
    use DeviceParam::*;
    match p {
        ApplicationController { enabled } => dcmd(if enabled { 0x16 } else { 0x17 }),
        PowerCycleNotification { enabled } => dcmd(if enabled { 0x1F } else { 0x20 }),
        Group { group, member } if group < 32 => {
            let bit = 1u16 << (group % 16);
            dtr(1, bit as u8)?;
            dtr(2, (bit >> 8) as u8)?;
            dcmd(match (member, group < 16) {
                (true, true) => 0x19,
                (true, false) => 0x1A,
                (false, true) => 0x1B,
                (false, false) => 0x1C,
            })
        }
        OperatingMode { value } => {
            dtr(0, value)?;
            dcmd(0x18)
        }
        InstanceEnabled { instance, enabled } => icmd(instance, if enabled { 0x62 } else { 0x63 }),
        EventScheme { instance, value } => {
            dtr(0, value)?;
            icmd(instance, 0x67)
        }
        EventPriority { instance, value } => {
            dtr(0, value)?;
            icmd(instance, 0x61)
        }
        PrimaryGroup { instance, value } => {
            dtr(0, value)?;
            icmd(instance, 0x64)
        }
        InstanceGroup1 { instance, value } => {
            dtr(0, value)?;
            icmd(instance, 0x65)
        }
        InstanceGroup2 { instance, value } => {
            dtr(0, value)?;
            icmd(instance, 0x66)
        }
        EventFilter { instance, value } => {
            dtr(0, value as u8)?;
            dtr(1, (value >> 8) as u8)?;
            dtr(2, (value >> 16) as u8)?;
            icmd(instance, 0x68)
        }
        Deadtime { instance, value } => {
            dtr(0, value)?;
            icmd(instance, 0x23)
        }
        Hold { instance, value } => {
            dtr(0, value)?;
            icmd(instance, 0x21)
        }
        Report { instance, value } => {
            dtr(0, value)?;
            icmd(instance, 0x22)
        }
        _ => Err(DongleError::Other("invalid parameter".into())),
    }
}
