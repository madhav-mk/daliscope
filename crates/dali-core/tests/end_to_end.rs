//! End-to-end: the real connection manager talking LUBA to the simulator.

use dali_core::dongle::{Dongle, DongleEvent, ManagerConfig};
use dali_core::luba::TxMode;
use dali_core::ops::{self, AddressingMode, Ctx, DaliBus, GearParam, Proto};
use dali_core::sim::{Sim, SimLink};
use dali_core::{ConnState, FrameResult};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

fn connected_sim() -> (Dongle, crossbeam_channel::Receiver<DongleEvent>) {
    let (d, rx) = Dongle::start(ManagerConfig { simulate: true, ..Default::default() });
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(d.state(), ConnState::Connected { .. }) {
            return (d, rx);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("simulator never connected: {:?}", d.state());
}

#[test]
fn connects_and_queries() {
    let (d, rx) = connected_sim();
    let info = d.info().unwrap();
    assert_eq!(info.info.article_text, "DALI-2 USB 30mA");
    assert_eq!(info.descriptor.tick_us, 100);
    assert_eq!(info.settings.event_filter, 0, "manager must enable all events");

    // QUERY DEVICE TYPE on A2 (DT6+DT8) -> MASK (multiple)
    let r = d.transact(16, 0x0599, TxMode::query()).unwrap();
    assert_eq!(r, FrameResult::Answer(0xFF));
    let r = d.transact(16, 0x7F99, TxMode::query()).unwrap(); // A63: nobody
    assert_eq!(r, FrameResult::NoAnswer);
    // Broadcast "present" -> collision
    let r = d.transact(16, 0xFF91, TxMode::query()).unwrap();
    assert_eq!(r, FrameResult::Collision);

    // Monitor got decoded records for our traffic.
    let recs: Vec<_> = rx.try_iter().filter_map(|e| if let DongleEvent::Record(r) = e { Some(r) } else { None }).collect();
    assert!(recs.iter().any(|r| r.command == "QUERY DEVICE TYPE" && r.target == "A2"), "{recs:#?}");
    assert!(recs.iter().any(|r| r.command == "ANSWER" && r.detail.contains("multiple")));
    assert!(recs.iter().any(|r| r.command == "NO ANSWER"));
    d.shutdown();
}

#[test]
fn reads_and_writes_gear_parameters() {
    let (d, _rx) = connected_sim();
    let cancel = AtomicBool::new(false);
    let ctx = Ctx { bus: &d, cancel: &cancel, progress: &|_| {} };
    let g = ops::read_gear(&ctx, 2, true).unwrap();
    assert_eq!(g.device_types, vec![6, 8]);
    assert_eq!(g.groups, Some(0b10));
    assert_eq!(g.dt8_colour_temperature, Some(370));
    let b0 = g.bank0.unwrap();
    assert_eq!(b0.firmware, Some((2, 3)));

    ops::write_gear(&d, ops::gear::short(2), GearParam::MaxLevel, 200).unwrap();
    ops::write_gear(&d, ops::gear::short(2), GearParam::Scene(3), 120).unwrap();
    ops::write_gear(&d, ops::gear::short(2), GearParam::Group(5), 1).unwrap();
    let g = ops::read_gear(&ctx, 2, false).unwrap();
    assert_eq!(g.max_level, Some(200));
    assert_eq!(g.scenes[3], Some(120));
    assert_eq!(g.groups, Some(0b10_0010));
    d.shutdown();
}

#[test]
fn reads_and_writes_device_parameters() {
    let (d, _rx) = connected_sim();
    let cancel = AtomicBool::new(false);
    let ctx = Ctx { bus: &d, cancel: &cancel, progress: &|_| {} };
    let dv = ops::read_device(&ctx, 0, true).unwrap();
    assert_eq!(dv.instance_count, Some(1));
    let i0 = &dv.instances[0];
    assert_eq!(i0.instance_type, Some(3));
    assert_eq!(i0.event_scheme, Some(1));
    assert_eq!(i0.occupancy.as_ref().unwrap().deadtime, Some(2));
    ops::write_device(&d, 0, ops::DeviceParam::Deadtime { instance: 0, value: 9 }).unwrap();
    ops::write_device(&d, 0, ops::DeviceParam::EventScheme { instance: 0, value: 0 }).unwrap();
    ops::write_device(&d, 0, ops::DeviceParam::Group { group: 20, member: true }).unwrap();
    let dv = ops::read_device(&ctx, 0, false).unwrap();
    assert_eq!(dv.instances[0].occupancy.as_ref().unwrap().deadtime, Some(9));
    assert_eq!(dv.instances[0].event_scheme, Some(0));
    assert_eq!(dv.groups, Some(1 << 20));
    d.shutdown();
}

#[test]
fn addressing_new_installation_and_extension() {
    // Run the algorithm straight against the simulator bus (fast, no manager).
    struct Direct(std::sync::Mutex<SimLink>);
    impl DaliBus for Direct {
        fn send(&self, bits: u8, value: u32, twice: bool) -> dali_core::dongle::Result<()> {
            self.tx(bits, value, if twice { 0x83 } else { 0x03 });
            Ok(())
        }
        fn query(&self, bits: u8, value: u32) -> dali_core::dongle::Result<FrameResult> {
            Ok(self.tx(bits, value, 0x43))
        }
    }
    impl Direct {
        fn tx(&self, bits: u8, value: u32, mode: u8) -> FrameResult {
            let mut l = self.0.lock().unwrap();
            let mut tm = TxMode::plain();
            tm.send_twice = mode & 0x80 != 0;
            tm.wait_response = mode & 0x40 != 0;
            l.input(&dali_core::luba::add_frame(0, bits, value, tm).encode());
            // Fast-forward: everything the sim scheduled is due within ~100 ms.
            let mut dec = dali_core::luba::Decoder::new();
            let deadline = Instant::now() + Duration::from_millis(400);
            let mut result = FrameResult::Sent;
            loop {
                let out = l.output();
                for p in dec.push(&out) {
                    if p.cmd == 0x31 {
                        let ev = dali_core::event::parse(&p.data, true, true).unwrap();
                        use dali_core::event::EventKind::*;
                        match ev.kind {
                            Answer { value, .. } => return FrameResult::Answer(value),
                            AnswerNo { .. } => return FrameResult::NoAnswer,
                            AnswerCollision { .. } => return FrameResult::Collision,
                            Sent { .. } if !tm.wait_response => result = FrameResult::Sent,
                            _ => {}
                        }
                    }
                }
                if l.sim.next_due().is_none() && (!tm.wait_response) {
                    return result;
                }
                if Instant::now() > deadline {
                    return result;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    let bus = Direct(std::sync::Mutex::new(SimLink::new(Sim::unaddressed(5, 3))));
    let cancel = AtomicBool::new(false);
    let ctx = Ctx { bus: &bus, cancel: &cancel, progress: &|_| {} };
    let rep = ops::address(&ctx, Proto::Gear, AddressingMode::NewInstallation, &[]).unwrap();
    assert_eq!(rep.assigned.len(), 5, "{rep:?}");
    let rep = ops::address(&ctx, Proto::Device, AddressingMode::NewInstallation, &[]).unwrap();
    assert_eq!(rep.assigned.len(), 3, "{rep:?}");
    let mut g: Vec<_> = bus.0.lock().unwrap().sim.gear_addresses().into_iter().map(Option::unwrap).collect();
    g.sort();
    assert_eq!(g, vec![0, 1, 2, 3, 4]);
    let mut dv: Vec<_> = bus.0.lock().unwrap().sim.device_addresses().into_iter().map(Option::unwrap).collect();
    dv.sort();
    assert_eq!(dv, vec![0, 1, 2]);

    // Delete one, then extension only addresses that one, keeping the rest.
    ops::delete_address(&bus, Proto::Gear, 3).unwrap();
    let rep = ops::address(&ctx, Proto::Gear, AddressingMode::Extension, &[]).unwrap();
    assert_eq!(rep.existing.len(), 4);
    assert_eq!(rep.assigned.len(), 1);
    assert_eq!(rep.assigned[0].0, 3);
}

#[test]
fn scan_finds_demo_devices() {
    let (d, _rx) = connected_sim();
    let cancel = AtomicBool::new(false);
    let ctx = Ctx { bus: &d, cancel: &cancel, progress: &|_| {} };
    let g = ops::scan(&ctx, Proto::Gear).unwrap();
    assert_eq!(g.iter().map(|e| e.address).collect::<Vec<_>>(), vec![0, 1, 2]);
    let dv = ops::scan(&ctx, Proto::Device).unwrap();
    assert_eq!(dv.iter().map(|e| e.address).collect::<Vec<_>>(), vec![0]);
    d.shutdown();
}

fn wait_connected(d: &Dongle) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !matches!(d.state(), ConnState::Connected { .. }) {
        assert!(Instant::now() < deadline, "never connected: {:?}", d.state());
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn unpowered_bus_reports_voltage_error_until_power_is_on() {
    let (d, rx) = Dongle::start_with_sim(Sim::demo().without_psu());
    wait_connected(&d);
    let e = d.transact(16, 0xFF91, TxMode::query()).unwrap_err();
    assert!(e.to_string().contains("bus voltage"), "{e}");
    assert!(d.read_status().unwrap().bus_voltage_error);

    // Diagnostics flag the missing voltage.
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut saw = false;
    while Instant::now() < deadline && !saw {
        if let Ok(DongleEvent::Diag(g)) = rx.recv_timeout(Duration::from_millis(200)) {
            saw = g.line.is_some_and(|l| l.bus_voltage_error);
        }
    }
    assert!(saw, "no diag with bus_voltage_error");

    // Integrated supply on -> the bus works.
    assert!(d.set_bus_power(true).unwrap().bus_power());
    assert!(!d.read_status().unwrap().bus_voltage_error);
    assert_eq!(d.transact(16, 0xFF91, TxMode::query()).unwrap(), FrameResult::Collision);
    d.shutdown();
}

#[test]
fn events_without_tick_and_line_still_parse() {
    // Firmware that refuses to change its filter and omits tick + line.
    let mut sim = Sim::demo();
    sim.spontaneous = false;
    sim.lock_settings = true;
    sim_set_filter(&mut sim, 0x0C);
    let (d, rx) = Dongle::start_with_sim(sim);
    wait_connected(&d);
    assert_eq!(d.info().unwrap().settings.event_filter, 0x0C);
    assert_eq!(d.transact(16, 0x0599, TxMode::query()).unwrap(), FrameResult::Answer(0xFF));
    let recs: Vec<_> = rx.try_iter().filter_map(|e| if let DongleEvent::Record(r) = e { Some(r) } else { None }).collect();
    assert!(recs.iter().any(|r| r.command == "QUERY DEVICE TYPE" && r.target == "A2"), "{recs:#?}");
    d.shutdown();
}

#[test]
fn blocked_events_are_reported() {
    let mut sim = Sim::demo();
    sim.spontaneous = false;
    sim.lock_settings = true;
    sim_set_filter(&mut sim, 0x80);
    let (d, rx) = Dongle::start_with_sim(sim);
    wait_connected(&d);
    std::thread::sleep(Duration::from_millis(100));
    let recs: Vec<_> = rx.try_iter().filter_map(|e| if let DongleEvent::Record(r) = e { Some(r) } else { None }).collect();
    assert!(recs.iter().any(|r| r.command.contains("event reporting switched off")), "{recs:#?}");
    d.shutdown();
}

fn sim_set_filter(sim: &mut Sim, f: u8) {
    sim.set_event_filter(f);
}

#[test]
fn one_byte_add_frame_reply_is_a_frame_id_not_an_error() {
    // FirstID = 1 must not be mistaken for error 1 ("bus voltage error").
    let mut sim = Sim::demo();
    sim.spontaneous = false;
    sim.short_ack = true;
    sim.set_next_id(1);
    let (d, _rx) = Dongle::start_with_sim(sim);
    wait_connected(&d);
    for _ in 0..10 {
        // ids 1..=10 cover every value that overlaps an error code
        assert_eq!(d.transact(16, 0x0599, TxMode::query()).unwrap(), FrameResult::Answer(0xFF));
    }
    let cancel = AtomicBool::new(false);
    let ctx = Ctx { bus: &d, cancel: &cancel, progress: &|_| {} };
    assert_eq!(ops::scan(&ctx, Proto::Gear).unwrap().len(), 3);
    d.shutdown();
}

#[test]
fn one_byte_reply_without_power_is_still_an_error() {
    let mut sim = Sim::demo().without_psu();
    sim.short_ack = true;
    let (d, _rx) = Dongle::start_with_sim(sim);
    wait_connected(&d);
    let e = d.transact(16, 0xFF91, TxMode::query()).unwrap_err();
    assert!(e.to_string().contains("bus voltage"), "{e}");
    d.shutdown();
}
