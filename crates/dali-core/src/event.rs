//! Parsing of LUBA EVENT MESSAGE (0x31) packets.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    /// Our frame went out on the bus (event type 0, info = bit count).
    Sent { id: u8, bits: u8, value: u32 },
    /// The interface sent a configured auto answer.
    AutoAnswerSent { value: u8 },
    SendCollision { id: u8 },
    SendBusError { id: u8 },
    SendTimeout { id: u8 },
    /// Backward frame handling for one of our queries (event type 1).
    AnswerNo { id: u8 },
    Answer { id: u8, value: u8 },
    AnswerCollision { id: u8 },
    /// A frame seen on the bus that we did not send (event type 2).
    Received { bits: u8, value: u32 },
    StartStopOnly,
    FramingError,
    /// Line and interface events (event type 3).
    BusError,
    SystemError,
    BusRestored,
    SendBufferFull,
    SendBufferEmpty,
    BusSupplyWarning,
    MacroStopped { macro_cmd: u8 },
    MacroIntermediate { macro_cmd: u8, data: Vec<u8> },
    MacroError { macro_cmd: u8, data: Vec<u8> },
    MacroSuccess { macro_cmd: u8, data: Vec<u8> },
    Unknown { status: u8, data: Vec<u8> },
}

impl EventKind {
    /// Frame id this event refers to, if any.
    pub fn frame_id(&self) -> Option<u8> {
        use EventKind::*;
        match self {
            Sent { id, .. }
            | SendCollision { id }
            | SendBusError { id }
            | SendTimeout { id }
            | AnswerNo { id }
            | Answer { id, .. }
            | AnswerCollision { id } => Some(*id),
            _ => None,
        }
    }

    pub fn is_error(&self) -> bool {
        use EventKind::*;
        matches!(
            self,
            SendCollision { .. }
                | SendBusError { .. }
                | SendTimeout { .. }
                | AnswerCollision { .. }
                | FramingError
                | StartStopOnly
                | BusError
                | SystemError
                | BusSupplyWarning
                | MacroError { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RawEvent {
    pub tick: Option<u16>,
    pub line: Option<u8>,
    pub kind: EventKind,
}

/// Interprets up to 4 frame bytes. The spec gives transmit frames in
/// big-endian, but is silent for event payloads (and uses little-endian for
/// CONFIG AUTO ANSWER), so accept either: the correct reading is the one that
/// fits in `bits`.
pub fn frame_value(bytes: &[u8], bits: u8) -> u32 {
    let b = &bytes[..bytes.len().min(4)];
    if b.is_empty() {
        return 0;
    }
    let be = b.iter().fold(0u32, |a, &x| (a << 8) | x as u32);
    let fits = |v: u32| bits >= 32 || v >> bits == 0;
    if fits(be) {
        return be;
    }
    let le = b.iter().rev().fold(0u32, |a, &x| (a << 8) | x as u32);
    if fits(le) {
        le
    } else if bits < 32 {
        be & ((1u32 << bits) - 1)
    } else {
        be
    }
}

pub fn parse(data: &[u8], has_tick: bool, has_line: bool) -> Option<RawEvent> {
    let mut i = 0;
    let tick = if has_tick {
        let t = u16::from_le_bytes([*data.first()?, *data.get(1)?]);
        i += 2;
        Some(t)
    } else {
        None
    };
    let line = if has_line {
        let l = *data.get(i)?;
        i += 1;
        Some(l)
    } else {
        None
    };
    let status = *data.get(i)?;
    let d = &data[i + 1..];
    let etype = status >> 6;
    let info = status & 0x3F;
    let b0 = d.first().copied();
    use EventKind::*;
    let kind = match (etype, info) {
        (0, 1..=32) => Sent { id: b0?, bits: info, value: frame_value(&d[1..], info) },
        (0, 60) => AutoAnswerSent { value: *d.get(1)? },
        (0, 61) => SendCollision { id: b0? },
        (0, 62) => SendBusError { id: b0? },
        (0, 63) => SendTimeout { id: b0? },
        (1, 0) => AnswerNo { id: b0? },
        (1, 8) => Answer { id: b0?, value: *d.get(1)? },
        (1, 63) => AnswerCollision { id: b0? },
        (2, 1..=32) => Received { bits: info, value: frame_value(d, info) },
        (2, 62) => StartStopOnly,
        (2, 63) => FramingError,
        (3, 0) => BusError,
        (3, 1) => SystemError,
        (3, 2) => BusRestored,
        (3, 3) => SendBufferFull,
        (3, 4) => SendBufferEmpty,
        (3, 5) => BusSupplyWarning,
        (3, 60) => MacroStopped { macro_cmd: b0? },
        (3, 61) => MacroIntermediate { macro_cmd: b0?, data: d[1..].to_vec() },
        (3, 62) => MacroError { macro_cmd: b0?, data: d[1..].to_vec() },
        (3, 63) => MacroSuccess { macro_cmd: b0?, data: d[1..].to_vec() },
        _ => Unknown { status, data: d.to_vec() },
    };
    Some(RawEvent { tick, line, kind })
}

/// Builds an event payload (used by the simulator and tests).
pub fn build(tick: u16, line: u8, etype: u8, info: u8, data: &[u8]) -> Vec<u8> {
    let mut v = tick.to_le_bytes().to_vec();
    v.push(line);
    v.push((etype << 6) | (info & 0x3F));
    v.extend_from_slice(data);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sent_and_received() {
        let e = parse(&build(10, 0, 0, 16, &[5, 0, 0, 0xFF, 0xA0]), true, true).unwrap();
        assert_eq!(e.kind, EventKind::Sent { id: 5, bits: 16, value: 0xFFA0 });
        let e = parse(&build(11, 0, 2, 24, &[0, 0x00, 0x0C, 0x03]), true, true).unwrap();
        assert_eq!(e.kind, EventKind::Received { bits: 24, value: 0x000C03 });
        // little-endian payload is detected too
        let e = parse(&build(11, 0, 2, 16, &[0xA0, 0xFF, 0, 0]), true, true).unwrap();
        assert_eq!(e.kind, EventKind::Received { bits: 16, value: 0xFFA0 });
        let e = parse(&build(12, 0, 1, 8, &[5, 0xFE]), true, true).unwrap();
        assert_eq!(e.kind, EventKind::Answer { id: 5, value: 0xFE });
        assert_eq!(e.tick, Some(12));
    }

    #[test]
    fn parses_without_tick_or_line() {
        let e = parse(&[(3 << 6) | 2], false, false).unwrap();
        assert_eq!(e.kind, EventKind::BusRestored);
    }
}
