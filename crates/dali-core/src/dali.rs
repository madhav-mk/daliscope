//! DALI frame decoding (IEC 62386-102/103/207/209/301/303 command sets) and a
//! stateful annotator for the bus monitor.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    Gear,
    GearSpecial,
    Device,
    DeviceSpecial,
    Event,
    Backward,
    Other,
}

/// Identifies a command so answers can be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CmdKey {
    Gear(u8),
    GearDt(u8, u8),
    GearSpecial(u8),
    Device(u8),
    Instance(u8),
    InstanceType(u8, u8),
    DeviceSpecial(u8),
}

#[derive(Debug, Clone, Serialize)]
pub struct Decoded {
    pub family: Family,
    pub target: String,
    pub command: String,
    pub detail: String,
    pub config: bool,
    pub query: bool,
    #[serde(skip)]
    pub key: Option<CmdKey>,
}

impl Decoded {
    fn new(family: Family, target: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            family,
            target: target.into(),
            command: command.into(),
            detail: String::new(),
            config: false,
            query: false,
            key: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Value helpers
// ---------------------------------------------------------------------------

/// Arc power level to percent on the standard logarithmic curve.
pub fn level_percent(level: u8) -> f64 {
    match level {
        0 => 0.0,
        255 => f64::NAN,
        n => 10f64.powf((n as f64 - 1.0) / (253.0 / 3.0) - 1.0),
    }
}

pub fn level_text(level: u8) -> String {
    match level {
        255 => "MASK".into(),
        0 => "0 (off)".into(),
        n => {
            let p = level_percent(n);
            if p < 1.0 {
                format!("{n} ({p:.2}%)")
            } else {
                format!("{n} ({p:.1}%)")
            }
        }
    }
}

pub fn fade_time_text(n: u8) -> String {
    if n == 0 {
        "0 (extended / none)".into()
    } else {
        format!("{n} ({:.1} s)", 0.5 * 2f64.powf(n as f64 / 2.0))
    }
}

pub fn fade_rate_text(n: u8) -> String {
    if n == 0 {
        "0 (n/a)".into()
    } else {
        format!("{n} ({:.1} steps/s)", 506.0 / 2f64.powf(n as f64 / 2.0))
    }
}

pub fn device_type_name(dt: u8) -> &'static str {
    match dt {
        0 => "fluorescent",
        1 => "emergency",
        2 => "HID",
        3 => "low-voltage halogen",
        4 => "incandescent dimmer",
        5 => "0-10V converter",
        6 => "LED",
        7 => "switching",
        8 => "colour control",
        9 => "sequencer",
        15 => "load referencing",
        16 => "thermal gear protection",
        17 => "dimming curve selection",
        20 => "demand response",
        21 => "thermal lamp protection",
        23 => "non-replaceable light source",
        49 => "integrated bus power supply",
        50 => "memory bank 1 ext.",
        51 => "energy reporting",
        52 => "diagnostics & maintenance",
        254 => "none / end",
        255 => "multiple",
        _ => "unknown",
    }
}

pub fn instance_type_name(t: u8) -> &'static str {
    match t {
        0 => "generic",
        1 => "push button (301)",
        2 => "absolute input (302)",
        3 => "occupancy (303)",
        4 => "light sensor (304)",
        5 => "colour sensor (305)",
        6 => "general purpose sensor (306)",
        _ => "type",
    }
}

fn group_list(v: u8, base: u8) -> String {
    let g: Vec<String> = (0..8).filter(|b| v & (1 << b) != 0).map(|b| (base + b).to_string()).collect();
    if g.is_empty() {
        "none".into()
    } else {
        g.join(",")
    }
}

fn bits_text(v: u8, names: &[&str]) -> String {
    let set: Vec<&str> = names
        .iter()
        .enumerate()
        .filter(|(i, n)| v & (1 << i) != 0 && !n.is_empty())
        .map(|(_, n)| *n)
        .collect();
    if set.is_empty() {
        format!("0x{v:02X} (none)")
    } else {
        format!("0x{v:02X} {}", set.join(", "))
    }
}

// ---------------------------------------------------------------------------
// 16-bit (IEC 62386-102 control gear)
// ---------------------------------------------------------------------------

fn gear_command_name(op: u8) -> Option<&'static str> {
    Some(match op {
        0x00 => "OFF",
        0x01 => "UP",
        0x02 => "DOWN",
        0x03 => "STEP UP",
        0x04 => "STEP DOWN",
        0x05 => "RECALL MAX LEVEL",
        0x06 => "RECALL MIN LEVEL",
        0x07 => "STEP DOWN AND OFF",
        0x08 => "ON AND STEP UP",
        0x09 => "ENABLE DAPC SEQUENCE",
        0x0A => "GO TO LAST ACTIVE LEVEL",
        0x0B => "CONTINUOUS UP",
        0x0C => "CONTINUOUS DOWN",
        0x20 => "RESET",
        0x21 => "STORE ACTUAL LEVEL IN DTR0",
        0x22 => "SAVE PERSISTENT VARIABLES",
        0x23 => "SET OPERATING MODE (DTR0)",
        0x24 => "RESET MEMORY BANK (DTR0)",
        0x25 => "IDENTIFY DEVICE",
        0x2A => "SET MAX LEVEL (DTR0)",
        0x2B => "SET MIN LEVEL (DTR0)",
        0x2C => "SET SYSTEM FAILURE LEVEL (DTR0)",
        0x2D => "SET POWER ON LEVEL (DTR0)",
        0x2E => "SET FADE TIME (DTR0)",
        0x2F => "SET FADE RATE (DTR0)",
        0x30 => "SET EXTENDED FADE TIME (DTR0)",
        0x80 => "SET SHORT ADDRESS (DTR0)",
        0x81 => "ENABLE WRITE MEMORY",
        0x90 => "QUERY STATUS",
        0x91 => "QUERY CONTROL GEAR PRESENT",
        0x92 => "QUERY LAMP FAILURE",
        0x93 => "QUERY LAMP POWER ON",
        0x94 => "QUERY LIMIT ERROR",
        0x95 => "QUERY RESET STATE",
        0x96 => "QUERY MISSING SHORT ADDRESS",
        0x97 => "QUERY VERSION NUMBER",
        0x98 => "QUERY CONTENT DTR0",
        0x99 => "QUERY DEVICE TYPE",
        0x9A => "QUERY PHYSICAL MINIMUM",
        0x9B => "QUERY POWER FAILURE",
        0x9C => "QUERY CONTENT DTR1",
        0x9D => "QUERY CONTENT DTR2",
        0x9E => "QUERY OPERATING MODE",
        0x9F => "QUERY LIGHT SOURCE TYPE",
        0xA0 => "QUERY ACTUAL LEVEL",
        0xA1 => "QUERY MAX LEVEL",
        0xA2 => "QUERY MIN LEVEL",
        0xA3 => "QUERY POWER ON LEVEL",
        0xA4 => "QUERY SYSTEM FAILURE LEVEL",
        0xA5 => "QUERY FADE TIME/FADE RATE",
        0xA6 => "QUERY MANUFACTURER SPECIFIC MODE",
        0xA7 => "QUERY NEXT DEVICE TYPE",
        0xA8 => "QUERY EXTENDED FADE TIME",
        0xAA => "QUERY CONTROL GEAR FAILURE",
        0xC0 => "QUERY GROUPS 0-7",
        0xC1 => "QUERY GROUPS 8-15",
        0xC2 => "QUERY RANDOM ADDRESS (H)",
        0xC3 => "QUERY RANDOM ADDRESS (M)",
        0xC4 => "QUERY RANDOM ADDRESS (L)",
        0xC5 => "READ MEMORY LOCATION (DTR1,DTR0)",
        0xFF => "QUERY EXTENDED VERSION NUMBER",
        _ => return None,
    })
}

fn gear_command(op: u8) -> (String, bool) {
    if let Some(n) = gear_command_name(op) {
        return (n.to_string(), false);
    }
    let s = match op {
        0x10..=0x1F => format!("GO TO SCENE {}", op - 0x10),
        0x40..=0x4F => format!("SET SCENE {} (DTR0)", op - 0x40),
        0x50..=0x5F => format!("REMOVE FROM SCENE {}", op - 0x50),
        0x60..=0x6F => format!("ADD TO GROUP {}", op - 0x60),
        0x70..=0x7F => format!("REMOVE FROM GROUP {}", op - 0x70),
        0xB0..=0xBF => format!("QUERY SCENE LEVEL {}", op - 0xB0),
        0xE0..=0xFE => return (format!("APPLICATION EXTENDED 0x{op:02X}"), true),
        _ => format!("RESERVED 0x{op:02X}"),
    };
    (s, false)
}

fn dt6_command(op: u8) -> Option<&'static str> {
    Some(match op {
        0xE0 => "REFERENCE SYSTEM POWER",
        0xE1 => "ENABLE CURRENT PROTECTOR",
        0xE2 => "DISABLE CURRENT PROTECTOR",
        0xE3 => "SELECT DIMMING CURVE (DTR0)",
        0xE4 => "STORE DTR AS FAST FADE TIME",
        0xED => "QUERY GEAR TYPE",
        0xEE => "QUERY DIMMING CURVE",
        0xEF => "QUERY POSSIBLE OPERATING MODES",
        0xF0 => "QUERY FEATURES",
        0xF1 => "QUERY FAILURE STATUS",
        0xF2 => "QUERY SHORT CIRCUIT",
        0xF3 => "QUERY OPEN CIRCUIT",
        0xF4 => "QUERY LOAD DECREASE",
        0xF5 => "QUERY LOAD INCREASE",
        0xF6 => "QUERY CURRENT PROTECTOR ACTIVE",
        0xF7 => "QUERY THERMAL SHUTDOWN",
        0xF8 => "QUERY THERMAL OVERLOAD",
        0xF9 => "QUERY REFERENCE RUNNING",
        0xFA => "QUERY REFERENCE MEASUREMENT FAILED",
        0xFB => "QUERY CURRENT PROTECTOR ENABLED",
        0xFC => "QUERY OPERATING MODE",
        0xFD => "QUERY FAST FADE TIME",
        0xFE => "QUERY MIN FAST FADE TIME",
        0xFF => "QUERY EXTENDED VERSION NUMBER",
        _ => return None,
    })
}

fn dt8_command(op: u8) -> Option<&'static str> {
    Some(match op {
        0xE0 => "SET TEMPORARY X-COORDINATE",
        0xE1 => "SET TEMPORARY Y-COORDINATE",
        0xE2 => "ACTIVATE",
        0xE3 => "X-COORDINATE STEP UP",
        0xE4 => "X-COORDINATE STEP DOWN",
        0xE5 => "Y-COORDINATE STEP UP",
        0xE6 => "Y-COORDINATE STEP DOWN",
        0xE7 => "SET TEMPORARY COLOUR TEMPERATURE Tc",
        0xE8 => "COLOUR TEMPERATURE Tc STEP COOLER",
        0xE9 => "COLOUR TEMPERATURE Tc STEP WARMER",
        0xEA => "SET TEMPORARY PRIMARY N DIMLEVEL",
        0xEB => "SET TEMPORARY RGB DIMLEVEL",
        0xEC => "SET TEMPORARY WAF DIMLEVEL",
        0xED => "SET TEMPORARY RGBWAF CONTROL",
        0xEE => "COPY REPORT TO TEMPORARY",
        0xF0 => "STORE TY PRIMARY N",
        0xF1 => "STORE XY-COORDINATE PRIMARY N",
        0xF2 => "STORE COLOUR TEMPERATURE Tc LIMIT",
        0xF3 => "STORE GEAR FEATURES/STATUS",
        0xF5 => "ASSIGN COLOUR TO LINKED CHANNEL",
        0xF6 => "START AUTO CALIBRATION",
        0xF7 => "QUERY GEAR FEATURES/STATUS",
        0xF8 => "QUERY COLOUR STATUS",
        0xF9 => "QUERY COLOUR TYPE FEATURES",
        0xFA => "QUERY COLOUR VALUE",
        0xFB => "QUERY RGBWAF CONTROL",
        0xFC => "QUERY ASSIGNED COLOUR",
        0xFF => "QUERY EXTENDED VERSION NUMBER",
        _ => return None,
    })
}

fn gear_special(a: u8) -> Option<(&'static str, bool)> {
    // (name, has meaningful data byte)
    Some(match a {
        0xA1 => ("TERMINATE", false),
        0xA3 => ("DTR0", true),
        0xA5 => ("INITIALISE", true),
        0xA7 => ("RANDOMISE", false),
        0xA9 => ("COMPARE", false),
        0xAB => ("WITHDRAW", false),
        0xAD => ("PING", false),
        0xB1 => ("SEARCHADDRH", true),
        0xB3 => ("SEARCHADDRM", true),
        0xB5 => ("SEARCHADDRL", true),
        0xB7 => ("PROGRAM SHORT ADDRESS", true),
        0xB9 => ("VERIFY SHORT ADDRESS", true),
        0xBB => ("QUERY SHORT ADDRESS", false),
        0xC1 => ("ENABLE DEVICE TYPE", true),
        0xC3 => ("DTR1", true),
        0xC5 => ("DTR2", true),
        0xC7 => ("WRITE MEMORY LOCATION", true),
        0xC9 => ("WRITE MEMORY LOCATION - NO REPLY", true),
        _ => return None,
    })
}

fn gear_target(a: u8) -> Option<String> {
    if a & 0x80 == 0 {
        Some(format!("A{}", (a >> 1) & 0x3F))
    } else if a & 0xE0 == 0x80 {
        Some(format!("G{}", (a >> 1) & 0x0F))
    } else if a >= 0xFE {
        Some("BC".into())
    } else if a >= 0xFC {
        Some("BC unaddr".into())
    } else {
        None
    }
}

fn short_address_param(d: u8) -> String {
    if d == 0xFF {
        "MASK (delete)".into()
    } else if d & 0x81 == 0x01 {
        format!("A{}", (d >> 1) & 0x3F)
    } else {
        format!("0x{d:02X}")
    }
}

pub fn decode16(v: u32, edt: Option<u8>) -> Decoded {
    let a = (v >> 8) as u8;
    let d = v as u8;
    if (0xA0..=0xCB).contains(&a) && a & 1 == 1 {
        let mut x = match gear_special(a) {
            Some((name, has_data)) => {
                let mut x = Decoded::new(Family::GearSpecial, "special", name);
                if has_data {
                    x.detail = match a {
                        0xA5 => match d {
                            0x00 => "all".into(),
                            0xFF => "unaddressed".into(),
                            _ => format!("A{}", (d >> 1) & 0x3F),
                        },
                        0xB7 | 0xB9 => short_address_param(d),
                        0xC1 => format!("DT{d} {}", device_type_name(d)),
                        _ => format!("0x{d:02X} ({d})"),
                    };
                }
                x
            }
            None => Decoded::new(Family::GearSpecial, "special", format!("RESERVED SPECIAL 0x{a:02X}")),
        };
        x.key = Some(CmdKey::GearSpecial(a));
        x.query = matches!(a, 0xA9 | 0xB9 | 0xBB | 0xC7);
        x.config = matches!(a, 0xA5 | 0xA7);
        return x;
    }
    let Some(target) = gear_target(a) else {
        return Decoded::new(Family::Other, "?", format!("RESERVED ADDRESS 0x{a:02X}"));
    };
    if a & 1 == 0 {
        let mut x = Decoded::new(Family::Gear, target, "DAPC");
        x.detail = level_text(d);
        return x;
    }
    let (mut name, app_ext) = gear_command(d);
    let mut key = CmdKey::Gear(d);
    if app_ext {
        let dt_name = match edt {
            Some(6) => dt6_command(d),
            Some(8) => dt8_command(d),
            _ => None,
        };
        if let (Some(n), Some(dt)) = (dt_name, edt) {
            name = format!("DT{dt} {n}");
            key = CmdKey::GearDt(dt, d);
        } else if let Some(dt) = edt {
            name = format!("DT{dt} EXTENDED 0x{d:02X}");
            key = CmdKey::GearDt(dt, d);
        }
    }
    let mut x = Decoded::new(Family::Gear, target, name);
    x.key = Some(key);
    x.config = (0x20..=0x81).contains(&d)
        || (app_ext && edt == Some(6) && (0xE0..=0xE4).contains(&d))
        || (app_ext && edt == Some(8) && (0xF0..=0xF6).contains(&d));
    x.query = (0x90..=0xC5).contains(&d)
        || d == 0xFF
        || (app_ext && edt == Some(6) && d >= 0xED)
        || (app_ext && edt == Some(8) && d >= 0xF7);
    x
}

// ---------------------------------------------------------------------------
// 24-bit (IEC 62386-103 control devices, events)
// ---------------------------------------------------------------------------

fn device_command_name(op: u8) -> Option<&'static str> {
    Some(match op {
        0x00 => "IDENTIFY DEVICE",
        0x01 => "RESET POWER CYCLE SEEN",
        0x10 => "RESET",
        0x11 => "RESET MEMORY BANK (DTR0)",
        0x14 => "SET SHORT ADDRESS (DTR0)",
        0x15 => "ENABLE WRITE MEMORY",
        0x16 => "ENABLE APPLICATION CONTROLLER",
        0x17 => "DISABLE APPLICATION CONTROLLER",
        0x18 => "SET OPERATING MODE (DTR0)",
        0x19 => "ADD TO DEVICE GROUPS 0-15 (DTR2:DTR1)",
        0x1A => "ADD TO DEVICE GROUPS 16-31 (DTR2:DTR1)",
        0x1B => "REMOVE FROM DEVICE GROUPS 0-15 (DTR2:DTR1)",
        0x1C => "REMOVE FROM DEVICE GROUPS 16-31 (DTR2:DTR1)",
        0x1D => "START QUIESCENT MODE",
        0x1E => "STOP QUIESCENT MODE",
        0x1F => "ENABLE POWER CYCLE NOTIFICATION",
        0x20 => "DISABLE POWER CYCLE NOTIFICATION",
        0x21 => "SAVE PERSISTENT VARIABLES",
        0x30 => "QUERY DEVICE STATUS",
        0x31 => "QUERY APPLICATION CONTROLLER ERROR",
        0x32 => "QUERY INPUT DEVICE ERROR",
        0x33 => "QUERY MISSING SHORT ADDRESS",
        0x34 => "QUERY VERSION NUMBER",
        0x35 => "QUERY NUMBER OF INSTANCES",
        0x36 => "QUERY CONTENT DTR0",
        0x37 => "QUERY CONTENT DTR1",
        0x38 => "QUERY CONTENT DTR2",
        0x39 => "QUERY RANDOM ADDRESS (H)",
        0x3A => "QUERY RANDOM ADDRESS (M)",
        0x3B => "QUERY RANDOM ADDRESS (L)",
        0x3C => "READ MEMORY LOCATION (DTR1,DTR0)",
        0x3D => "QUERY APPLICATION CONTROL ENABLED",
        0x3E => "QUERY OPERATING MODE",
        0x3F => "QUERY MANUFACTURER SPECIFIC MODE",
        0x40 => "QUERY QUIESCENT MODE",
        0x41 => "QUERY DEVICE GROUPS 0-7",
        0x42 => "QUERY DEVICE GROUPS 8-15",
        0x43 => "QUERY DEVICE GROUPS 16-23",
        0x44 => "QUERY DEVICE GROUPS 24-31",
        0x45 => "QUERY POWER CYCLE NOTIFICATION",
        0x46 => "QUERY DEVICE CAPABILITIES",
        0x47 => "QUERY EXTENDED VERSION NUMBER (DTR0)",
        0x48 => "QUERY RESET STATE",
        0x49 => "QUERY APPLICATION CONTROLLER ALWAYS ACTIVE",
        _ => return None,
    })
}

fn instance_command_name(op: u8) -> Option<&'static str> {
    Some(match op {
        0x61 => "SET EVENT PRIORITY (DTR0)",
        0x62 => "ENABLE INSTANCE",
        0x63 => "DISABLE INSTANCE",
        0x64 => "SET PRIMARY INSTANCE GROUP (DTR0)",
        0x65 => "SET INSTANCE GROUP 1 (DTR0)",
        0x66 => "SET INSTANCE GROUP 2 (DTR0)",
        0x67 => "SET EVENT SCHEME (DTR0)",
        0x68 => "SET EVENT FILTER (DTR2:DTR1:DTR0)",
        0x80 => "QUERY INSTANCE TYPE",
        0x81 => "QUERY RESOLUTION",
        0x82 => "QUERY INSTANCE ERROR",
        0x83 => "QUERY INSTANCE STATUS",
        0x84 => "QUERY EVENT PRIORITY",
        0x86 => "QUERY INSTANCE ENABLED",
        0x88 => "QUERY PRIMARY INSTANCE GROUP",
        0x89 => "QUERY INSTANCE GROUP 1",
        0x8A => "QUERY INSTANCE GROUP 2",
        0x8B => "QUERY EVENT SCHEME",
        0x8C => "QUERY INPUT VALUE",
        0x8D => "QUERY INPUT VALUE LATCH",
        0x8E => "QUERY FEATURE TYPE",
        0x8F => "QUERY NEXT FEATURE TYPE",
        0x90 => "QUERY EVENT FILTER 0-7",
        0x91 => "QUERY EVENT FILTER 8-15",
        0x92 => "QUERY EVENT FILTER 16-23",
        _ => return None,
    })
}

fn occupancy_command_name(op: u8) -> Option<&'static str> {
    Some(match op {
        0x20 => "CATCH MOVEMENT",
        0x21 => "SET HOLD TIMER (DTR0)",
        0x22 => "SET REPORT TIMER (DTR0)",
        0x23 => "SET DEADTIME TIMER (DTR0)",
        0x24 => "CANCEL HOLD TIMER",
        0x2C => "QUERY DEADTIME TIMER",
        0x2D => "QUERY HOLD TIMER",
        0x2E => "QUERY REPORT TIMER",
        0x2F => "QUERY CATCHING",
        _ => return None,
    })
}

fn device_special(a: u8, i: u8) -> Option<(&'static str, bool)> {
    if a != 0xC1 {
        return None;
    }
    Some(match i {
        0x00 => ("TERMINATE", false),
        0x01 => ("INITIALISE", true),
        0x02 => ("RANDOMISE", false),
        0x03 => ("COMPARE", false),
        0x04 => ("WITHDRAW", false),
        0x05 => ("SEARCHADDRH", true),
        0x06 => ("SEARCHADDRM", true),
        0x07 => ("SEARCHADDRL", true),
        0x08 => ("PROGRAM SHORT ADDRESS", true),
        0x09 => ("VERIFY SHORT ADDRESS", true),
        0x0A => ("QUERY SHORT ADDRESS", false),
        0x20 => ("WRITE MEMORY LOCATION", true),
        0x21 => ("WRITE MEMORY LOCATION - NO REPLY", true),
        0x30 => ("DTR0", true),
        0x31 => ("DTR1", true),
        0x32 => ("DTR2", true),
        0x33 => ("SEND TESTFRAME", true),
        _ => return None,
    })
}

fn device_target(a: u8) -> String {
    if a & 0x80 == 0 {
        format!("D{}", (a >> 1) & 0x3F)
    } else if a & 0xC0 == 0x80 {
        format!("DG{}", (a >> 1) & 0x1F)
    } else if a == 0xFF {
        "BC".into()
    } else if a == 0xFD {
        "BC unaddr".into()
    } else {
        format!("?0x{a:02X}")
    }
}

fn instance_selector(i: u8) -> (String, Option<u8>) {
    // returns (label, instance type if selected by type)
    match i {
        0xFE => ("device".into(), None),
        0xFC => ("feature dev".into(), None),
        0xFD => ("feature I*".into(), None),
        0xFF => ("I*".into(), None),
        _ => match i >> 5 {
            0b000 => (format!("I{}", i & 0x1F), None),
            0b100 => (format!("IG{}", i & 0x1F), None),
            0b110 => (format!("IT{}", i & 0x1F), Some(i & 0x1F)),
            0b001 => (format!("F I{}", i & 0x1F), None),
            0b101 => (format!("F IG{}", i & 0x1F), None),
            0b011 => (format!("F IT{}", i & 0x1F), Some(i & 0x1F)),
            _ => (format!("?0x{i:02X}"), None),
        },
    }
}

fn occupancy_event_text(info: u16) -> String {
    let b = info & 0x0F;
    if info >> 4 != 0 {
        return format!("info 0x{info:03X}");
    }
    format!(
        "{}, {}{}, {} sensor",
        if b & 2 != 0 { "occupied" } else { "vacant" },
        if b & 1 != 0 { "movement" } else { "no movement" },
        if b & 4 != 0 { ", repeat" } else { "" },
        if b & 8 != 0 { "movement" } else { "presence" }
    )
}

fn button_event_text(info: u16) -> String {
    match info {
        0x00 => "button released".into(),
        0x01 => "button pressed".into(),
        0x02 => "short press".into(),
        0x05 => "double press".into(),
        0x09 => "long press start".into(),
        0x0B => "long press repeat".into(),
        0x0C => "long press stop".into(),
        0x0E => "button free".into(),
        0x0F => "button stuck".into(),
        _ => format!("info 0x{info:03X}"),
    }
}

pub fn event_info_text(instance_type: Option<u8>, info: u16) -> String {
    match instance_type {
        Some(1) => button_event_text(info),
        Some(3) => occupancy_event_text(info),
        Some(4) => format!("illuminance value {info}"),
        Some(2) => format!("position {info}"),
        _ => format!("info 0x{info:03X} ({info})"),
    }
}

pub fn decode_event(v: u32) -> Decoded {
    let bit = |n: u32| (v >> n) & 1 == 1;
    let f = |hi: u32, lo: u32| ((v >> lo) & ((1 << (hi - lo + 1)) - 1)) as u8;
    let info = (v & 0x3FF) as u16;
    let (target, itype, scheme) = if !bit(23) && !bit(15) {
        (format!("D{} · IT{}", f(22, 17), f(14, 10)), Some(f(14, 10)), "device")
    } else if !bit(23) && bit(15) {
        (format!("D{} · I{}", f(22, 17), f(14, 10)), None, "device/instance")
    } else if bit(23) && !bit(22) && !bit(15) {
        (format!("DG{} · IT{}", f(21, 17), f(14, 10)), Some(f(14, 10)), "device group")
    } else if bit(23) && !bit(22) && bit(15) {
        (format!("IT{} · I{}", f(21, 17), f(14, 10)), Some(f(21, 17)), "instance")
    } else if bit(23) && bit(22) && !bit(15) {
        (format!("IG{} · IT{}", f(21, 17), f(14, 10)), Some(f(14, 10)), "instance group")
    } else {
        let mut x = Decoded::new(Family::Event, "—", "POWER NOTIFICATION / RESERVED");
        x.detail = format!("raw 0x{v:06X}");
        return x;
    };
    let mut x = Decoded::new(
        Family::Event,
        target,
        format!("EVENT {}", itype.map(instance_type_name).unwrap_or("(type via instance)")).to_uppercase(),
    );
    x.detail = format!("{} · scheme: {scheme}", event_info_text(itype, info));
    x
}

pub fn decode24(v: u32) -> Decoded {
    let a = (v >> 16) as u8;
    let i = (v >> 8) as u8;
    let o = v as u8;
    if a & 1 == 0 {
        return decode_event(v);
    }
    match a {
        0xC1 => {
            let mut x = match device_special(a, i) {
                Some((n, has)) => {
                    let mut x = Decoded::new(Family::DeviceSpecial, "special", n);
                    if has {
                        x.detail = match i {
                            0x01 => match o {
                                0xFF => "all".into(),
                                0x7F => "unaddressed".into(),
                                0..=63 => format!("D{o}"),
                                _ => format!("0x{o:02X}"),
                            },
                            0x08 | 0x09 => {
                                if o == 0xFF {
                                    "MASK (delete)".into()
                                } else {
                                    format!("D{}", o & 0x3F)
                                }
                            }
                            _ => format!("0x{o:02X} ({o})"),
                        };
                    }
                    x
                }
                None => Decoded::new(Family::DeviceSpecial, "special", format!("RESERVED 0xC1 0x{i:02X}")),
            };
            x.key = Some(CmdKey::DeviceSpecial(i));
            x.query = matches!(i, 0x03 | 0x09 | 0x0A | 0x20);
            x.config = matches!(i, 0x01 | 0x02);
            x
        }
        0xC5 => {
            let mut x = Decoded::new(Family::DeviceSpecial, "special", "DIRECT WRITE MEMORY (DTR0)");
            x.detail = format!("offset 0x{i:02X} ← 0x{o:02X}");
            x.query = true;
            x
        }
        0xC7 => {
            let mut x = Decoded::new(Family::DeviceSpecial, "special", "DTR1:DTR0");
            x.detail = format!("DTR1=0x{i:02X} DTR0=0x{o:02X}");
            x
        }
        0xC9 => {
            let mut x = Decoded::new(Family::DeviceSpecial, "special", "DTR2:DTR1");
            x.detail = format!("DTR2=0x{i:02X} DTR1=0x{o:02X}");
            x
        }
        _ => {
            let dev = device_target(a);
            let (sel, sel_type) = instance_selector(i);
            if i == 0xFE {
                let name = device_command_name(o).map(String::from).unwrap_or(format!("DEVICE CMD 0x{o:02X}"));
                let mut x = Decoded::new(Family::Device, dev, name);
                x.key = Some(CmdKey::Device(o));
                x.config = o <= 0x21;
                x.query = o >= 0x30;
                return x;
            }
            let target = format!("{dev} · {sel}");
            if let Some(n) = instance_command_name(o) {
                let mut x = Decoded::new(Family::Device, target, n);
                x.key = Some(CmdKey::Instance(o));
                x.config = (0x61..=0x68).contains(&o);
                x.query = o >= 0x80;
                return x;
            }
            // Instance-type specific (0x00..0x5F)
            let ty = sel_type.unwrap_or(3);
            let name = match ty {
                3 => occupancy_command_name(o).map(|n| {
                    if sel_type.is_some() {
                        n.to_string()
                    } else {
                        format!("{n} (303)")
                    }
                }),
                _ => None,
            }
            .unwrap_or(format!("INSTANCE CMD 0x{o:02X}"));
            let mut x = Decoded::new(Family::Device, target, name);
            x.key = Some(CmdKey::InstanceType(ty, o));
            x.config = (0x21..=0x23).contains(&o);
            x.query = (0x2C..=0x2F).contains(&o);
            x
        }
    }
}

pub fn decode(bits: u8, value: u32, edt: Option<u8>) -> Decoded {
    match bits {
        16 => decode16(value, edt),
        24 => decode24(value),
        8 => {
            let mut x = Decoded::new(Family::Backward, "←", "BACKWARD FRAME");
            x.detail = format!("0x{:02X} ({})", value as u8, value as u8);
            x
        }
        25 => Decoded::new(Family::Other, "eDALI", format!("eDALI 0x{value:07X}")),
        17 => Decoded::new(Family::Other, "17-bit", format!("17-bit frame 0x{value:05X}")),
        n => Decoded::new(Family::Other, format!("{n}-bit"), format!("{n}-bit frame 0x{value:X}")),
    }
}

// ---------------------------------------------------------------------------
// Answer interpretation
// ---------------------------------------------------------------------------

pub fn interpret_answer(key: Option<CmdKey>, v: u8) -> String {
    let yes = || if v == 0xFF { "YES".to_string() } else { format!("0x{v:02X}") };
    let Some(key) = key else { return format!("0x{v:02X} ({v})") };
    match key {
        CmdKey::Gear(op) => match op {
            0x90 => bits_text(
                v,
                &["gear failure", "lamp failure", "lamp on", "limit error", "fade running", "reset state", "short address MASK", "power cycle seen"],
            ),
            0x91..=0x96 | 0x9B => yes(),
            0x97 => format!("{}.{}", v >> 2, v & 3),
            0x99 | 0xA7 => format!("DT{v} {}", device_type_name(v)),
            0xA0..=0xA4 | 0x9A | 0xB0..=0xBF => level_text(v),
            0xA5 => format!("fade time {} · fade rate {}", fade_time_text(v >> 4), fade_rate_text(v & 0x0F)),
            0xA8 => format!("base {} × multiplier {}", v & 0x0F, (v >> 4) & 7),
            0xC0 => format!("groups {}", group_list(v, 0)),
            0xC1 => format!("groups {}", group_list(v, 8)),
            _ => format!("0x{v:02X} ({v})"),
        },
        CmdKey::GearSpecial(0xBB) => format!("A{}", (v >> 1) & 0x3F),
        CmdKey::GearSpecial(_) => yes(),
        CmdKey::DeviceSpecial(0x0A) => format!("D{}", v & 0x3F),
        CmdKey::DeviceSpecial(_) => yes(),
        CmdKey::Device(op) => match op {
            0x30 => bits_text(v, &["input device error", "quiescent mode", "short address MASK", "app. active", "app. controller error", "power cycle seen", "reset state", ""]),
            0x34 => format!("{}.{}", v >> 2, v & 3),
            0x35 => format!("{v} instance(s)"),
            0x41 => format!("groups {}", group_list(v, 0)),
            0x42 => format!("groups {}", group_list(v, 8)),
            0x43 => format!("groups {}", group_list(v, 16)),
            0x44 => format!("groups {}", group_list(v, 24)),
            0x46 => bits_text(v, &["application controller present", "number of instances > 0", "app. controller always active", "", "", "", "", ""]),
            0x31..=0x33 | 0x3D | 0x40 | 0x45 | 0x48 | 0x49 => yes(),
            _ => format!("0x{v:02X} ({v})"),
        },
        CmdKey::Instance(op) => match op {
            0x80 => format!("{v} = {}", instance_type_name(v)),
            0x86 => yes(),
            0x8B => format!(
                "{v} = {}",
                match v {
                    0 => "instance",
                    1 => "device",
                    2 => "device/instance",
                    3 => "device group",
                    4 => "instance group",
                    _ => "?",
                }
            ),
            0x83 => bits_text(v, &["instance error", "instance active", "", "", "", "", "", ""]),
            _ => format!("0x{v:02X} ({v})"),
        },
        CmdKey::InstanceType(3, op) => match op {
            0x2C => format!("{v} × 50 ms = {} ms", v as u32 * 50),
            0x2D => format!("{v} × 10 s = {} s", v as u32 * 10),
            0x2E => format!("{v} s"),
            0x2F => yes(),
            _ => format!("0x{v:02X} ({v})"),
        },
        _ => format!("0x{v:02X} ({v})"),
    }
}

// ---------------------------------------------------------------------------
// Stateful annotator
// ---------------------------------------------------------------------------

/// Follows bus context (DTRs, ENABLE DEVICE TYPE, repeated config frames,
/// query/answer pairing) to enrich decoded frames.
#[derive(Debug, Default)]
pub struct Annotator {
    dtr: [Option<u8>; 3],
    edt: Option<u8>,
    last_forward: Option<(u8, u32, f64)>,
    last_query: Option<(Option<CmdKey>, f64)>,
}

impl Annotator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Decode a forward frame seen at time `t_ms` (bus-relative ms).
    pub fn forward(&mut self, bits: u8, value: u32, t_ms: f64) -> Decoded {
        let edt = self.edt.take();
        let mut x = decode(bits, value, edt);

        // Send-twice detection: identical config frame within 100 ms.
        let repeat = matches!(self.last_forward, Some((b, v, t)) if b == bits && v == value && t_ms - t <= 100.0);
        self.last_forward = Some((bits, value, t_ms));

        // DTR context.
        let d0 = |s: &Self| s.dtr[0].map(|v| format!("DTR0 = {v}")).unwrap_or_default();
        match (bits, x.key) {
            (16, Some(CmdKey::GearSpecial(a))) => match a {
                0xA3 => self.dtr[0] = Some(value as u8),
                0xC3 => self.dtr[1] = Some(value as u8),
                0xC5 => self.dtr[2] = Some(value as u8),
                0xC1 => self.edt = Some(value as u8),
                _ => {}
            },
            (24, Some(CmdKey::DeviceSpecial(i))) => match i {
                0x30 => self.dtr[0] = Some(value as u8),
                0x31 => self.dtr[1] = Some(value as u8),
                0x32 => self.dtr[2] = Some(value as u8),
                _ => {}
            },
            (16, Some(CmdKey::Gear(op))) if x.command.contains("(DTR0)") => {
                let ctx = match op {
                    0x2A..=0x2D | 0x40..=0x4F => self.dtr[0].map(level_text).map(|s| format!("DTR0 = {s}")),
                    0x2E => self.dtr[0].map(fade_time_text).map(|s| format!("DTR0 = {s}")),
                    0x2F => self.dtr[0].map(fade_rate_text).map(|s| format!("DTR0 = {s}")),
                    0x80 => self.dtr[0].map(short_address_param).map(|s| format!("DTR0 = {s}")),
                    _ => Some(d0(self)),
                };
                x.detail = ctx.unwrap_or_default();
            }
            (24, Some(CmdKey::Device(_) | CmdKey::Instance(_) | CmdKey::InstanceType(..))) if x.command.contains("(DTR") => {
                let parts: Vec<String> = (0..3)
                    .filter(|i| x.command.contains(&format!("DTR{i}")))
                    .filter_map(|i| self.dtr[i].map(|v| format!("DTR{i} = {v}")))
                    .collect();
                x.detail = parts.join(" · ");
            }
            _ => {}
        }
        if repeat && x.config {
            x.detail = if x.detail.is_empty() { "2nd (send-twice)".into() } else { format!("{} · 2nd (send-twice)", x.detail) };
        }
        if x.query {
            self.last_query = Some((x.key, t_ms));
        } else if bits == 16 || bits == 24 {
            self.last_query = None;
        }
        x
    }

    /// Interpret a backward frame seen on the bus (from another master's query).
    pub fn backward(&mut self, value: u8, t_ms: f64) -> Decoded {
        let mut x = decode(8, value as u32, None);
        if let Some((key, t)) = self.last_query.take() {
            if t_ms - t < 40.0 {
                x.command = "ANSWER".into();
                x.detail = interpret_answer(key, value);
            }
        }
        x
    }

    /// Key of the most recent query (for pairing our own answers).
    pub fn take_query(&mut self) -> Option<CmdKey> {
        self.last_query.take().and_then(|(k, _)| k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gear_frames() {
        let x = decode16(0xFFA0, None);
        assert_eq!(x.target, "BC");
        assert_eq!(x.command, "QUERY ACTUAL LEVEL");
        assert!(x.query);
        let x = decode16(0x0AFE, None);
        assert_eq!(x.target, "A5");
        assert_eq!(x.command, "DAPC");
        let x = decode16(0x8361, None);
        assert_eq!(x.target, "G1");
        assert_eq!(x.command, "ADD TO GROUP 1");
        assert!(x.config);
        let x = decode16(0xA500, None);
        assert_eq!(x.command, "INITIALISE");
        assert_eq!(x.detail, "all");
    }

    #[test]
    fn dt8_context() {
        let mut a = Annotator::new();
        a.forward(16, 0xC108, 0.0);
        let x = a.forward(16, 0xFFE2, 10.0);
        assert_eq!(x.command, "DT8 ACTIVATE");
        let x = a.forward(16, 0xFFE2, 20.0);
        assert!(x.command.starts_with("APPLICATION EXTENDED"));
    }

    #[test]
    fn device_commands_and_events() {
        let x = decode24(0x01FE30);
        assert_eq!(x.target, "D0");
        assert_eq!(x.command, "QUERY DEVICE STATUS");
        let x = decode24(0x01008B);
        assert_eq!(x.target, "D0 · I0");
        assert_eq!(x.command, "QUERY EVENT SCHEME");
        let x = decode24(0x01002C);
        assert!(x.command.starts_with("QUERY DEADTIME TIMER"));
        // Device-scheme occupancy event, as sent by a Part 303 sensor.
        let x = decode24(0x000C0B);
        assert_eq!(x.family, Family::Event);
        assert_eq!(x.target, "D0 · IT3");
        assert!(x.detail.starts_with("occupied, movement"), "{}", x.detail);
        // Instance-scheme event
        let x = decode24(0x868003);
        assert_eq!(x.target, "IT3 · I0");
        let x = decode24(0xC13042);
        assert_eq!(x.command, "DTR0");
    }

    #[test]
    fn dtr_annotation_and_answers() {
        let mut a = Annotator::new();
        a.forward(16, 0xA3C8, 0.0);
        let x = a.forward(16, 0x032A, 20.0);
        assert_eq!(x.detail, "DTR0 = 200 (22.9%)");
        let x = a.forward(16, 0x032A, 40.0);
        assert!(x.detail.ends_with("2nd (send-twice)"));
        a.forward(16, 0x0399, 60.0);
        let b = a.backward(6, 70.0);
        assert_eq!(b.detail, "DT6 LED");
    }

    #[test]
    fn level_curve() {
        assert!((level_percent(254) - 100.0).abs() < 0.01);
        assert!((level_percent(1) - 0.1).abs() < 0.001);
    }
}
