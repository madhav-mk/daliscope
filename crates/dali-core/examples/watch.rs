//! Headless bus monitor: `cargo run -p dali-core --example watch [port]`.
//! Without a port it auto-detects a Lunatone interface (plug-and-play).

use dali_core::dongle::{Dongle, DongleEvent, ManagerConfig};

fn main() {
    let port = std::env::args().nth(1);
    let simulate = port.as_deref() == Some("sim");
    let (_d, rx) = Dongle::start(ManagerConfig {
        pinned_port: port.filter(|p| p != "sim"),
        simulate,
        ..Default::default()
    });
    for ev in rx {
        match ev {
            DongleEvent::State(s) => eprintln!("[state] {}", serde_json_lite(&s)),
            DongleEvent::Log { level, message } => eprintln!("[{level}] {message}"),
            DongleEvent::Diag(g) => {
                if g.line.is_some_and(|l| l.bus_voltage_error) {
                    eprintln!("[diag] no DALI bus voltage");
                }
            }
            DongleEvent::Record(r) => println!(
                "{:>10} {:>7} {:<4} {:<9} {:<10} {:<40} {}",
                r.bus_us.map(|u| format!("{:.1}", u as f64 / 1000.0)).unwrap_or_default(),
                r.delta_ms.map(|d| format!("+{d:.1}")).unwrap_or_default(),
                format!("{:?}", r.dir).to_uppercase(),
                r.raw,
                r.target,
                r.command,
                r.detail
            ),
        }
    }
}

fn serde_json_lite(s: &dali_core::ConnState) -> String {
    match s {
        dali_core::ConnState::Connected { info } => format!("connected {} ({})", dali_core::dongle::display_name(info), info.port),
        other => format!("{other:?}"),
    }
}
