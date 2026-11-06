//! Turns LUBA events into decoded, timestamped bus-monitor records.

use crate::dali::{interpret_answer, Annotator, CmdKey, Decoded, Family};
use crate::event::{EventKind, RawEvent};
use serde::Serialize;
use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    /// Frame sent by this app through the interface.
    Tx,
    /// Frame received from the bus (another master / control device).
    Rx,
    /// Backward frame / answer to one of our queries.
    Ans,
    /// Line or interface status.
    Sys,
    /// Annotation inserted by the app.
    Note,
}

#[derive(Debug, Clone, Serialize)]
pub struct BusRecord {
    pub seq: u64,
    pub host_ms: f64,
    pub bus_us: Option<u64>,
    pub delta_ms: Option<f64>,
    pub dir: Dir,
    pub bits: u8,
    pub raw: String,
    pub family: String,
    pub target: String,
    pub command: String,
    pub detail: String,
    pub error: bool,
    pub frame_id: Option<u8>,
}

fn family_str(f: Family) -> &'static str {
    match f {
        Family::Gear => "gear",
        Family::GearSpecial => "gear_special",
        Family::Device => "device",
        Family::DeviceSpecial => "device_special",
        Family::Event => "event",
        Family::Backward => "backward",
        Family::Other => "other",
    }
}

pub fn hex_frame(bits: u8, value: u32) -> String {
    let digits = (bits as usize).div_ceil(4);
    let s = format!("{:0width$X}", value, width = digits.max(2));
    // Group in bytes from the right for readability.
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let first = chars.len() % 2;
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && (i + 2 - first).is_multiple_of(2) {
            out.push(' ');
        }
        out.push(*c);
    }
    out
}

/// Unwraps the interface's 16-bit tick counter using host time to resolve
/// whole wraps during quiet periods.
#[derive(Debug, Default)]
struct TickClock {
    tick_us: u64,
    last: Option<(u16, Instant)>,
    total_us: u64,
}

impl TickClock {
    fn update(&mut self, tick: u16, now: Instant) -> u64 {
        if let Some((lt, lh)) = self.last {
            let period = 65_536 * self.tick_us;
            let raw = tick.wrapping_sub(lt) as u64 * self.tick_us;
            let host = now.saturating_duration_since(lh).as_micros() as u64;
            let wraps = if host > raw + period / 2 { ((host - raw) as f64 / period as f64).round() as u64 } else { 0 };
            self.total_us += raw + wraps * period;
        }
        self.last = Some((tick, now));
        self.total_us
    }
}

pub struct Monitor {
    ann: Annotator,
    clock: TickClock,
    ours: HashMap<u8, Option<CmdKey>>,
    last_frame_us: Option<u64>,
    start: Instant,
}

impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

impl Monitor {
    pub fn new() -> Self {
        Self {
            ann: Annotator::new(),
            clock: TickClock { tick_us: 100, ..Default::default() },
            ours: HashMap::new(),
            last_frame_us: None,
            start: Instant::now(),
        }
    }

    pub fn set_tick_us(&mut self, us: u32) {
        self.clock.tick_us = us as u64;
    }

    pub fn reset(&mut self) {
        let tick = self.clock.tick_us;
        *self = Self::new();
        self.clock.tick_us = tick;
    }

    fn base(&self, dir: Dir, bus_us: Option<u64>) -> BusRecord {
        BusRecord {
            seq: 0,
            host_ms: now_ms(),
            bus_us,
            delta_ms: None,
            dir,
            bits: 0,
            raw: String::new(),
            family: String::new(),
            target: String::new(),
            command: String::new(),
            detail: String::new(),
            error: false,
            frame_id: None,
        }
    }

    /// A raw LUBA packet, for the optional interface packet log.
    pub fn raw(&mut self, outgoing: bool, cmd: u8, data: &[u8]) -> BusRecord {
        let mut r = self.base(if outgoing { Dir::Tx } else { Dir::Rx }, None);
        r.family = "luba".into();
        r.target = if outgoing { "→ iface".into() } else { "← iface".into() };
        r.raw = format!("{cmd:02X}");
        r.command = format!("LUBA 0x{cmd:02X} {}{}", crate::luba::cmd_name(cmd), if cmd & 1 == 1 && cmd != 0x31 { " (response)" } else { "" });
        r.detail = if data.is_empty() { "no data".into() } else { hex(data) };
        r
    }

    pub fn note(&mut self, text: &str) -> BusRecord {
        let mut r = self.base(Dir::Note, None);
        r.family = "note".into();
        r.command = text.into();
        r
    }

    fn frame_record(&mut self, dir: Dir, bus_us: Option<u64>, bits: u8, value: u32, d: Decoded) -> BusRecord {
        let mut r = self.base(dir, bus_us);
        r.bits = bits;
        r.raw = hex_frame(bits, value);
        r.family = family_str(d.family).into();
        r.target = d.target;
        r.command = d.command;
        r.detail = d.detail;
        if let (Some(now), Some(prev)) = (bus_us, self.last_frame_us) {
            r.delta_ms = Some(now.saturating_sub(prev) as f64 / 1000.0);
        }
        if bus_us.is_some() {
            self.last_frame_us = bus_us;
        }
        r
    }

    pub fn on_event(&mut self, ev: &RawEvent) -> Vec<BusRecord> {
        let now = Instant::now();
        let bus_us = ev.tick.map(|t| self.clock.update(t, now));
        let t_ms = bus_us.map(|u| u as f64 / 1000.0).unwrap_or_else(|| self.start.elapsed().as_secs_f64() * 1000.0);
        let mut out = Vec::new();
        let sys = |m: &mut Self, cmd: &str, detail: String, error: bool, family: &str| {
            let mut r = m.base(Dir::Sys, bus_us);
            r.family = family.into();
            r.command = cmd.into();
            r.detail = detail;
            r.error = error;
            r
        };
        use EventKind::*;
        match &ev.kind {
            Sent { id, bits, value } => {
                let d = self.ann.forward(*bits, *value, t_ms);
                self.ours.insert(*id, if d.query { d.key } else { None });
                let mut r = self.frame_record(Dir::Tx, bus_us, *bits, *value, d);
                r.frame_id = Some(*id);
                out.push(r);
            }
            Answer { id, value } => {
                let key = self.ours.get(id).copied().flatten();
                let _ = self.ann.take_query();
                let mut r = self.frame_record(
                    Dir::Ans,
                    bus_us,
                    8,
                    *value as u32,
                    Decoded {
                        family: Family::Backward,
                        target: "←".into(),
                        command: "ANSWER".into(),
                        detail: interpret_answer(key, *value),
                        config: false,
                        query: false,
                        key: None,
                    },
                );
                r.frame_id = Some(*id);
                out.push(r);
            }
            AnswerNo { id } => {
                let _ = self.ann.take_query();
                let mut r = self.base(Dir::Ans, bus_us);
                r.family = "backward".into();
                r.target = "←".into();
                r.command = "NO ANSWER".into();
                r.frame_id = Some(*id);
                out.push(r);
            }
            AnswerCollision { id } => {
                let _ = self.ann.take_query();
                let mut r = self.base(Dir::Ans, bus_us);
                r.family = "backward".into();
                r.target = "←".into();
                r.command = "ANSWER COLLISION".into();
                r.detail = "several devices answered (counts as YES)".into();
                r.frame_id = Some(*id);
                out.push(r);
            }
            SendCollision { id } | SendBusError { id } | SendTimeout { id } => {
                let what = match &ev.kind {
                    SendCollision { .. } => "SEND FAILED: collision",
                    SendBusError { .. } => "SEND FAILED: bus error",
                    _ => "SEND FAILED: timeout",
                };
                let mut r = sys(self, what, format!("frame #{id}"), true, "error");
                r.frame_id = Some(*id);
                out.push(r);
            }
            Received { bits, value } => {
                let d = if *bits == 8 { self.ann.backward(*value as u8, t_ms) } else { self.ann.forward(*bits, *value, t_ms) };
                out.push(self.frame_record(Dir::Rx, bus_us, *bits, *value, d));
            }
            StartStopOnly => out.push(sys(self, "INVALID FRAME", "start/stop bits only".into(), true, "error")),
            FramingError => out.push(sys(self, "FRAMING ERROR", "malformed Manchester timing".into(), true, "error")),
            BusError => out.push(sys(self, "BUS ERROR", "bus low ≈ 42.5 ms".into(), true, "status")),
            SystemError => out.push(sys(self, "SYSTEM FAILURE", "bus low ≥ 500 ms (no bus power?)".into(), true, "status")),
            BusRestored => out.push(sys(self, "BUS RESTORED", "bus high again".into(), false, "status")),
            BusSupplyWarning => out.push(sys(self, "BUS SUPPLY WARNING", "bus low ≈ 22.5 ms".into(), true, "status")),
            SendBufferFull => out.push(sys(self, "SEND BUFFER FULL", String::new(), false, "buffer")),
            SendBufferEmpty => out.push(sys(self, "SEND BUFFER EMPTY", String::new(), false, "buffer")),
            AutoAnswerSent { value } => {
                let mut r = self.frame_record(
                    Dir::Tx,
                    bus_us,
                    8,
                    *value as u32,
                    Decoded {
                        family: Family::Backward,
                        target: "→".into(),
                        command: "AUTO ANSWER".into(),
                        detail: format!("0x{value:02X}"),
                        config: false,
                        query: false,
                        key: None,
                    },
                );
                r.family = "backward".into();
                out.push(r);
            }
            MacroStopped { macro_cmd } => out.push(sys(self, &format!("MACRO {} STOPPED", macro_name(*macro_cmd)), String::new(), true, "macro")),
            MacroIntermediate { macro_cmd, data } => out.push(sys(self, &format!("MACRO {}", macro_name(*macro_cmd)), macro_detail(*macro_cmd, data), false, "macro")),
            MacroError { macro_cmd, data } => out.push(sys(self, &format!("MACRO {} FAILED", macro_name(*macro_cmd)), hex(data), true, "macro")),
            MacroSuccess { macro_cmd, data } => out.push(sys(self, &format!("MACRO {} DONE", macro_name(*macro_cmd)), hex(data), false, "macro")),
            Unknown { status, data } => out.push(sys(self, "UNKNOWN EVENT", format!("status 0x{status:02X} {}", hex(data)), false, "status")),
        }
        out
    }
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
}

pub fn macro_name(c: u8) -> &'static str {
    match c {
        0x84 => "DEVICE SEARCH",
        0x86 => "FIND DUPLICATES",
        0x8A => "ADDRESSING",
        0x8C => "DELETE ADDRESS",
        0x94 => "READ DEVICE TYPES",
        0x96 => "MEMORY BANK",
        0x9E => "FADE",
        0xA4 => "DT8 PARAMETER",
        0xB8 => "DT8 SET RGB",
        0xBA => "DT8 SET TC",
        0x0A => "NEW ADDRESS (16-bit)",
        0x0B => "NEW ADDRESS (24-bit)",
        0x01 | 0x02 => "START QUIESCENT",
        0x0D | 0x0E => "STOP QUIESCENT",
        0x10 => "MEMORY ACCESS",
        0x90 => "RECOGNISE DEVICE",
        _ => "?",
    }
}

fn macro_detail(c: u8, d: &[u8]) -> String {
    let proto = |p: u8| match p >> 6 {
        1 => "16-bit",
        2 => "24-bit",
        _ => "",
    };
    match (c, d) {
        (0x84, [p, a, ..]) => format!("found {} {}{}", proto(*p), if *p >> 6 == 2 { "D" } else { "A" }, a & 0x3F),
        (0x86, [p, a, ..]) => format!("duplicate {} address {}", proto(*p), a & 0x3F),
        (0x0A | 0x0B, [p, a, ..]) => format!("assigned {} {}{}", proto(*p), if c == 0x0B { "D" } else { "A" }, a & 0x3F),
        _ => hex(d),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::RawEvent;

    fn ev(tick: u16, kind: EventKind) -> RawEvent {
        RawEvent { tick: Some(tick), line: Some(0), kind }
    }

    #[test]
    fn pairs_answer_with_query_and_computes_delta() {
        let mut m = Monitor::new();
        m.set_tick_us(100);
        let a = m.on_event(&ev(1000, EventKind::Sent { id: 7, bits: 16, value: 0x0599 }));
        assert_eq!(a[0].command, "QUERY DEVICE TYPE");
        assert_eq!(a[0].raw, "05 99");
        let b = m.on_event(&ev(1120, EventKind::Answer { id: 7, value: 6 }));
        assert_eq!(b[0].detail, "DT6 LED");
        assert_eq!(b[0].delta_ms, Some(12.0));
    }

    #[test]
    fn hex_grouping() {
        assert_eq!(hex_frame(24, 0x01FE30), "01 FE 30");
        assert_eq!(hex_frame(8, 0x5), "05");
        assert_eq!(hex_frame(25, 0x1ABCDEF), "1 AB CD EF");
    }

    #[test]
    fn tick_wraps() {
        let mut c = TickClock { tick_us: 100, ..Default::default() };
        let t0 = Instant::now();
        c.update(65000, t0);
        let v = c.update(100, t0 + std::time::Duration::from_millis(64));
        assert_eq!(v, (636) * 100);
    }
}
