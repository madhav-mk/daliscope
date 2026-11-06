//! LUBA (Lunatone Universal Building and Automation) protocol framing and
//! command definitions, implemented from the public LUBAP V1.10 specification.
//!
//! Wire format (RS232 / USB VCOM, 38400 8N1):
//!
//! ```text
//! 'Y'(0x59) | CMD | LEN | DATA[LEN] | CHECKSUM (= CMD ^ LEN ^ DATA[0] ^ ...)
//! ```

use serde::Serialize;

pub const SYNC: u8 = 0x59;
pub const BAUD: u32 = 38_400;

/// Command numbers. A response always uses `command + 1`.
pub mod cmd {
    pub const QUERY_DEVICE_INFO: u8 = 0x20;
    pub const IDENTIFY_SELF: u8 = 0x24;
    pub const DEVICE_NAME: u8 = 0x26;
    pub const QUERY_DEVICE_DESCRIPTOR: u8 = 0x28;
    pub const SETTINGS: u8 = 0x2A;
    pub const READ_STATUS: u8 = 0x2C;
    pub const EVENT: u8 = 0x31;
    pub const ADD_FRAME: u8 = 0x32;
    pub const ADD_FRAME_16: u8 = 0x34;
    pub const ADD_FRAME_24: u8 = 0x36;
    pub const ADD_FRAME_EDALI: u8 = 0x38;
    pub const CONFIG_AUTO_ANSWER: u8 = 0x52;
    pub const SHORT_BUS: u8 = 0x54;
    pub const MACRO_BUFFER: u8 = 0x80;
    pub const MACRO_STATUS: u8 = 0x82;
    pub const MACRO_DEVICE_SEARCH: u8 = 0x84;
    pub const MACRO_FIND_DUPLICATES: u8 = 0x86;
    pub const MACRO_ADDRESSING: u8 = 0x8A;
    pub const MACRO_DELETE_ADDRESS: u8 = 0x8C;
    pub const MACRO_READ_DEVICE_TYPES: u8 = 0x94;
    pub const MACRO_MEMORY_BANK: u8 = 0x96;
    pub const MACRO_FADE: u8 = 0x9E;
    pub const MACRO_DT8_PARAM: u8 = 0xA4;
    pub const MACRO_DT8_RGB: u8 = 0xB8;
    pub const MACRO_DT8_TC: u8 = 0xBA;

    pub const fn response(c: u8) -> u8 {
        c.wrapping_add(1)
    }
}

/// One LUBA packet (without sync/len/checksum).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub cmd: u8,
    pub data: Vec<u8>,
}

impl Packet {
    pub fn new(cmd: u8, data: impl Into<Vec<u8>>) -> Self {
        Self { cmd, data: data.into() }
    }

    pub fn checksum(cmd: u8, data: &[u8]) -> u8 {
        data.iter().fold(cmd ^ data.len() as u8, |a, b| a ^ b)
    }

    pub fn encode(&self) -> Vec<u8> {
        assert!(self.data.len() <= 255, "LUBA payload too long");
        let mut out = Vec::with_capacity(self.data.len() + 4);
        out.push(SYNC);
        out.push(self.cmd);
        out.push(self.data.len() as u8);
        out.extend_from_slice(&self.data);
        out.push(Self::checksum(self.cmd, &self.data));
        out
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct DecoderStats {
    pub packets: u64,
    pub checksum_errors: u64,
    pub discarded_bytes: u64,
}

/// Streaming decoder that tolerates garbage, partial reads and bad checksums.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    pub stats: DecoderStats,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Feed raw bytes; returns every complete, checksum-valid packet.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Packet> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            // Align on SYNC.
            match self.buf.iter().position(|&b| b == SYNC) {
                None => {
                    self.stats.discarded_bytes += self.buf.len() as u64;
                    self.buf.clear();
                    break;
                }
                Some(0) => {}
                Some(n) => {
                    self.stats.discarded_bytes += n as u64;
                    self.buf.drain(..n);
                }
            }
            if self.buf.len() < 3 {
                break;
            }
            let len = self.buf[2] as usize;
            let total = 4 + len;
            if self.buf.len() < total {
                break;
            }
            let cmd = self.buf[1];
            let data = &self.buf[3..3 + len];
            if Packet::checksum(cmd, data) == self.buf[3 + len] {
                out.push(Packet::new(cmd, data.to_vec()));
                self.stats.packets += 1;
                self.buf.drain(..total);
            } else {
                // False sync or corruption: skip this sync byte and rescan.
                self.stats.checksum_errors += 1;
                self.stats.discarded_bytes += 1;
                self.buf.drain(..1);
            }
        }
        out
    }

    /// Called when the line has been idle. A partial frame that never
    /// completes (e.g. a stray 'Y' with a large LEN) would otherwise stall the
    /// stream, so drop its sync byte and rescan what's left.
    pub fn on_idle(&mut self) -> Vec<Packet> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        self.stats.discarded_bytes += 1;
        self.buf.drain(..1);
        self.push(&[])
    }
}

pub fn cmd_name(c: u8) -> &'static str {
    match c & !1 {
        cmd::QUERY_DEVICE_INFO => "DEVICE INFO",
        cmd::IDENTIFY_SELF => "IDENTIFY",
        cmd::DEVICE_NAME => "DEVICE NAME",
        cmd::QUERY_DEVICE_DESCRIPTOR => "DEVICE DESCRIPTOR",
        cmd::SETTINGS => "SETTINGS",
        cmd::READ_STATUS => "READ STATUS",
        0x30 => "EVENT",
        cmd::ADD_FRAME => "ADD FRAME",
        cmd::ADD_FRAME_16 => "ADD 16-BIT FRAME",
        cmd::ADD_FRAME_24 => "ADD 24-BIT FRAME",
        cmd::ADD_FRAME_EDALI => "ADD eDALI FRAME",
        cmd::CONFIG_AUTO_ANSWER => "AUTO ANSWER",
        cmd::SHORT_BUS => "SHORT BUS",
        _ if c >= 0x80 => "MACRO",
        _ => "?",
    }
}

// ---------------------------------------------------------------------------
// Request payload builders
// ---------------------------------------------------------------------------

/// Mode byte for ADD ... FRAME TO TX BUFFER.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxMode {
    pub send_twice: bool,
    pub wait_response: bool,
    /// DALI priority 1..=5
    pub priority: u8,
}

impl TxMode {
    pub const fn plain() -> Self {
        Self { send_twice: false, wait_response: false, priority: 3 }
    }
    pub const fn query() -> Self {
        Self { send_twice: false, wait_response: true, priority: 3 }
    }
    pub const fn twice() -> Self {
        Self { send_twice: true, wait_response: false, priority: 3 }
    }
    pub fn byte(&self) -> u8 {
        let mut b = self.priority.clamp(1, 5);
        if self.send_twice {
            b |= 0x80;
        }
        if self.wait_response {
            b |= 0x40;
        }
        b
    }
}

/// Builds an ADD DALI FRAME TO TX BUFFER request. Uses the dedicated 16/24-bit
/// commands where possible and the generic one for other lengths.
pub fn add_frame(line: u8, bits: u8, value: u32, mode: TxMode) -> Packet {
    match bits {
        16 => Packet::new(
            cmd::ADD_FRAME_16,
            vec![line, mode.byte(), (value >> 8) as u8, value as u8],
        ),
        24 => Packet::new(
            cmd::ADD_FRAME_24,
            vec![line, mode.byte(), (value >> 16) as u8, (value >> 8) as u8, value as u8],
        ),
        _ => {
            let v = value.to_be_bytes();
            Packet::new(cmd::ADD_FRAME, vec![line, bits, mode.byte(), v[0], v[1], v[2], v[3]])
        }
    }
}

/// Errors returned in the single-byte response of ADD ... FRAME and macros.
pub fn tx_error_text(code: u8) -> &'static str {
    match code {
        1 => "bus voltage error",
        2 => "interface is in DALI Initialise mode",
        3 => "interface is in DALI Quiescent mode",
        4 => "send buffer full",
        5 => "DALI line not available",
        6 => "syntax error in parameters",
        7 => "a macro is running",
        _ => "unknown error",
    }
}

// ---------------------------------------------------------------------------
// Response parsers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct DeviceDescriptor {
    pub lines: u8,
    pub buffer_size: u8,
    pub tick_us: u32,
    pub max_data_bytes: u8,
    pub implemented_macros: u64,
    pub device_list: u8,
    pub protocol_major: u8,
    pub protocol_minor: u8,
    pub has_bus_power: bool,
    pub can_short_bus: bool,
}

impl DeviceDescriptor {
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < 18 {
            return None;
        }
        let mut macros = [0u8; 8];
        macros.copy_from_slice(&d[7..15]);
        let hw = d.get(18).copied().unwrap_or(0);
        Some(Self {
            lines: d[0],
            buffer_size: d[1],
            tick_us: u32::from_le_bytes([d[2], d[3], d[4], d[5]]),
            max_data_bytes: d[6],
            implemented_macros: u64::from_le_bytes(macros),
            device_list: d[15],
            protocol_major: d[16],
            protocol_minor: d[17],
            has_bus_power: hw & 0x80 != 0,
            can_short_bus: hw & 0x10 != 0,
        })
    }

    /// Macro command numbers are encoded as bit n <=> command 2n + 132.
    pub fn implements_macro(&self, command: u8) -> bool {
        if command < 132 || !(command - 132).is_multiple_of(2) {
            return false;
        }
        let n = (command - 132) / 2;
        n < 64 && self.implemented_macros & (1u64 << n) != 0
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DeviceInfo {
    pub gtin: u64,
    #[serde(serialize_with = "ser_hex_u64")]
    pub serial: u64,
    pub pcb_version: u8,
    pub assembly_version: u8,
    pub article_number: u32,
    pub article_text: String,
    pub production_year: u16,
    pub production_week: u8,
}

impl DeviceInfo {
    pub fn apply_set0(&mut self, d: &[u8]) {
        if d.len() >= 20 {
            self.gtin = d[0..6].iter().fold(0u64, |a, &b| (a << 8) | b as u64);
            self.serial = d[6..14].iter().fold(0u64, |a, &b| (a << 8) | b as u64);
            self.pcb_version = d[14];
            self.assembly_version = d[15];
            self.article_number = u32::from_le_bytes([d[16], d[17], d[18], d[19]]);
        }
    }

    pub fn apply_set1(&mut self, d: &[u8]) {
        if d.len() >= 16 {
            self.article_text = c_string(&d[..16]);
        }
        if d.len() >= 18 {
            self.production_year = 2000 + d[16] as u16;
            self.production_week = d[17];
        }
    }
}

/// 64-bit identifiers exceed JavaScript's safe integer range; send them as hex.
pub fn ser_hex_u64<S: serde::Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("0x{v:X}"))
}

pub fn ser_opt_hex_u64<S: serde::Serializer>(v: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(v) => s.serialize_str(&format!("0x{v:X}")),
        None => s.serialize_none(),
    }
}

pub fn c_string(d: &[u8]) -> String {
    let end = d.iter().position(|&b| b == 0).unwrap_or(d.len());
    String::from_utf8_lossy(&d[..end]).trim().to_string()
}

/// Interface settings (READ / WRITE SETTINGS, 3 bytes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Settings {
    pub mode: u8,
    pub event_filter: u8,
    pub hardware: u8,
}

impl Settings {
    pub fn parse(d: &[u8]) -> Option<Self> {
        (d.len() >= 3).then(|| Self { mode: d[0], event_filter: d[1], hardware: d[2] })
    }
    pub fn bytes(&self) -> [u8; 3] {
        [self.mode, self.event_filter, self.hardware]
    }
    /// Event filter bits (a set bit suppresses that event / field).
    pub const F_ALL: u8 = 0x80;
    pub const F_SENT: u8 = 0x40;
    pub const F_RECEIVED: u8 = 0x20;
    pub const F_BUFFER: u8 = 0x10;
    pub const F_TICK: u8 = 0x08;
    pub const F_LINE: u8 = 0x04;
    pub const F_MACRO: u8 = 0x02;

    /// Event types the monitor can't work without are switched off.
    pub fn events_blocked(&self) -> bool {
        self.event_filter & (Self::F_ALL | Self::F_SENT | Self::F_RECEIVED) != 0
    }

    pub fn bus_power(&self) -> bool {
        self.hardware & 0x80 != 0
    }
    pub fn set_bus_power(&mut self, on: bool) {
        if on {
            self.hardware |= 0x80
        } else {
            self.hardware &= !0x80
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct LineStatus {
    pub tick: u16,
    pub next_id: u8,
    pub queued: u8,
    pub bus_voltage_error: bool,
    pub initialise_blocked: bool,
    pub quiescent_blocked: bool,
}

impl LineStatus {
    pub fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < 6 {
            return None;
        }
        Some(Self {
            tick: u16::from_le_bytes([d[1], d[2]]),
            next_id: d[3],
            queued: d[4],
            bus_voltage_error: d[5] & 0x80 != 0,
            initialise_blocked: d[5] & 0x40 != 0,
            quiescent_blocked: d[5] & 0x20 != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_matches_spec_checksum() {
        // QUERY ACTUAL LEVEL broadcast, from the LUBAP example (2.5.1).
        let p = add_frame(0, 16, 0xFFA0, TxMode::query());
        assert_eq!(p.data, vec![0, 0x43, 0xFF, 0xA0]);
        let e = p.encode();
        assert_eq!(e[0], SYNC);
        assert_eq!(e[1], 0x34);
        assert_eq!(e[2], 4);
        assert_eq!(*e.last().unwrap(), 0x34 ^ 4 ^ 0 ^ 0x43 ^ 0xFF ^ 0xA0);
    }

    #[test]
    fn decoder_handles_garbage_split_and_bad_checksum() {
        let a = Packet::new(0x31, vec![1, 2, 3, 4]).encode();
        let b = Packet::new(0x29, vec![9; 19]).encode();
        let mut bad = Packet::new(0x31, vec![7, 7]).encode();
        *bad.last_mut().unwrap() ^= 0xFF;

        let mut stream = vec![0x00, 0x13, 0x59]; // junk incl. stray sync
        stream.extend(&bad);
        stream.extend(&a);
        stream.extend(&b);

        let mut d = Decoder::new();
        let mut got = Vec::new();
        for chunk in stream.chunks(3) {
            got.extend(d.push(chunk));
        }
        got.extend(d.on_idle());
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].cmd, 0x31);
        assert_eq!(got[1].data.len(), 19);
        assert!(d.stats.checksum_errors >= 1);
    }

    #[test]
    fn descriptor_macro_bits() {
        let mut d = vec![1, 16, 100, 0, 0, 0, 32];
        d.extend([0b0000_0101, 0, 0, 0, 0, 0, 0, 0]); // bit0 -> 132, bit2 -> 136
        d.extend([1, 1, 10, 0x80]);
        let desc = DeviceDescriptor::parse(&d).unwrap();
        assert_eq!(desc.tick_us, 100);
        assert!(desc.implements_macro(132));
        assert!(!desc.implements_macro(134));
        assert!(desc.implements_macro(136));
        assert!(desc.has_bus_power);
    }
}
