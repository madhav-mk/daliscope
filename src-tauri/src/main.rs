#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use dali_core::dongle::{self, Dongle, DongleError, DongleEvent, ManagerConfig, PortCandidate};
use dali_core::luba::TxMode;
use dali_core::ops::{self, AddressingMode, Ctx, DeviceParam, GearParam, Progress, Proto};
use dali_core::{BusRecord, ConnState, FrameResult};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

const MONITOR_CAP: usize = 250_000;

struct App {
    dongle: Arc<Dongle>,
    records: Mutex<VecDeque<BusRecord>>,
    /// Serialises multi-frame operations so e.g. addressing isn't interleaved.
    bus: Mutex<()>,
    job_cancel: Mutex<Option<Arc<AtomicBool>>>,
    prefs_path: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Prefs {
    manager: ManagerConfig,
}

fn load_prefs(p: &PathBuf) -> Prefs {
    std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_prefs(p: &PathBuf, prefs: &Prefs) {
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, serde_json::to_string_pretty(prefs).unwrap_or_default());
}

type CmdResult<T> = Result<T, String>;

fn err(e: DongleError) -> String {
    e.to_string()
}

impl App {
    fn try_bus(&self) -> CmdResult<parking_lot::MutexGuard<'_, ()>> {
        self.bus
            .try_lock_for(Duration::from_millis(250))
            .ok_or_else(|| "The bus is busy with another operation (e.g. addressing). Cancel it or wait.".to_string())
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Snapshot {
    state: ConnState,
    config: ManagerConfig,
    ports: Vec<PortCandidate>,
    version: &'static str,
}

#[tauri::command]
fn get_snapshot(app: State<'_, App>) -> Snapshot {
    Snapshot {
        state: app.dongle.state(),
        config: app.dongle.config(),
        ports: dongle::list_ports(),
        version: env!("CARGO_PKG_VERSION"),
    }
}

#[tauri::command]
fn list_ports() -> Vec<PortCandidate> {
    dongle::list_ports()
}

#[tauri::command]
fn set_config(app: State<'_, App>, config: ManagerConfig) {
    app.dongle.set_config(config.clone());
    save_prefs(&app.prefs_path, &Prefs { manager: config });
}

#[tauri::command]
fn rescan(app: State<'_, App>) {
    app.dongle.rescan();
}

#[tauri::command]
fn reconnect(app: State<'_, App>) {
    app.dongle.disconnect("reconnect requested");
}

#[tauri::command]
fn set_bus_power(app: State<'_, App>, on: bool) -> CmdResult<dali_core::luba::Settings> {
    app.dongle.set_bus_power(on).map_err(err)
}

#[tauri::command]
fn identify_interface(app: State<'_, App>) -> CmdResult<()> {
    app.dongle.identify_interface().map_err(err)
}

// ---------------------------------------------------------------------------
// Monitor
// ---------------------------------------------------------------------------

#[tauri::command]
fn monitor_snapshot(app: State<'_, App>, max: Option<usize>) -> Vec<BusRecord> {
    let r = app.records.lock();
    let n = max.unwrap_or(50_000).min(r.len());
    r.iter().skip(r.len() - n).cloned().collect()
}

#[tauri::command]
fn monitor_clear(app: State<'_, App>) {
    app.records.lock().clear();
    app.dongle.reset_monitor_context();
}

#[tauri::command]
fn monitor_note(app: State<'_, App>, text: String) {
    app.dongle.note(&text);
}

fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Write records (optionally only the given seqs, i.e. what the UI filter shows).
#[tauri::command]
fn monitor_export(app: State<'_, App>, path: String, seqs: Option<Vec<u64>>) -> CmdResult<usize> {
    let r = app.records.lock();
    let wanted: Option<std::collections::HashSet<u64>> = seqs.map(|s| s.into_iter().collect());
    let rows: Vec<&BusRecord> = r.iter().filter(|x| wanted.as_ref().is_none_or(|w| w.contains(&x.seq))).collect();
    let as_json = path.to_ascii_lowercase().ends_with(".jsonl");
    let mut out = String::new();
    if as_json {
        for x in &rows {
            out.push_str(&serde_json::to_string(x).map_err(|e| e.to_string())?);
            out.push('\n');
        }
    } else {
        out.push_str("seq,host_time_ms,bus_time_ms,delta_ms,direction,bits,raw,target,command,detail,error\n");
        for x in &rows {
            out.push_str(&format!(
                "{},{:.3},{},{},{:?},{},{},{},{},{},{}\n",
                x.seq,
                x.host_ms,
                x.bus_us.map(|u| format!("{:.3}", u as f64 / 1000.0)).unwrap_or_default(),
                x.delta_ms.map(|d| format!("{d:.3}")).unwrap_or_default(),
                x.dir,
                x.bits,
                csv_escape(&x.raw),
                csv_escape(&x.target),
                csv_escape(&x.command),
                csv_escape(&x.detail),
                x.error
            ));
        }
    }
    std::fs::write(&path, out).map_err(|e| e.to_string())?;
    Ok(rows.len())
}

#[tauri::command]
fn decode_frame(bits: u8, value: u32) -> dali_core::dali::Decoded {
    dali_core::dali::decode(bits, value, None)
}

#[derive(Deserialize)]
struct RawFrame {
    bits: u8,
    value: u32,
    send_twice: bool,
    expect_answer: bool,
    priority: Option<u8>,
}

#[tauri::command]
async fn send_raw(handle: AppHandle, frame: RawFrame) -> CmdResult<FrameResult> {
    let mode = TxMode { send_twice: frame.send_twice, wait_response: frame.expect_answer, priority: frame.priority.unwrap_or(3) };
    blocking(handle, move |d| d.transact(frame.bits, frame.value, mode)).await
}

// ---------------------------------------------------------------------------
// Jobs with progress (scan, addressing, reading)
// ---------------------------------------------------------------------------

fn run_job<T: Send + 'static>(
    handle: &AppHandle,
    name: &'static str,
    f: impl FnOnce(&Ctx) -> dongle::Result<T> + Send + 'static,
) -> impl std::future::Future<Output = CmdResult<T>> {
    let handle = handle.clone();
    async move {
        let app = handle.state::<App>();
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut j = app.job_cancel.lock();
            if j.is_some() {
                return Err("Another operation is already running.".into());
            }
            *j = Some(cancel.clone());
        }
        let d = app.dongle.clone();
        let h2 = handle.clone();
        let res = tauri::async_runtime::spawn_blocking(move || {
            let app = h2.state::<App>();
            let _g = app.bus.lock();
            let last = Mutex::new(Instant::now() - Duration::from_secs(1));
            let progress = |p: Progress| {
                let mut l = last.lock();
                if p.fraction >= 1.0 || l.elapsed() > Duration::from_millis(60) {
                    *l = Instant::now();
                    let _ = h2.emit("job://progress", serde_json::json!({ "job": name, "progress": p }));
                }
            };
            let ctx = Ctx { bus: d.as_ref(), cancel: &cancel, progress: &progress };
            f(&ctx)
        })
        .await
        .map_err(|e| e.to_string());
        *app.job_cancel.lock() = None;
        let _ = handle.emit("job://done", name);
        res?.map_err(err)
    }
}

#[tauri::command]
fn cancel_job(app: State<'_, App>) {
    if let Some(c) = app.job_cancel.lock().as_ref() {
        c.store(true, Ordering::SeqCst);
    }
}

#[derive(Serialize)]
struct ScanResult {
    gear: Vec<ops::ScanEntry>,
    devices: Vec<ops::ScanEntry>,
}

#[tauri::command]
async fn scan(handle: AppHandle) -> CmdResult<ScanResult> {
    run_job(&handle, "scan", |ctx| {
        let gear = ops::scan_range(ctx, Proto::Gear, 0.0, 0.5)?;
        let devices = ops::scan_range(ctx, Proto::Device, 0.5, 0.5)?;
        Ok(ScanResult { gear, devices })
    })
    .await
}

#[tauri::command]
async fn address(handle: AppHandle, gear: bool, devices: bool, mode: AddressingMode) -> CmdResult<Vec<ops::AddressingReport>> {
    run_job(&handle, "address", move |ctx| {
        let mut reps = vec![];
        if gear {
            reps.push(ops::address(ctx, Proto::Gear, mode, &[])?);
        }
        if devices && !reps.last().is_some_and(|r| r.cancelled) {
            reps.push(ops::address(ctx, Proto::Device, mode, &[])?);
        }
        Ok(reps)
    })
    .await
}

#[tauri::command]
async fn read_gear(handle: AppHandle, address: u8) -> CmdResult<ops::GearInfo> {
    run_job(&handle, "read", move |ctx| ops::read_gear(ctx, address, true)).await
}

#[tauri::command]
async fn read_device(handle: AppHandle, address: u8) -> CmdResult<ops::DeviceInfo103> {
    run_job(&handle, "read", move |ctx| ops::read_device(ctx, address, true)).await
}

#[tauri::command]
async fn read_bank(handle: AppHandle, proto: Proto, address: u8, bank: u8) -> CmdResult<Vec<Option<u8>>> {
    run_job(&handle, "bank", move |ctx| ops::read_bank(ctx.bus, proto, address, bank, None)).await
}

// ---------------------------------------------------------------------------
// Single operations
// ---------------------------------------------------------------------------

/// Run a bus operation on a worker thread, holding the bus lock there.
async fn blocking<T: Send + 'static>(
    handle: AppHandle,
    f: impl FnOnce(&Dongle) -> dongle::Result<T> + Send + 'static,
) -> CmdResult<T> {
    tauri::async_runtime::spawn_blocking(move || {
        let app = handle.state::<App>();
        let _g = app.try_bus()?;
        f(&app.dongle).map_err(err)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Gear target: "A5", "G3" or "BC".
fn gear_target(t: &str) -> CmdResult<u8> {
    let t = t.trim().to_ascii_uppercase();
    if t == "BC" {
        return Ok(ops::gear::BROADCAST);
    }
    let (kind, n) = t.split_at(1);
    let n: u8 = n.parse().map_err(|_| format!("bad target {t}"))?;
    match kind {
        "A" if n < 64 => Ok(ops::gear::short(n)),
        "G" if n < 16 => Ok(ops::gear::group(n)),
        _ => Err(format!("bad target {t}")),
    }
}

#[tauri::command]
async fn write_gear(handle: AppHandle, target: String, param: GearParam, value: u8) -> CmdResult<()> {
    let t = gear_target(&target)?;
    blocking(handle, move |d| ops::write_gear(d, t, param, value)).await
}

#[tauri::command]
async fn write_device(handle: AppHandle, address: u8, param: DeviceParam) -> CmdResult<()> {
    blocking(handle, move |d| ops::write_device(d, address, param)).await
}

#[tauri::command]
async fn gear_level(handle: AppHandle, target: String, level: u8) -> CmdResult<()> {
    let t = gear_target(&target)?;
    blocking(handle, move |d| d.transact(16, ops::gear::dapc(t, level), TxMode::plain()).map(|_| ())).await
}

#[tauri::command]
async fn gear_command(handle: AppHandle, target: String, opcode: u8) -> CmdResult<FrameResult> {
    let t = gear_target(&target)?;
    let twice = (0x20..=0x81).contains(&opcode);
    let query = (0x90..=0xC5).contains(&opcode);
    let mode = TxMode { send_twice: twice, wait_response: query, priority: 3 };
    blocking(handle, move |d| d.transact(16, ops::gear::cmd(t, opcode), mode)).await
}

#[tauri::command]
async fn identify(handle: AppHandle, proto: Proto, address: u8) -> CmdResult<()> {
    blocking(handle, move |d| ops::identify(d, proto, address)).await
}

#[tauri::command]
async fn set_short_address(handle: AppHandle, proto: Proto, address: u8, new_address: Option<u8>) -> CmdResult<()> {
    blocking(handle, move |d| ops::set_short_address(d, proto, address, new_address)).await
}

// ---------------------------------------------------------------------------
// Event pump
// ---------------------------------------------------------------------------

fn pump(handle: AppHandle, rx: crossbeam_channel::Receiver<DongleEvent>) {
    let mut batch: Vec<BusRecord> = Vec::with_capacity(512);
    let mut last_flush = Instant::now();
    loop {
        let ev = rx.recv_timeout(Duration::from_millis(40));
        match ev {
            Ok(DongleEvent::Record(r)) => batch.push(r),
            Ok(DongleEvent::State(s)) => {
                // Flush first so the "disconnected" note lands before the state change.
                flush(&handle, &mut batch);
                let _ = handle.emit("dongle://state", s);
            }
            Ok(DongleEvent::Diag(g)) => {
                let _ = handle.emit("dongle://diag", g);
            }
            Ok(DongleEvent::Log { level, message }) => {
                let _ = handle.emit("dongle://log", serde_json::json!({ "level": level, "message": message }));
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(_) => return,
        }
        if !batch.is_empty() && (last_flush.elapsed() >= Duration::from_millis(50) || batch.len() > 400) {
            flush(&handle, &mut batch);
            last_flush = Instant::now();
        }
    }
}

fn flush(handle: &AppHandle, batch: &mut Vec<BusRecord>) {
    if batch.is_empty() {
        return;
    }
    let app = handle.state::<App>();
    {
        let mut r = app.records.lock();
        r.extend(batch.iter().cloned());
        let over = r.len().saturating_sub(MONITOR_CAP);
        if over > 0 {
            r.drain(..over);
        }
    }
    let _ = handle.emit("monitor://batch", &*batch);
    batch.clear();
}

fn main() {
    #[cfg(target_os = "linux")]
    {
        let wsl = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false);
        if wsl && std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            // WSLg has no GPU DMA-BUF path; WebKit's fallback is slower and noisier.
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let prefs_path = app
                .path()
                .app_config_dir()
                .unwrap_or_else(|_| dirs::config_dir().unwrap_or_default().join("daliscope"))
                .join("prefs.json");
            let prefs = load_prefs(&prefs_path);
            let (dongle, rx) = Dongle::start(prefs.manager.clone());
            app.manage(App {
                dongle: Arc::new(dongle),
                records: Mutex::new(VecDeque::new()),
                bus: Mutex::new(()),
                job_cancel: Mutex::new(None),
                prefs_path,
            });
            let h = app.handle().clone();
            std::thread::Builder::new().name("event-pump".into()).spawn(move || pump(h, rx))?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            list_ports,
            set_config,
            rescan,
            reconnect,
            set_bus_power,
            identify_interface,
            monitor_snapshot,
            monitor_clear,
            monitor_note,
            monitor_export,
            send_raw,
            decode_frame,
            cancel_job,
            scan,
            address,
            read_gear,
            read_device,
            read_bank,
            write_gear,
            write_device,
            gear_level,
            gear_command,
            identify,
            set_short_address,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Daliscope")
        .run(|handle, ev| {
            if let tauri::RunEvent::Exit = ev {
                handle.state::<App>().dongle.shutdown();
            }
        });
}
