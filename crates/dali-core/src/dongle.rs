//! Plug-and-play connection manager for LUBA interfaces (DALI-2 USB).
//!
//! A background thread watches the serial ports, probes candidates with
//! QUERY DEVICE DESCRIPTOR, connects automatically, keeps a heartbeat and
//! falls back to searching when the dongle is unplugged. Every connection gets
//! its own I/O thread which decodes packets, matches responses to requests and
//! turns bus events into monitor records.

use crate::event::{self, EventKind, RawEvent};
use crate::luba::{self, cmd, DeviceDescriptor, DeviceInfo, LineStatus, Packet, Settings, TxMode};
use crate::monitor::{BusRecord, Monitor};
use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub const LUNATONE_VID: u16 = 0x17B5;

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Byte transport to an interface. `read` blocks at most a few ms and returns
/// `Ok(0)` when nothing arrived.
pub trait Link: Send {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    fn write_all(&mut self, data: &[u8]) -> io::Result<()>;
}

pub struct SerialLink(Box<dyn serialport::SerialPort>);

impl SerialLink {
    pub fn open(path: &str) -> io::Result<Self> {
        let mut p = serialport::new(path, luba::BAUD)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .flow_control(serialport::FlowControl::None)
            .timeout(Duration::from_millis(10))
            .open()
            .map_err(|e| match e.kind {
                serialport::ErrorKind::Io(k) => io::Error::new(k, e.description),
                serialport::ErrorKind::NoDevice => io::Error::new(io::ErrorKind::NotFound, e.description),
                _ => io::Error::other(e.description),
            })?;
        let _ = p.write_data_terminal_ready(true);
        let _ = p.clear(serialport::ClearBuffer::All);
        Ok(Self(p))
    }
}

impl Link for SerialLink {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buf) {
            Ok(0) => Err(friendly_io(io::Error::new(io::ErrorKind::BrokenPipe, "port closed"))),
            Ok(n) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock => Ok(0),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(0),
            Err(e) => Err(friendly_io(e)),
        }
    }
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        io::Write::write_all(&mut self.0, data)?;
        io::Write::flush(&mut self.0)
    }
}

/// Unplugging a USB CDC device surfaces as EIO / ENXIO / EPIPE depending on
/// the OS; report all of them the same way.
fn friendly_io(e: io::Error) -> io::Error {
    let unplugged = matches!(e.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected | io::ErrorKind::UnexpectedEof)
        || matches!(e.raw_os_error(), Some(5) | Some(6) | Some(19));
    if unplugged {
        io::Error::new(io::ErrorKind::BrokenPipe, "interface unplugged")
    } else {
        e
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error, Clone, Serialize)]
pub enum DongleError {
    #[error("no DALI interface connected")]
    NotConnected,
    #[error("interface did not respond in time")]
    Timeout,
    #[error("interface rejected the request: {0}")]
    Rejected(String),
    #[error("DALI transmission failed: {0}")]
    Bus(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, DongleError>;

#[derive(Debug, Clone, Serialize)]
pub struct DongleInfo {
    pub port: String,
    pub usb_product: Option<String>,
    pub usb_serial: Option<String>,
    pub simulated: bool,
    pub name: String,
    pub descriptor: DeviceDescriptor,
    pub info: DeviceInfo,
    pub settings: Settings,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnState {
    Searching {
        candidates: Vec<String>,
        /// Why a candidate couldn't be used (permissions, port busy, ...).
        hint: Option<String>,
    },
    Probing { port: String },
    Connected { info: DongleInfo },
    Disconnected { reason: String },
    Stopped,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DongleEvent {
    State(ConnState),
    Record(BusRecord),
    /// Periodic line/interface health, about once a second while connected.
    Diag(Diag),
    Log { level: String, message: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct Diag {
    /// None when the interface didn't answer READ STATUS.
    pub line: Option<LineStatus>,
    /// EVENT MESSAGES received since connecting.
    pub events: u64,
    /// Bad packets (checksum errors) since connecting.
    pub bad_packets: u64,
    pub event_filter: u8,
    pub events_blocked: bool,
    pub bus_power: bool,
}

/// Outcome of a single DALI transmission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
pub enum FrameResult {
    Sent,
    Answer(u8),
    NoAnswer,
    Collision,
}

impl FrameResult {
    pub fn value(&self) -> Option<u8> {
        match self {
            FrameResult::Answer(v) => Some(*v),
            _ => None,
        }
    }
    pub fn yes(&self) -> bool {
        matches!(self, FrameResult::Answer(_) | FrameResult::Collision)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PortCandidate {
    pub path: String,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub product: Option<String>,
    pub serial: Option<String>,
    pub lunatone: bool,
}

#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ManagerConfig {
    /// Probe every USB serial port, not just Lunatone VID ones.
    pub probe_all_usb: bool,
    /// An explicit port to try (e.g. a simulator pty).
    pub pinned_port: Option<String>,
    /// Connect to the built-in simulated interface instead of hardware.
    pub simulate: bool,
    /// Pause auto-connect entirely.
    pub paused: bool,
    /// Show every LUBA packet to/from the interface in the monitor.
    pub raw_log: bool,
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

struct FrameTrack {
    waiters: HashMap<u8, Sender<EventKind>>,
    recent: VecDeque<(u8, EventKind, Instant)>,
}

pub struct Conn {
    out: Sender<Vec<u8>>,
    alive: AtomicBool,
    pending: Mutex<HashMap<u8, VecDeque<Sender<Vec<u8>>>>>,
    frames: Mutex<FrameTrack>,
    macro_subs: Mutex<Vec<Sender<EventKind>>>,
    req_lock: Mutex<()>,
    dead_reason: Mutex<Option<String>>,
    pub port: String,
    enumerated: bool,
    last_rx: Mutex<Instant>,
    /// Current event filter; decides whether events carry tick and line fields.
    event_filter: AtomicU8,
    events_rx: AtomicU64,
    bad_packets: AtomicU64,
}

impl Conn {
    fn new(out: Sender<Vec<u8>>, port: String, enumerated: bool) -> Self {
        Self {
            out,
            alive: AtomicBool::new(true),
            pending: Mutex::new(HashMap::new()),
            frames: Mutex::new(FrameTrack { waiters: HashMap::new(), recent: VecDeque::new() }),
            macro_subs: Mutex::new(Vec::new()),
            req_lock: Mutex::new(()),
            dead_reason: Mutex::new(None),
            port,
            enumerated,
            last_rx: Mutex::new(Instant::now()),
            // Until we know better, assume the documented full format.
            event_filter: AtomicU8::new(0),
            events_rx: AtomicU64::new(0),
            bad_packets: AtomicU64::new(0),
        }
    }

    fn kill(&self, reason: impl Into<String>) {
        if self.alive.swap(false, Ordering::SeqCst) {
            *self.dead_reason.lock() = Some(reason.into());
        }
        // Unblock everybody.
        self.pending.lock().clear();
        self.frames.lock().waiters.clear();
        self.macro_subs.lock().clear();
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Send a request and wait for the matching response (`cmd + 1`).
    pub fn request(&self, command: u8, data: Vec<u8>, timeout: Duration) -> Result<Vec<u8>> {
        if !self.is_alive() {
            return Err(DongleError::NotConnected);
        }
        let _g = self.req_lock.lock();
        let (tx, rx) = bounded(1);
        self.pending.lock().entry(cmd::response(command)).or_default().push_back(tx);
        self.out
            .send(Packet::new(command, data).encode())
            .map_err(|_| DongleError::NotConnected)?;
        match rx.recv_timeout(timeout) {
            Ok(d) => Ok(d),
            Err(RecvTimeoutError::Timeout) => {
                if let Some(q) = self.pending.lock().get_mut(&cmd::response(command)) {
                    q.pop_front();
                }
                Err(DongleError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => Err(DongleError::NotConnected),
        }
    }

    fn on_packet(&self, p: Packet) -> Option<RawEvent> {
        *self.last_rx.lock() = Instant::now();
        if p.cmd == cmd::EVENT {
            self.events_rx.fetch_add(1, Ordering::Relaxed);
            let f = self.event_filter.load(Ordering::Relaxed);
            let ev = event::parse(&p.data, f & Settings::F_TICK == 0, f & Settings::F_LINE == 0)?;
            if let Some(id) = ev.kind.frame_id() {
                let mut f = self.frames.lock();
                let now = Instant::now();
                f.recent.push_back((id, ev.kind.clone(), now));
                while f.recent.len() > 256 || f.recent.front().is_some_and(|r| now - r.2 > Duration::from_secs(3)) {
                    f.recent.pop_front();
                }
                if let Some(w) = f.waiters.get(&id) {
                    let _ = w.send(ev.kind.clone());
                }
            }
            if matches!(
                ev.kind,
                EventKind::MacroIntermediate { .. } | EventKind::MacroError { .. } | EventKind::MacroSuccess { .. } | EventKind::MacroStopped { .. }
            ) {
                self.macro_subs.lock().retain(|s| s.send(ev.kind.clone()).is_ok());
            }
            return Some(ev);
        }
        if let Some(q) = self.pending.lock().get_mut(&p.cmd) {
            if let Some(tx) = q.pop_front() {
                let _ = tx.send(p.data);
            }
        }
        None
    }

    /// Queue one DALI frame and wait until it completes on the bus.
    pub fn transact(&self, line: u8, bits: u8, value: u32, mode: TxMode, timeout: Duration) -> Result<FrameResult> {
        let p = luba::add_frame(line, bits, value, mode);
        let asked = Instant::now();
        let resp = self.request(p.cmd, p.data, Duration::from_millis(500))?;
        let id = match resp.as_slice() {
            [id, _, ..] => *id,
            // NrOfFrames is optional, so a one-byte reply is either FirstID or an
            // error code (1-7). Frame IDs overlap that range: it's an ID if the
            // interface goes on to report that frame, and an error if it doesn't.
            [b] if (1..=7).contains(b) => {
                if !self.frame_reported(*b, asked, Duration::from_millis(300)) {
                    return Err(DongleError::Rejected(luba::tx_error_text(*b).into()));
                }
                *b
            }
            [id] => *id,
            [] => return Err(DongleError::Rejected("empty response".into())),
        };
        let (tx, rx) = unbounded();
        let mut backlog = Vec::new();
        {
            let mut f = self.frames.lock();
            let now = Instant::now();
            for (rid, k, t) in f.recent.iter() {
                if *rid == id && now - *t < Duration::from_secs(2) {
                    backlog.push(k.clone());
                }
            }
            f.recent.retain(|r| r.0 != id);
            f.waiters.insert(id, tx);
        }
        let mut deadline = Instant::now() + timeout;
        let mut sent = false;
        let mut sent_count = 0;
        let result = (|| {
            let mut iter = backlog.into_iter();
            loop {
                let k = match iter.next() {
                    Some(k) => k,
                    None => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        match rx.recv_timeout(left) {
                            Ok(k) => k,
                            Err(_) if sent && !mode.wait_response => return Ok(FrameResult::Sent),
                            Err(_) if !self.is_alive() => return Err(DongleError::NotConnected),
                            Err(_) if !sent => {
                                return Err(DongleError::Bus(
                                    "the interface queued the frame but never reported sending it (no bus power, or bus event reporting is off)".into(),
                                ))
                            }
                            Err(_) => return Err(DongleError::Timeout),
                        }
                    }
                };
                match k {
                    EventKind::Sent { .. } => {
                        sent = true;
                        sent_count += 1;
                        if !mode.wait_response {
                            if !mode.send_twice || sent_count >= 2 {
                                return Ok(FrameResult::Sent);
                            }
                            // The interface may report the repeat separately; wait briefly for it.
                            deadline = deadline.min(Instant::now() + Duration::from_millis(120));
                        }
                    }
                    EventKind::Answer { value, .. } => return Ok(FrameResult::Answer(value)),
                    EventKind::AnswerNo { .. } => return Ok(FrameResult::NoAnswer),
                    EventKind::AnswerCollision { .. } => return Ok(FrameResult::Collision),
                    EventKind::SendCollision { .. } => return Err(DongleError::Bus("collision while sending".into())),
                    EventKind::SendBusError { .. } => return Err(DongleError::Bus("bus error while sending".into())),
                    EventKind::SendTimeout { .. } => return Err(DongleError::Bus("send timeout (bus busy?)".into())),
                    _ => {}
                }
            }
        })();
        let mut f = self.frames.lock();
        f.waiters.remove(&id);
        f.recent.retain(|r| r.0 != id);
        // A send-twice config frame that went out once and then went quiet is fine.
        match result {
            Err(DongleError::Timeout) if sent => Ok(FrameResult::Sent),
            r => r,
        }
    }

    /// Did an event for frame `id` arrive after `since` (waiting up to `wait`)?
    fn frame_reported(&self, id: u8, since: Instant, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            if self.frames.lock().recent.iter().any(|(rid, _, t)| *rid == id && *t >= since) {
                return true;
            }
            if Instant::now() >= deadline || !self.is_alive() {
                return false;
            }
            thread::sleep(Duration::from_millis(4));
        }
    }

    pub fn subscribe_macros(&self) -> Receiver<EventKind> {
        let (tx, rx) = unbounded();
        self.macro_subs.lock().push(tx);
        rx
    }
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

pub struct Dongle {
    inner: Arc<Inner>,
}

struct Inner {
    conn: Mutex<Option<Arc<Conn>>>,
    state: Mutex<ConnState>,
    info: Mutex<Option<DongleInfo>>,
    events: Sender<DongleEvent>,
    cfg: Mutex<ManagerConfig>,
    shutdown: AtomicBool,
    monitor: Mutex<Monitor>,
    seq: AtomicU64,
    kick: (Sender<()>, Receiver<()>),
    sim_override: Mutex<Option<crate::sim::Sim>>,
}

impl Dongle {
    /// Start the manager. Events (state changes, monitor records) are delivered
    /// on the returned receiver.
    pub fn start(cfg: ManagerConfig) -> (Self, Receiver<DongleEvent>) {
        let (tx, rx) = unbounded();
        let inner = Arc::new(Inner {
            conn: Mutex::new(None),
            state: Mutex::new(ConnState::Searching { candidates: vec![], hint: None }),
            info: Mutex::new(None),
            events: tx,
            cfg: Mutex::new(cfg),
            shutdown: AtomicBool::new(false),
            monitor: Mutex::new(Monitor::new()),
            seq: AtomicU64::new(0),
            kick: bounded(1),
            sim_override: Mutex::new(None),
        });
        let i2 = inner.clone();
        thread::Builder::new()
            .name("dongle-manager".into())
            .spawn(move || manager_loop(i2))
            .expect("spawn manager");
        (Self { inner }, rx)
    }

    /// Start connected to a specific simulated bus (tests, demos).
    pub fn start_with_sim(sim: crate::sim::Sim) -> (Self, Receiver<DongleEvent>) {
        let (d, rx) = Self::start(ManagerConfig { paused: true, ..Default::default() });
        *d.inner.sim_override.lock() = Some(sim);
        d.set_config(ManagerConfig { simulate: true, ..Default::default() });
        (d, rx)
    }

    pub fn conn(&self) -> Result<Arc<Conn>> {
        self.inner.conn.lock().clone().filter(|c| c.is_alive()).ok_or(DongleError::NotConnected)
    }

    pub fn state(&self) -> ConnState {
        self.inner.state.lock().clone()
    }

    pub fn info(&self) -> Option<DongleInfo> {
        self.inner.info.lock().clone()
    }

    pub fn config(&self) -> ManagerConfig {
        self.inner.cfg.lock().clone()
    }

    pub fn set_config(&self, cfg: ManagerConfig) {
        let changed_target = {
            let old = self.inner.cfg.lock();
            old.simulate != cfg.simulate || old.pinned_port != cfg.pinned_port || old.paused != cfg.paused
        };
        *self.inner.cfg.lock() = cfg;
        if changed_target {
            self.disconnect("configuration changed");
        }
        let _ = self.inner.kick.0.try_send(());
    }

    pub fn disconnect(&self, reason: &str) {
        if let Some(c) = self.inner.conn.lock().as_ref() {
            c.kill(reason);
        }
        let _ = self.inner.kick.0.try_send(());
    }

    /// Re-run port discovery now (e.g. after the user plugs something in).
    pub fn rescan(&self) {
        let _ = self.inner.kick.0.try_send(());
    }

    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.disconnect("shutdown");
    }

    pub fn transact(&self, bits: u8, value: u32, mode: TxMode) -> Result<FrameResult> {
        let timeout = if mode.wait_response { 400 } else { 300 } + if mode.send_twice { 150 } else { 0 };
        self.conn()?.transact(0, bits, value, mode, Duration::from_millis(timeout))
    }

    pub fn request(&self, command: u8, data: Vec<u8>) -> Result<Vec<u8>> {
        self.conn()?.request(command, data, Duration::from_millis(800))
    }

    pub fn write_settings(&self, s: Settings) -> Result<Settings> {
        let _ = self.request(cmd::SETTINGS, s.bytes().to_vec())?;
        // Read back what the interface actually applied rather than trusting the echo.
        let r = self.request(cmd::SETTINGS, vec![])?;
        let s = Settings::parse(&r).ok_or(DongleError::Other("bad settings response".into()))?;
        if let Ok(c) = self.conn() {
            c.event_filter.store(s.event_filter, Ordering::Relaxed);
        }
        if let Some(i) = self.inner.info.lock().as_mut() {
            i.settings = s;
        }
        let info = self.info();
        if let Some(info) = info {
            set_state(&self.inner, ConnState::Connected { info });
        }
        Ok(s)
    }

    pub fn set_bus_power(&self, on: bool) -> Result<Settings> {
        let mut s = self
            .info()
            .map(|i| i.settings)
            .ok_or(DongleError::NotConnected)?;
        s.set_bus_power(on);
        self.write_settings(s)
    }

    pub fn identify_interface(&self) -> Result<()> {
        self.request(cmd::IDENTIFY_SELF, vec![]).map(|_| ())
    }

    pub fn read_status(&self) -> Result<LineStatus> {
        let r = self.request(cmd::READ_STATUS, vec![0])?;
        LineStatus::parse(&r).ok_or(DongleError::Other("bad status".into()))
    }

    pub fn reset_monitor_context(&self) {
        self.inner.monitor.lock().reset();
    }

    /// Inject a note into the monitor stream (e.g. "addressing started").
    pub fn note(&self, text: &str) {
        let rec = self.inner.monitor.lock().note(text);
        emit_record(&self.inner, rec);
    }
}

fn emit_record(inner: &Inner, mut rec: BusRecord) {
    rec.seq = inner.seq.fetch_add(1, Ordering::Relaxed);
    let _ = inner.events.send(DongleEvent::Record(rec));
}

fn log(inner: &Inner, level: &str, message: impl Into<String>) {
    let message = message.into();
    log::info!("{message}");
    let _ = inner.events.send(DongleEvent::Log { level: level.into(), message });
}

fn set_state(inner: &Inner, s: ConnState) {
    *inner.state.lock() = s.clone();
    let _ = inner.events.send(DongleEvent::State(s));
}

pub fn list_ports() -> Vec<PortCandidate> {
    let ports = serialport::available_ports().unwrap_or_default();
    let names: Vec<String> = ports.iter().map(|p| p.port_name.clone()).collect();
    let mut out = Vec::new();
    for p in ports {
        // macOS lists each device twice; the callout (cu.*) device is the one to open.
        if let Some(rest) = p.port_name.strip_prefix("/dev/tty.") {
            if names.iter().any(|n| n == &format!("/dev/cu.{rest}")) {
                continue;
            }
        }
        let (vid, pid, product, serial) = match &p.port_type {
            serialport::SerialPortType::UsbPort(u) => (Some(u.vid), Some(u.pid), u.product.clone(), u.serial_number.clone()),
            _ => (None, None, None, None),
        };
        let lunatone = vid == Some(LUNATONE_VID)
            || product.as_deref().is_some_and(|s| s.to_ascii_uppercase().contains("DALI"));
        out.push(PortCandidate { path: p.port_name, vid, pid, product, serial, lunatone });
    }
    out
}

fn manager_loop(inner: Arc<Inner>) {
    let mut failed: HashMap<String, Instant> = HashMap::new();
    let mut last_heartbeat = Instant::now();
    let mut hb_failures = 0;
    let mut announced_search = false;
    let mut hint: Option<String> = None;
    let mut last_voltage_error: Option<bool> = None;
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            if let Some(c) = inner.conn.lock().take() {
                c.kill("shutdown");
            }
            set_state(&inner, ConnState::Stopped);
            return;
        }
        let current = inner.conn.lock().clone();
        match current {
            Some(c) if c.is_alive() => {
                // Unplug detection via enumeration (some OSes keep the fd "open").
                if c.enumerated && !list_ports().iter().any(|p| p.path == c.port) {
                    c.kill("interface unplugged");
                    continue;
                }
                if last_heartbeat.elapsed() > Duration::from_millis(1000) {
                    last_heartbeat = Instant::now();
                    let quiet = c.last_rx.lock().elapsed() > Duration::from_millis(1500);
                    let line = match c.request(cmd::READ_STATUS, vec![0], Duration::from_millis(600)) {
                        Ok(d) => {
                            hb_failures = 0;
                            LineStatus::parse(&d)
                        }
                        Err(_) => {
                            if quiet {
                                hb_failures += 1;
                                if hb_failures >= 3 {
                                    c.kill("interface stopped responding");
                                }
                            }
                            None
                        }
                    };
                    if let Some(l) = line {
                        if last_voltage_error != Some(l.bus_voltage_error) {
                            if l.bus_voltage_error {
                                let mut rec = inner.monitor.lock().note("No DALI bus voltage. Turn on Bus power or connect a DALI power supply.");
                                rec.error = true;
                                emit_record(&inner, rec);
                            } else if last_voltage_error.is_some() {
                                let rec = inner.monitor.lock().note("DALI bus voltage present");
                                emit_record(&inner, rec);
                            }
                            last_voltage_error = Some(l.bus_voltage_error);
                        }
                    }
                    let settings = inner.info.lock().as_ref().map(|i| i.settings).unwrap_or_default();
                    let _ = inner.events.send(DongleEvent::Diag(Diag {
                        line,
                        events: c.events_rx.load(Ordering::Relaxed),
                        bad_packets: c.bad_packets.load(Ordering::Relaxed),
                        event_filter: settings.event_filter,
                        events_blocked: settings.events_blocked(),
                        bus_power: settings.bus_power(),
                    }));
                }
            }
            Some(c) => {
                let reason = c.dead_reason.lock().clone().unwrap_or_else(|| "connection lost".into());
                *inner.conn.lock() = None;
                *inner.info.lock() = None;
                log(&inner, "warn", format!("Disconnected from {}: {reason}", c.port));
                let rec = inner.monitor.lock().note(&format!("Interface disconnected ({reason})"));
                emit_record(&inner, rec);
                set_state(&inner, ConnState::Disconnected { reason });
                failed.remove(&c.port);
                announced_search = false;
                last_voltage_error = None;
                // Give the OS a moment to settle the device node.
                thread::sleep(Duration::from_millis(300));
            }
            None => {
                let cfg = inner.cfg.lock().clone();
                if cfg.paused {
                    if !matches!(*inner.state.lock(), ConnState::Disconnected { .. }) {
                        set_state(&inner, ConnState::Disconnected { reason: "auto-connect paused".into() });
                    }
                } else if cfg.simulate {
                    try_sim(&inner);
                } else {
                    let ports = list_ports();
                    let mut cands: Vec<(String, bool, Option<PortCandidate>)> = Vec::new();
                    if let Some(p) = cfg.pinned_port.clone().filter(|p| !p.is_empty()) {
                        let pc = ports.iter().find(|c| c.path == p).cloned();
                        cands.push((p, pc.is_some(), pc));
                    }
                    for p in &ports {
                        if cands.iter().any(|c| c.0 == p.path) {
                            continue;
                        }
                        if p.lunatone || (cfg.probe_all_usb && p.vid.is_some()) {
                            cands.push((p.path.clone(), true, Some(p.clone())));
                        }
                    }
                    let names: Vec<String> = cands.iter().map(|c| c.0.clone()).collect();
                    let stale = match &*inner.state.lock() {
                        ConnState::Searching { candidates, hint: h } => candidates != &names || h != &hint,
                        _ => true,
                    };
                    if !announced_search || stale {
                        set_state(&inner, ConnState::Searching { candidates: names.clone(), hint: hint.clone() });
                        announced_search = true;
                    }
                    for (path, enumerated, pc) in cands {
                        let backoff = if pc.as_ref().is_some_and(|p| p.lunatone) { 2 } else { 6 };
                        if failed.get(&path).is_some_and(|t| t.elapsed() < Duration::from_secs(backoff)) {
                            continue;
                        }
                        set_state(&inner, ConnState::Probing { port: path.clone() });
                        match SerialLink::open(&path) {
                            Ok(link) => {
                                let lunatone = pc.as_ref().is_some_and(|p| p.lunatone);
                                if connect(&inner, Box::new(link), path.clone(), enumerated, pc, false) {
                                    failed.clear();
                                    hint = None;
                                    hb_failures = 0;
                                    last_heartbeat = Instant::now();
                                    break;
                                }
                                if lunatone {
                                    hint = Some(format!("{path} opened but did not answer LUBA queries. Replug the interface; if it persists, its firmware may need updating."));
                                }
                                failed.insert(path, Instant::now());
                            }
                            Err(e) => {
                                log::debug!("open {path}: {e}");
                                hint = match e.kind() {
                                    // Simply not plugged in (a pinned port that's gone): nothing to fix.
                                    io::ErrorKind::NotFound => None,
                                    io::ErrorKind::PermissionDenied => Some(if cfg!(target_os = "linux") {
                                        format!("No permission to open {path}. Install the udev rule (see README) or add yourself to the 'dialout' group, then replug.")
                                    } else {
                                        format!("No permission to open {path}.")
                                    }),
                                    _ if e.to_string().to_ascii_lowercase().contains("busy") => {
                                        Some(format!("{path} is in use by another program (DALI Cockpit in a VM, a terminal, ModemManager?)."))
                                    }
                                    _ => Some(format!("Could not open {path}: {e}")),
                                };
                                failed.insert(path, Instant::now());
                            }
                        }
                    }
                    if inner.conn.lock().is_none() && matches!(*inner.state.lock(), ConnState::Probing { .. }) {
                        set_state(&inner, ConnState::Searching { candidates: names, hint: hint.clone() });
                    }
                }
            }
        }
        let _ = inner.kick.1.recv_timeout(Duration::from_millis(400));
    }
}

fn try_sim(inner: &Arc<Inner>) {
    set_state(inner, ConnState::Probing { port: "simulator".into() });
    let sim = inner.sim_override.lock().take().unwrap_or_else(|| {
        // DALISCOPE_SIM_NO_PSU=1 simulates a bus with no power supply.
        let s = crate::sim::Sim::demo();
        if std::env::var_os("DALISCOPE_SIM_NO_PSU").is_some() { s.without_psu() } else { s }
    });
    let link = crate::sim::SimLink::new(sim);
    if !connect(inner, Box::new(link), "simulator".into(), false, None, true) {
        set_state(inner, ConnState::Disconnected { reason: "simulator failed to start".into() });
    }
}

/// Spawn the I/O thread, probe and initialise. Returns true when connected.
fn connect(
    inner: &Arc<Inner>,
    mut link: Box<dyn Link>,
    port: String,
    enumerated: bool,
    pc: Option<PortCandidate>,
    simulated: bool,
) -> bool {
    let (out_tx, out_rx) = unbounded::<Vec<u8>>();
    let conn = Arc::new(Conn::new(out_tx, port.clone(), enumerated));
    let c2 = conn.clone();
    let i2 = inner.clone();
    let stop_probe = Arc::new(AtomicBool::new(false));
    let probing = stop_probe.clone();
    thread::Builder::new()
        .name(format!("dongle-io {port}"))
        .spawn(move || {
            let mut dec = luba::Decoder::new();
            let mut tx_dec = luba::Decoder::new();
            let mut buf = [0u8; 512];
            let mut idle_since: Option<Instant> = None;
            while c2.is_alive() {
                while let Ok(bytes) = out_rx.try_recv() {
                    if let Err(e) = link.write_all(&bytes) {
                        c2.kill(format!("write failed: {e}"));
                        return;
                    }
                    if i2.cfg.lock().raw_log {
                        for p in tx_dec.push(&bytes) {
                            if p.cmd != cmd::READ_STATUS {
                                let rec = i2.monitor.lock().raw(true, p.cmd, &p.data);
                                emit_record(&i2, rec);
                            }
                        }
                    }
                }
                match link.read(&mut buf) {
                    Ok(0) => {
                        if dec.pending() > 0 {
                            let since = *idle_since.get_or_insert_with(Instant::now);
                            if since.elapsed() > Duration::from_millis(60) {
                                for p in dec.on_idle() {
                                    handle_packet(&i2, &c2, p, &probing);
                                }
                                idle_since = None;
                            }
                        }
                    }
                    Ok(n) => {
                        idle_since = None;
                        for p in dec.push(&buf[..n]) {
                            handle_packet(&i2, &c2, p, &probing);
                        }
                        c2.bad_packets.store(dec.stats.checksum_errors, Ordering::Relaxed);
                    }
                    Err(e) => {
                        c2.kill(format!("{e}"));
                        return;
                    }
                }
            }
        })
        .expect("spawn io");

    // Probe: QUERY DEVICE DESCRIPTOR, twice.
    let mut desc = None;
    for _ in 0..2 {
        if let Ok(d) = conn.request(cmd::QUERY_DEVICE_DESCRIPTOR, vec![], Duration::from_millis(500)) {
            desc = DeviceDescriptor::parse(&d);
            if desc.is_some() {
                break;
            }
        }
    }
    let Some(descriptor) = desc else {
        conn.kill("not a LUBA interface");
        return false;
    };
    let t = Duration::from_millis(600);
    let mut info = DeviceInfo::default();
    if let Ok(d) = conn.request(cmd::QUERY_DEVICE_INFO, vec![0], t) {
        info.apply_set0(&d);
    }
    if let Ok(d) = conn.request(cmd::QUERY_DEVICE_INFO, vec![1], t) {
        info.apply_set1(&d);
    }
    let name = conn
        .request(cmd::DEVICE_NAME, vec![], t)
        .map(|d| luba::c_string(&d))
        .unwrap_or_default();
    let mut settings = conn
        .request(cmd::SETTINGS, vec![], t)
        .ok()
        .and_then(|d| Settings::parse(&d))
        .unwrap_or_default();
    conn.event_filter.store(settings.event_filter, Ordering::Relaxed);
    // We need every event, with tick and line, to drive the monitor.
    if settings.event_filter != 0 {
        let mut want = settings;
        want.event_filter = 0;
        let _ = conn.request(cmd::SETTINGS, want.bytes().to_vec(), t);
        // Read back rather than trusting the write's echo.
        if let Some(s) = conn.request(cmd::SETTINGS, vec![], t).ok().and_then(|d| Settings::parse(&d)) {
            settings = s;
        }
        conn.event_filter.store(settings.event_filter, Ordering::Relaxed);
    }
    let events_note = settings.events_blocked().then(|| {
        format!(
            "The interface kept bus event reporting switched off (event filter 0x{:02X}), so bus traffic can't be shown. Replug it; if this persists, its firmware may need updating.",
            settings.event_filter
        )
    });
    {
        let mut m = inner.monitor.lock();
        m.reset();
        m.set_tick_us(descriptor.tick_us.max(1));
    }
    stop_probe.store(true, Ordering::SeqCst);

    let info = DongleInfo {
        port: port.clone(),
        usb_product: pc.as_ref().and_then(|p| p.product.clone()),
        usb_serial: pc.as_ref().and_then(|p| p.serial.clone()),
        simulated,
        name,
        descriptor,
        info,
        settings,
    };
    *inner.info.lock() = Some(info.clone());
    *inner.conn.lock() = Some(conn);
    log(inner, "info", format!("Connected to {} on {port}", display_name(&info)));
    let rec = inner.monitor.lock().note(&format!("Interface connected: {} ({port})", display_name(&info)));
    emit_record(inner, rec);
    if let Some(n) = events_note {
        log(inner, "warn", n.clone());
        let mut rec = inner.monitor.lock().note(&n);
        rec.error = true;
        emit_record(inner, rec);
    }
    set_state(inner, ConnState::Connected { info });
    true
}

pub fn display_name(i: &DongleInfo) -> String {
    if !i.info.article_text.is_empty() {
        i.info.article_text.clone()
    } else if let Some(p) = &i.usb_product {
        p.clone()
    } else if !i.name.is_empty() {
        i.name.clone()
    } else {
        "LUBA interface".into()
    }
}

fn handle_packet(inner: &Inner, conn: &Conn, p: Packet, probing_done: &AtomicBool) {
    if probing_done.load(Ordering::Relaxed) && p.cmd != cmd::response(cmd::READ_STATUS) && inner.cfg.lock().raw_log {
        let rec = inner.monitor.lock().raw(false, p.cmd, &p.data);
        emit_record(inner, rec);
    }
    if let Some(ev) = conn.on_packet(p) {
        // Don't feed the monitor until the connection is fully set up.
        if !probing_done.load(Ordering::Relaxed) {
            return;
        }
        let recs = inner.monitor.lock().on_event(&ev);
        for r in recs {
            emit_record(inner, r);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn partial_config_uses_defaults() {
        let c: super::ManagerConfig = serde_json::from_str(r#"{"simulate":true}"#).unwrap();
        assert!(c.simulate && !c.paused && !c.probe_all_usb && c.pinned_port.is_none());
    }
}
