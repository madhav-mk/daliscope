//! A simulated DALI-2 USB interface with a virtual DALI bus.
//!
//! It speaks LUBA byte-for-byte (same framing, responses and event messages
//! as described in LUBAP V1.10) and models control gear (102 + DT6/DT8) and
//! control devices (103 + 303 occupancy), including random-address addressing,
//! send-twice semantics, memory bank 0 and realistic frame timing. Spontaneous
//! traffic from an "application controller" and sensor events make the bus
//! monitor come alive without hardware.

use crate::dongle::Link;
use crate::event;
use crate::luba::{cmd, Decoder, Packet, Settings};
use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

const BIT_US: f64 = 833.33;

fn frame_dur(bits: u8) -> Duration {
    // start bit + data bits + 2 stop bits
    Duration::from_micros(((1 + bits as u32 + 2) as f64 * BIT_US) as u64)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
}

#[derive(Clone, Default)]
struct Search {
    random: u32,
    initialise: bool,
    withdrawn: bool,
}

fn bank0(gtin: u64, serial: u64, fw: (u8, u8), gear_units: u8, dev_units: u8) -> Vec<u8> {
    let mut b = vec![0u8; 0x1B];
    b[0] = 0x1A;
    b[1] = 0;
    b[2] = 0;
    for i in 0..6 {
        b[3 + i] = (gtin >> (8 * (5 - i))) as u8;
    }
    b[9] = fw.0;
    b[10] = fw.1;
    for i in 0..8 {
        b[0x0B + i] = (serial >> (8 * (7 - i))) as u8;
    }
    b[0x13] = 1;
    b[0x14] = 0;
    b[0x15] = 0x08;
    b[0x16] = if gear_units > 0 { 0x08 } else { 0xFF };
    b[0x17] = if dev_units > 0 { 0x08 } else { 0xFF };
    b[0x18] = dev_units;
    b[0x19] = gear_units;
    b[0x1A] = 0;
    b
}

#[derive(Clone)]
struct Gear {
    short: Option<u8>,
    s: Search,
    types: Vec<u8>,
    next_type: usize,
    actual: u8,
    last_active: u8,
    max: u8,
    min: u8,
    phys_min: u8,
    power_on: u8,
    sys_fail: u8,
    fade_time: u8,
    fade_rate: u8,
    ext_fade: u8,
    groups: u16,
    scenes: [u8; 16],
    op_mode: u8,
    dimming_curve: u8,
    tc: u16,
    bank0: Vec<u8>,
    reset_state: bool,
}

impl Gear {
    fn new(short: Option<u8>, types: Vec<u8>, random: u32, serial: u64) -> Self {
        Self {
            short,
            s: Search { random, ..Default::default() },
            types,
            next_type: 0,
            actual: 0,
            last_active: 254,
            max: 254,
            min: 85,
            phys_min: 85,
            power_on: 254,
            sys_fail: 254,
            fade_time: 0,
            fade_rate: 7,
            ext_fade: 0,
            groups: 0,
            scenes: [255; 16],
            op_mode: 0,
            dimming_curve: 0,
            tc: 370,
            bank0: bank0(9010342005000 + serial % 1000, serial, (2, 3), 1, 0),
            reset_state: true,
        }
    }

    fn matches(&self, a: u8) -> bool {
        if a & 0x80 == 0 {
            self.short == Some((a >> 1) & 0x3F)
        } else if a & 0xE0 == 0x80 {
            self.groups & (1 << ((a >> 1) & 0x0F)) != 0
        } else if a >= 0xFE {
            true
        } else if a >= 0xFC {
            self.short.is_none()
        } else {
            false
        }
    }

    fn status(&self) -> u8 {
        let mut s = 0;
        if self.actual > 0 {
            s |= 0x04;
        }
        if self.reset_state {
            s |= 0x20;
        }
        if self.short.is_none() {
            s |= 0x40;
        }
        s
    }

    fn set_level(&mut self, l: u8) {
        if l == 255 {
            return;
        }
        self.actual = if l == 0 { 0 } else { l.clamp(self.min, self.max) };
        if self.actual > 0 {
            self.last_active = self.actual;
        }
        self.reset_state = false;
    }
}

#[derive(Clone)]
struct Instance {
    itype: u8,
    enabled: bool,
    scheme: u8,
    priority: u8,
    pgroup: u8,
    g1: u8,
    g2: u8,
    filter: u32,
    resolution: u8,
    deadtime: u8,
    hold: u8,
    report: u8,
    occupied: bool,
    value: u16,
}

#[derive(Clone)]
struct Device {
    short: Option<u8>,
    s: Search,
    app_enabled: bool,
    power_cycle_notify: bool,
    groups: u32,
    op_mode: u8,
    instances: Vec<Instance>,
    bank0: Vec<u8>,
    quiescent: bool,
}

impl Device {
    fn matches(&self, a: u8) -> bool {
        if a & 0x80 == 0 {
            self.short == Some((a >> 1) & 0x3F)
        } else if a & 0xC0 == 0x80 {
            self.groups & (1 << ((a >> 1) & 0x1F)) != 0
        } else if a == 0xFF {
            true
        } else if a == 0xFD {
            self.short.is_none()
        } else {
            false
        }
    }

    fn inst_matches(&self, sel: u8, idx: usize) -> bool {
        let i = &self.instances[idx];
        match sel {
            0xFF => true,
            _ => match sel >> 5 {
                0b000 => (sel & 0x1F) as usize == idx,
                0b100 => [i.pgroup, i.g1, i.g2].contains(&(sel & 0x1F)),
                0b110 => i.itype == sel & 0x1F,
                _ => false,
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Answer {
    None,
    Value(u8),
    Collision,
}

pub struct Sim {
    gear: Vec<Gear>,
    devices: Vec<Device>,
    dtr: [u8; 3],
    gear_search: u32,
    dev_search: u32,
    edt: Option<u8>,
    settings: Settings,
    start: Instant,
    next_id: u8,
    bus_free: Instant,
    out: VecDeque<(Instant, Vec<u8>)>,
    rng: Rng,
    next_event: Instant,
    next_query: Instant,
    last_config: Option<(u8, u32, Instant)>,
    pub spontaneous: bool,
    /// Another power supply feeds the bus (otherwise only the interface's own can).
    pub external_psu: bool,
    /// Ignore SETTINGS writes (models firmware that won't change its filter).
    pub lock_settings: bool,
    /// Answer ADD FRAME with FirstID only (NrOfFrames is optional in LUBAP).
    pub short_ack: bool,
}

impl Sim {
    pub fn empty() -> Self {
        let now = Instant::now();
        Self {
            gear: vec![],
            devices: vec![],
            dtr: [0; 3],
            gear_search: 0xFF_FFFF,
            dev_search: 0xFF_FFFF,
            edt: None,
            settings: Settings { mode: 0, event_filter: 0x10, hardware: 0 },
            start: now,
            next_id: 0,
            bus_free: now,
            out: VecDeque::new(),
            rng: Rng(0x9E37_79B9_7F4A_7C15),
            next_event: now + Duration::from_secs(3),
            next_query: now + Duration::from_secs(5),
            last_config: None,
            spontaneous: false,
            external_psu: true,
            lock_settings: false,
            short_ack: false,
        }
    }

    /// A small demo installation: three LED drivers, a DT8 tunable-white
    /// driver, one unaddressed LED driver and two occupancy sensors (one
    /// unaddressed), plus background traffic.
    pub fn demo() -> Self {
        let mut s = Self::empty();
        s.spontaneous = true;
        let mut g0 = Gear::new(Some(0), vec![6], 0x12_3456, 1001);
        g0.groups = 0b01;
        let mut g1 = Gear::new(Some(1), vec![6], 0x45_6789, 1002);
        g1.groups = 0b11;
        g1.scenes[0] = 200;
        let mut g2 = Gear::new(Some(2), vec![6, 8], 0x78_9ABC, 1003);
        g2.groups = 0b10;
        let g3 = Gear::new(None, vec![6], 0x2B_CDEF, 1004);
        s.gear = vec![g0, g1, g2, g3];
        s.devices = vec![
            Self::sensor(Some(0), 0x31_4159, 2001),
            Self::sensor(None, 0x27_1828, 2002),
        ];
        s
    }

    /// Bus with only unaddressed devices (for addressing tests).
    pub fn unaddressed(gears: usize, devices: usize) -> Self {
        let mut s = Self::empty();
        for i in 0..gears {
            let r = (s.rng.next() & 0xFF_FFFF) as u32;
            s.gear.push(Gear::new(None, vec![6], r, 3000 + i as u64));
        }
        for i in 0..devices {
            let r = (s.rng.next() & 0xFF_FFFF) as u32;
            s.devices.push(Self::sensor(None, r, 4000 + i as u64));
        }
        s
    }

    fn sensor(short: Option<u8>, random: u32, serial: u64) -> Device {
        Device {
            short,
            s: Search { random, ..Default::default() },
            app_enabled: false,
            power_cycle_notify: false,
            groups: 0,
            op_mode: 0,
            instances: vec![Instance {
                itype: 3,
                enabled: true,
                scheme: 1,
                priority: 4,
                pgroup: 255,
                g1: 255,
                g2: 255,
                filter: 0x1F,
                resolution: 2,
                deadtime: 2,
                hold: 90,
                report: 20,
                occupied: false,
                value: 0,
            }],
            bank0: bank0(9010342007000 + serial % 1000, serial, (1, 4), 0, 1),
            quiescent: false,
        }
    }

    fn tick(&self, t: Instant) -> u16 {
        ((t - self.start).as_micros() / 100) as u16
    }

    fn push(&mut self, at: Instant, cmd: u8, data: Vec<u8>) {
        let bytes = Packet::new(cmd, data).encode();
        let pos = self.out.iter().position(|(t, _)| *t > at).unwrap_or(self.out.len());
        self.out.insert(pos, (at, bytes));
    }

    /// The bus has voltage when some supply is on.
    pub fn powered(&self) -> bool {
        self.external_psu || self.settings.bus_power()
    }

    pub fn set_next_id(&mut self, id: u8) {
        self.next_id = id;
    }

    pub fn set_event_filter(&mut self, f: u8) {
        self.settings.event_filter = f;
    }

    /// Builder: a bus with no supply but the interface's own (off by default).
    pub fn without_psu(mut self) -> Self {
        self.external_psu = false;
        self.spontaneous = false;
        self
    }

    fn push_event(&mut self, at: Instant, etype: u8, info: u8, data: &[u8]) {
        // Honour the event filter exactly as LUBAP describes it.
        let f = self.settings.event_filter;
        let suppressed = f & Settings::F_ALL != 0
            || (etype == 0 && f & Settings::F_SENT != 0)
            || (etype == 2 && f & Settings::F_RECEIVED != 0)
            || (etype == 3 && (info == 3 || info == 4) && f & Settings::F_BUFFER != 0)
            || (etype == 3 && info >= 60 && f & Settings::F_MACRO != 0);
        if suppressed {
            return;
        }
        let tick = (f & Settings::F_TICK == 0).then(|| self.tick(at));
        let line = (f & Settings::F_LINE == 0).then_some(0);
        let d = event::build_opt(tick, line, etype, info, data);
        self.push(at, cmd::EVENT, d);
    }

    /// Reserve the bus for a forward frame; returns its end time.
    fn reserve(&mut self, now: Instant, bits: u8, priority: u8) -> (Instant, Instant) {
        let gap = Duration::from_micros(13_500 + priority as u64 * 1_100);
        let start = if self.bus_free + gap > now { self.bus_free + gap } else { now + Duration::from_micros(500) };
        let end = start + frame_dur(bits);
        self.bus_free = end;
        (start, end)
    }

    fn answer_slot(&mut self, end: Instant) -> Instant {
        let a = end + Duration::from_micros(7_000) + frame_dur(8);
        self.bus_free = a;
        a
    }

    pub fn handle(&mut self, p: Packet, now: Instant) {
        let r = cmd::response(p.cmd);
        let at = now + Duration::from_millis(1);
        match p.cmd {
            cmd::QUERY_DEVICE_DESCRIPTOR => {
                let mut d = vec![1, 16];
                d.extend(100u32.to_le_bytes());
                d.push(32);
                d.extend([0u8; 8]);
                d.extend([0, 1, 10, 0x80]);
                self.push(at, r, d);
            }
            cmd::QUERY_DEVICE_INFO => {
                let set = p.data.first().copied().unwrap_or(0);
                let d = if set == 0 {
                    let mut d = 9_010_342_014_000u64.to_be_bytes()[2..].to_vec();
                    d.extend(0x0000_0000_00C0_FFEEu64.to_be_bytes());
                    d.extend([1, 1]);
                    d.extend(24_138_215u32.to_le_bytes());
                    d
                } else {
                    let mut d = b"DALI-2 USB 30mA".to_vec();
                    d.resize(16, 0);
                    d.extend([25, 10]);
                    d
                };
                self.push(at, r, d);
            }
            cmd::DEVICE_NAME => {
                let mut d = b"Simulated DALI-2 USB".to_vec();
                d.resize(29, 0);
                self.push(at, r, d);
            }
            cmd::SETTINGS => {
                if let Some(s) = Settings::parse(&p.data) {
                    if !self.lock_settings {
                        let was = self.powered();
                        self.settings = s;
                        if was != self.powered() {
                            // Bus voltage changes are reported as line events.
                            self.push_event(at, 3, if self.powered() { 2 } else { 1 }, &[]);
                        }
                    }
                }
                self.push(at, r, self.settings.bytes().to_vec());
            }
            cmd::READ_STATUS => {
                let mut d = vec![0];
                d.extend(self.tick(now).to_le_bytes());
                d.extend([self.next_id, 0, if self.powered() { 0 } else { 0x80 }]);
                self.push(at, r, d);
            }
            cmd::IDENTIFY_SELF => self.push(at, r, vec![]),
            cmd::ADD_FRAME | cmd::ADD_FRAME_16 | cmd::ADD_FRAME_24 => {
                let frames = parse_frames(&p);
                match frames {
                    Some(_) if !self.powered() => self.push(at, r, vec![1]),
                    Some(fr) if !fr.is_empty() => {
                        let first = self.next_id;
                        self.push(at, r, if self.short_ack { vec![first] } else { vec![first, fr.len() as u8] });
                        for (bits, mode, value) in fr {
                            let id = self.next_id;
                            self.next_id = if self.next_id >= 254 { 0 } else { self.next_id + 1 };
                            self.transmit(now, id, bits, mode, value);
                        }
                    }
                    _ => self.push(at, r, vec![6]),
                }
            }
            _ => {}
        }
    }

    fn transmit(&mut self, now: Instant, id: u8, bits: u8, mode: u8, value: u32) {
        let copies = if mode & 0x80 != 0 { 2 } else { 1 };
        let wait = mode & 0x40 != 0;
        let prio = (mode & 7).max(1);
        let mut last_end = now;
        let mut answer = Answer::None;
        for _ in 0..copies {
            let (_, end) = self.reserve(now, bits, prio);
            let mut d = vec![id];
            d.extend(value.to_be_bytes());
            self.push_event(end, 0, bits, &d);
            answer = self.execute(bits, value, end);
            last_end = end;
        }
        if wait {
            match answer {
                Answer::Value(v) => {
                    let t = self.answer_slot(last_end);
                    self.push_event(t, 1, 8, &[id, v]);
                }
                Answer::Collision => {
                    let t = self.answer_slot(last_end);
                    self.push_event(t, 1, 63, &[id, 255]);
                }
                Answer::None => {
                    let t = last_end + Duration::from_micros(22_000);
                    self.bus_free = t;
                    self.push_event(t, 1, 0, &[id]);
                }
            }
        }
    }

    /// A frame from some other master on the bus (shows up as "received").
    fn foreign(&mut self, now: Instant, bits: u8, value: u32) {
        let (_, end) = self.reserve(now, bits, 4);
        self.push_event(end, 2, bits, &value.to_be_bytes()[(4 - (bits as usize).div_ceil(8))..]);
        if let Answer::Value(v) = self.execute(bits, value, end) {
            let t = self.answer_slot(end);
            self.push_event(t, 2, 8, &[v]);
        }
    }

    /// Spontaneous traffic: sensor events and an application controller.
    pub fn pump(&mut self, now: Instant) {
        if !self.spontaneous || !self.powered() {
            return;
        }
        if now >= self.next_event {
            self.next_event = now + Duration::from_millis(self.rng.range(2_500, 7_000));
            let n = self.devices.len();
            if n > 0 {
                let di = self.rng.range(0, n as u64 - 1) as usize;
                let movement = self.rng.range(0, 2) > 0;
                let dev = &mut self.devices[di];
                let short = dev.short;
                if let (Some(sh), Some(inst)) = (short, dev.instances.first_mut()) {
                    if inst.enabled {
                        inst.occupied = movement || !inst.occupied && self.rng.0 & 1 == 1;
                        inst.value = if inst.occupied { 0xFF } else { 0 };
                        let info = (inst.occupied as u32) << 1 | movement as u32;
                        let frame = match inst.scheme {
                            0 => (1 << 23) | ((inst.itype as u32) << 17) | (1 << 15) | info,
                            2 => ((sh as u32) << 17) | (1 << 15) | info,
                            _ => ((sh as u32) << 17) | ((inst.itype as u32) << 10) | info,
                        };
                        let occupied = inst.occupied;
                        self.foreign(now, 24, frame);
                        // The application controller reacts.
                        let level = if occupied { 254 } else { 0 };
                        self.foreign(now, 16, ((0x80u32) << 8) | level);
                    }
                }
            }
        }
        if now >= self.next_query {
            self.next_query = now + Duration::from_millis(self.rng.range(6_000, 12_000));
            if let Some(a) = self.gear.iter().filter_map(|g| g.short).next() {
                self.foreign(now, 16, ((((a << 1) | 1) as u32) << 8) | 0xA0);
            }
        }
    }

    fn is_config(&mut self, bits: u8, value: u32, t: Instant, needs_twice: bool) -> bool {
        if !needs_twice {
            return true;
        }
        let ok = matches!(self.last_config, Some((b, v, lt)) if b == bits && v == value && t - lt <= Duration::from_millis(100));
        self.last_config = if ok { None } else { Some((bits, value, t)) };
        ok
    }

    fn collect(answers: Vec<u8>) -> Answer {
        match answers.len() {
            0 => Answer::None,
            1 => Answer::Value(answers[0]),
            _ => Answer::Collision,
        }
    }

    fn execute(&mut self, bits: u8, value: u32, t: Instant) -> Answer {
        match bits {
            16 => self.exec16(value, t),
            24 => self.exec24(value, t),
            _ => Answer::None,
        }
    }

    fn exec16(&mut self, v: u32, t: Instant) -> Answer {
        let a = (v >> 8) as u8;
        let d = v as u8;
        let edt = self.edt.take();
        if (0xA0..=0xCB).contains(&a) && a & 1 == 1 {
            let twice = matches!(a, 0xA5 | 0xA7);
            if !self.is_config(16, v, t, twice) {
                return Answer::None;
            }
            let search = self.gear_search;
            let mut answers = vec![];
            match a {
                0xA1 => self.gear.iter_mut().for_each(|g| {
                    g.s.initialise = false;
                    g.s.withdrawn = false
                }),
                0xA3 => self.dtr[0] = d,
                0xC3 => self.dtr[1] = d,
                0xC5 => self.dtr[2] = d,
                0xC1 => self.edt = Some(d),
                0xA5 => self.gear.iter_mut().for_each(|g| {
                    let m = match d {
                        0x00 => true,
                        0xFF => g.short.is_none(),
                        x => g.short == Some((x >> 1) & 0x3F),
                    };
                    if m {
                        g.s.initialise = true;
                        g.s.withdrawn = false;
                    }
                }),
                0xA7 => {
                    for i in 0..self.gear.len() {
                        if self.gear[i].s.initialise {
                            self.gear[i].s.random = (self.rng.next() & 0xFF_FFFF) as u32;
                        }
                    }
                }
                0xA9 => {
                    for g in &self.gear {
                        if g.s.initialise && !g.s.withdrawn && g.s.random <= search {
                            answers.push(0xFF);
                        }
                    }
                }
                0xAB => self.gear.iter_mut().for_each(|g| {
                    if g.s.initialise && g.s.random == search {
                        g.s.withdrawn = true
                    }
                }),
                0xB1 => self.gear_search = (search & 0x00FFFF) | (d as u32) << 16,
                0xB3 => self.gear_search = (search & 0xFF00FF) | (d as u32) << 8,
                0xB5 => self.gear_search = (search & 0xFFFF00) | d as u32,
                0xB7 => self.gear.iter_mut().for_each(|g| {
                    if g.s.initialise && g.s.random == search {
                        g.short = if d == 0xFF { None } else { Some((d >> 1) & 0x3F) };
                    }
                }),
                0xB9 => {
                    for g in &self.gear {
                        if g.s.initialise && g.short == Some((d >> 1) & 0x3F) {
                            answers.push(0xFF);
                        }
                    }
                }
                0xBB => {
                    for g in &self.gear {
                        if g.s.initialise && !g.s.withdrawn && g.s.random == search {
                            answers.push(g.short.map(|s| (s << 1) | 1).unwrap_or(0xFF));
                        }
                    }
                }
                _ => {}
            }
            return Self::collect(answers);
        }
        let config = (a & 1 == 1) && ((0x20..=0x81).contains(&d) || (edt == Some(6) && (0xE0..=0xE4).contains(&d)));
        if config && !self.is_config(16, v, t, true) {
            return Answer::None;
        }
        let dtr = self.dtr;
        let mut answers = vec![];
        for gi in 0..self.gear.len() {
            if !self.gear[gi].matches(a) {
                continue;
            }
            let g = &mut self.gear[gi];
            if a & 1 == 0 {
                g.set_level(d);
                continue;
            }
            let yes = |b: bool| if b { Some(0xFF) } else { None };
            let ans: Option<u8> = match d {
                0x00 => {
                    g.actual = 0;
                    None
                }
                0x05 => {
                    let m = g.max;
                    g.set_level(m);
                    None
                }
                0x06 => {
                    let m = g.min;
                    g.set_level(m);
                    None
                }
                0x0A => {
                    let l = g.last_active;
                    g.set_level(l);
                    None
                }
                0x10..=0x1F => {
                    let l = g.scenes[(d - 0x10) as usize];
                    g.set_level(l);
                    None
                }
                0x20 => {
                    let short = g.short;
                    let s = g.s.clone();
                    let types = g.types.clone();
                    let b0 = g.bank0.clone();
                    *g = Gear::new(short, types, s.random, 0);
                    g.s = s;
                    g.bank0 = b0;
                    None
                }
                0x21 => {
                    self.dtr[0] = g.actual;
                    None
                }
                0x23 => {
                    g.op_mode = dtr[0];
                    None
                }
                0x2A => {
                    g.max = dtr[0].clamp(g.min.max(g.phys_min), 254);
                    None
                }
                0x2B => {
                    g.min = dtr[0].clamp(g.phys_min, g.max);
                    None
                }
                0x2C => {
                    g.sys_fail = dtr[0];
                    None
                }
                0x2D => {
                    g.power_on = dtr[0];
                    None
                }
                0x2E => {
                    g.fade_time = dtr[0].min(15);
                    None
                }
                0x2F => {
                    g.fade_rate = dtr[0].clamp(1, 15);
                    None
                }
                0x30 => {
                    g.ext_fade = dtr[0] & 0x4F;
                    None
                }
                0x40..=0x4F => {
                    g.scenes[(d - 0x40) as usize] = dtr[0];
                    None
                }
                0x50..=0x5F => {
                    g.scenes[(d - 0x50) as usize] = 255;
                    None
                }
                0x60..=0x6F => {
                    g.groups |= 1 << (d - 0x60);
                    None
                }
                0x70..=0x7F => {
                    g.groups &= !(1 << (d - 0x70));
                    None
                }
                0x80 => {
                    g.short = if dtr[0] == 0xFF { None } else { Some((dtr[0] >> 1) & 0x3F) };
                    None
                }
                0x90 => Some(g.status()),
                0x91 => Some(0xFF),
                0x92 => None,
                0x93 => yes(g.actual > 0),
                0x95 => yes(g.reset_state),
                0x96 => yes(g.short.is_none()),
                0x97 => Some(0x08),
                0x98 => Some(dtr[0]),
                0x99 => Some(if g.types.len() > 1 {
                    g.next_type = 0;
                    0xFF
                } else {
                    g.types[0]
                }),
                0x9A => Some(g.phys_min),
                0x9C => Some(dtr[1]),
                0x9D => Some(dtr[2]),
                0x9E => Some(g.op_mode),
                0x9F => Some(6),
                0xA0 => Some(g.actual),
                0xA1 => Some(g.max),
                0xA2 => Some(g.min),
                0xA3 => Some(g.power_on),
                0xA4 => Some(g.sys_fail),
                0xA5 => Some(g.fade_time << 4 | g.fade_rate),
                0xA7 => {
                    let r = g.types.get(g.next_type).copied().unwrap_or(254);
                    g.next_type += 1;
                    Some(r)
                }
                0xA8 => Some(g.ext_fade),
                0xB0..=0xBF => Some(g.scenes[(d - 0xB0) as usize]),
                0xC0 => Some(g.groups as u8),
                0xC1 => Some((g.groups >> 8) as u8),
                0xC2 => Some((g.s.random >> 16) as u8),
                0xC3 => Some((g.s.random >> 8) as u8),
                0xC4 => Some(g.s.random as u8),
                0xC5 => {
                    let r = if dtr[1] == 0 { g.bank0.get(dtr[0] as usize).copied() } else { None };
                    self.dtr[0] = self.dtr[0].wrapping_add(1);
                    r
                }
                0xE0..=0xFE => match (edt, d) {
                    (Some(t), _) if !g.types.contains(&t) => None,
                    (Some(6), 0xE3) => {
                        g.dimming_curve = dtr[0];
                        None
                    }
                    (Some(6), 0xED) => Some(0x02),
                    (Some(6), 0xEE) => Some(g.dimming_curve),
                    (Some(6), 0xF0) => Some(0x00),
                    (Some(6), 0xF1) => Some(0x00),
                    (Some(8), 0xF9) => Some(0x02),
                    (Some(8), 0xFA) if dtr[0] == 2 => {
                        self.dtr[0] = g.tc as u8;
                        Some((g.tc >> 8) as u8)
                    }
                    _ => None,
                },
                0xFF => match edt {
                    Some(t) if g.types.contains(&t) => Some(2),
                    _ => None,
                },
                _ => None,
            };
            if let Some(x) = ans {
                answers.push(x);
            }
        }
        Self::collect(answers)
    }

    fn exec24(&mut self, v: u32, t: Instant) -> Answer {
        let a = (v >> 16) as u8;
        let i = (v >> 8) as u8;
        let o = v as u8;
        if a & 1 == 0 {
            return Answer::None; // event
        }
        if a == 0xC1 {
            let twice = matches!(i, 0x01 | 0x02);
            if !self.is_config(24, v, t, twice) {
                return Answer::None;
            }
            let search = self.dev_search;
            let mut answers = vec![];
            match i {
                0x00 => self.devices.iter_mut().for_each(|d| {
                    d.s.initialise = false;
                    d.s.withdrawn = false
                }),
                0x01 => self.devices.iter_mut().for_each(|d| {
                    let m = match o {
                        0xFF => true,
                        0x7F => d.short.is_none(),
                        x => d.short == Some(x),
                    };
                    if m {
                        d.s.initialise = true;
                        d.s.withdrawn = false;
                    }
                }),
                0x02 => {
                    for k in 0..self.devices.len() {
                        if self.devices[k].s.initialise {
                            self.devices[k].s.random = (self.rng.next() & 0xFF_FFFF) as u32;
                        }
                    }
                }
                0x03 => {
                    for d in &self.devices {
                        if d.s.initialise && !d.s.withdrawn && d.s.random <= search {
                            answers.push(0xFF);
                        }
                    }
                }
                0x04 => self.devices.iter_mut().for_each(|d| {
                    if d.s.initialise && d.s.random == search {
                        d.s.withdrawn = true
                    }
                }),
                0x05 => self.dev_search = (search & 0x00FFFF) | (o as u32) << 16,
                0x06 => self.dev_search = (search & 0xFF00FF) | (o as u32) << 8,
                0x07 => self.dev_search = (search & 0xFFFF00) | o as u32,
                0x08 => self.devices.iter_mut().for_each(|d| {
                    if d.s.initialise && d.s.random == search {
                        d.short = if o == 0xFF { None } else { Some(o & 0x3F) };
                    }
                }),
                0x09 => {
                    for d in &self.devices {
                        if d.s.initialise && d.short == Some(o) {
                            answers.push(0xFF);
                        }
                    }
                }
                0x0A => {
                    for d in &self.devices {
                        if d.s.initialise && !d.s.withdrawn && d.s.random == search {
                            answers.push(d.short.unwrap_or(0xFF));
                        }
                    }
                }
                0x30 => self.dtr[0] = o,
                0x31 => self.dtr[1] = o,
                0x32 => self.dtr[2] = o,
                _ => {}
            }
            return Self::collect(answers);
        }
        if a == 0xC7 {
            self.dtr[1] = i;
            self.dtr[0] = o;
            return Answer::None;
        }
        if a == 0xC9 {
            self.dtr[2] = i;
            self.dtr[1] = o;
            return Answer::None;
        }
        let config = if i == 0xFE { o <= 0x21 } else { (0x61..=0x68).contains(&o) || (0x21..=0x23).contains(&o) };
        if config && !self.is_config(24, v, t, true) {
            return Answer::None;
        }
        let dtr = self.dtr;
        let mut answers = vec![];
        for di in 0..self.devices.len() {
            if !self.devices[di].matches(a) {
                continue;
            }
            if i == 0xFE {
                let d = &mut self.devices[di];
                let yes = |b: bool| if b { Some(0xFF) } else { None };
                let r = match o {
                    0x14 => {
                        d.short = if dtr[0] == 0xFF { None } else { Some(dtr[0] & 0x3F) };
                        None
                    }
                    0x16 => {
                        d.app_enabled = true;
                        None
                    }
                    0x17 => {
                        d.app_enabled = false;
                        None
                    }
                    0x18 => {
                        d.op_mode = dtr[0];
                        None
                    }
                    0x19 => {
                        d.groups |= dtr[1] as u32 | (dtr[2] as u32) << 8;
                        None
                    }
                    0x1A => {
                        d.groups |= (dtr[1] as u32 | (dtr[2] as u32) << 8) << 16;
                        None
                    }
                    0x1B => {
                        d.groups &= !(dtr[1] as u32 | (dtr[2] as u32) << 8);
                        None
                    }
                    0x1C => {
                        d.groups &= !((dtr[1] as u32 | (dtr[2] as u32) << 8) << 16);
                        None
                    }
                    0x1D => {
                        d.quiescent = true;
                        None
                    }
                    0x1E => {
                        d.quiescent = false;
                        None
                    }
                    0x1F => {
                        d.power_cycle_notify = true;
                        None
                    }
                    0x20 => {
                        d.power_cycle_notify = false;
                        None
                    }
                    0x30 => Some(if d.short.is_none() { 0x04 } else { 0 } | if d.quiescent { 0x02 } else { 0 }),
                    0x31 | 0x32 => None,
                    0x33 => yes(d.short.is_none()),
                    0x34 => Some(0x08),
                    0x35 => Some(d.instances.len() as u8),
                    0x36 => Some(dtr[0]),
                    0x37 => Some(dtr[1]),
                    0x38 => Some(dtr[2]),
                    0x39 => Some((d.s.random >> 16) as u8),
                    0x3A => Some((d.s.random >> 8) as u8),
                    0x3B => Some(d.s.random as u8),
                    0x3C => {
                        let r = if dtr[1] == 0 { d.bank0.get(dtr[0] as usize).copied() } else { None };
                        self.dtr[0] = self.dtr[0].wrapping_add(1);
                        r
                    }
                    0x3D => yes(d.app_enabled),
                    0x3E => Some(d.op_mode),
                    0x40 => yes(d.quiescent),
                    0x41..=0x44 => Some((d.groups >> (8 * (o - 0x41))) as u8),
                    0x45 => yes(d.power_cycle_notify),
                    0x46 => Some(0x02),
                    0x47 => {
                        if dtr[0] == 3 {
                            Some(0x08)
                        } else {
                            None
                        }
                    }
                    0x48 => None,
                    _ => None,
                };
                if let Some(x) = r {
                    answers.push(x);
                }
                continue;
            }
            for ii in 0..self.devices[di].instances.len() {
                if !self.devices[di].inst_matches(i, ii) {
                    continue;
                }
                let inst = &mut self.devices[di].instances[ii];
                let yes = |b: bool| if b { Some(0xFF) } else { None };
                let r = match o {
                    0x20 => None,
                    0x21 => {
                        inst.hold = dtr[0];
                        None
                    }
                    0x22 => {
                        inst.report = dtr[0];
                        None
                    }
                    0x23 => {
                        inst.deadtime = dtr[0];
                        None
                    }
                    0x2C if inst.itype == 3 => Some(inst.deadtime),
                    0x2D if inst.itype == 3 => Some(inst.hold),
                    0x2E if inst.itype == 3 => Some(inst.report),
                    0x2F if inst.itype == 3 => None,
                    0x61 => {
                        inst.priority = dtr[0];
                        None
                    }
                    0x62 => {
                        inst.enabled = true;
                        None
                    }
                    0x63 => {
                        inst.enabled = false;
                        None
                    }
                    0x64 => {
                        inst.pgroup = dtr[0];
                        None
                    }
                    0x65 => {
                        inst.g1 = dtr[0];
                        None
                    }
                    0x66 => {
                        inst.g2 = dtr[0];
                        None
                    }
                    0x67 => {
                        inst.scheme = dtr[0].min(4);
                        None
                    }
                    0x68 => {
                        inst.filter = dtr[0] as u32 | (dtr[1] as u32) << 8 | (dtr[2] as u32) << 16;
                        None
                    }
                    0x80 => Some(inst.itype),
                    0x81 => Some(inst.resolution),
                    0x82 => None,
                    0x83 => Some(if inst.occupied { 0x02 } else { 0 }),
                    0x84 => Some(inst.priority),
                    0x86 => yes(inst.enabled),
                    0x88 => Some(inst.pgroup),
                    0x89 => Some(inst.g1),
                    0x8A => Some(inst.g2),
                    0x8B => Some(inst.scheme),
                    0x8C => Some(inst.value as u8),
                    0x8D => None,
                    0x90 => Some(inst.filter as u8),
                    0x91 => Some((inst.filter >> 8) as u8),
                    0x92 => Some((inst.filter >> 16) as u8),
                    _ => None,
                };
                if let Some(x) = r {
                    answers.push(x);
                }
            }
        }
        Self::collect(answers)
    }

    /// Bytes due by `now`.
    pub fn drain_due(&mut self, now: Instant, out: &mut Vec<u8>) {
        while self.out.front().is_some_and(|(t, _)| *t <= now) {
            out.extend(self.out.pop_front().unwrap().1);
        }
    }

    pub fn next_due(&self) -> Option<Instant> {
        self.out.front().map(|(t, _)| *t)
    }

    // Introspection for tests.
    pub fn gear_addresses(&self) -> Vec<Option<u8>> {
        self.gear.iter().map(|g| g.short).collect()
    }
    pub fn device_addresses(&self) -> Vec<Option<u8>> {
        self.devices.iter().map(|d| d.short).collect()
    }
}

fn parse_frames(p: &Packet) -> Option<Vec<(u8, u8, u32)>> {
    let d = &p.data;
    let mut out = vec![];
    match p.cmd {
        cmd::ADD_FRAME_16 => {
            for c in d.get(1..)?.chunks(3) {
                if c.len() != 3 {
                    return None;
                }
                out.push((16, c[0], (c[1] as u32) << 8 | c[2] as u32));
            }
        }
        cmd::ADD_FRAME_24 => {
            for c in d.get(1..)?.chunks(4) {
                if c.len() != 4 {
                    return None;
                }
                out.push((24, c[0], (c[1] as u32) << 16 | (c[2] as u32) << 8 | c[3] as u32));
            }
        }
        _ => {
            for c in d.get(1..)?.chunks(6) {
                if c.len() != 6 {
                    return None;
                }
                out.push((c[0], c[1], u32::from_be_bytes([c[2], c[3], c[4], c[5]])));
            }
        }
    }
    Some(out)
}

/// In-process [`Link`] to a [`Sim`].
pub struct SimLink {
    pub sim: Sim,
    dec: Decoder,
    pending: Vec<u8>,
}

impl SimLink {
    pub fn new(sim: Sim) -> Self {
        Self { sim, dec: Decoder::new(), pending: Vec::new() }
    }

    /// Feed host->interface bytes.
    pub fn input(&mut self, data: &[u8]) {
        let now = Instant::now();
        for p in self.dec.push(data) {
            self.sim.handle(p, now);
        }
    }

    /// Collect interface->host bytes due now.
    pub fn output(&mut self) -> Vec<u8> {
        let now = Instant::now();
        self.sim.pump(now);
        let mut v = std::mem::take(&mut self.pending);
        self.sim.drain_due(now, &mut v);
        v
    }
}

impl Link for SimLink {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            let now = Instant::now();
            self.sim.pump(now);
            let wait = self
                .sim
                .next_due()
                .map(|t| t.saturating_duration_since(now))
                .unwrap_or(Duration::from_millis(5))
                .min(Duration::from_millis(5));
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
            let mut v = Vec::new();
            self.sim.drain_due(Instant::now(), &mut v);
            self.pending = v;
        }
        let n = self.pending.len().min(buf.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.input(data);
        Ok(())
    }
}
