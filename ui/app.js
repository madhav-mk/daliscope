'use strict';

/* ============================================================
   Daliscope UI
   ============================================================ */

const TAURI = window.__TAURI__;
const invoke = TAURI.core.invoke;
const listen = TAURI.event.listen;
const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));
const esc = (s) => String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const hex2 = (n) => (n ?? 0).toString(16).toUpperCase().padStart(2, '0');
const ROW_H = 24;

const PREF_KEY = 'daliscope.ui.v1';
const prefs = Object.assign(
  {
    time: 'wall',
    theme: 'system',
    cap: 100000,
    filters: { tx: true, rx: true, gear: true, device: true, event: true, special: true, answer: true, status: true, buffer: false, luba: true },
    history: [],
  },
  (() => { try { return JSON.parse(localStorage.getItem(PREF_KEY) || '{}'); } catch { return {}; } })()
);
// Filters added in later versions default to on for existing users.
prefs.filters = Object.assign({ luba: true }, prefs.filters);
const savePrefs = () => { try { localStorage.setItem(PREF_KEY, JSON.stringify(prefs)); } catch { /* storage unavailable */ } };

const S = {
  conn: { state: 'searching', candidates: [] },
  wasConnected: false,
  info: null,
  config: {},
  ports: [],
  recs: [],
  view: [],
  lastSeq: -1,
  frameCount: 0,
  paused: false,
  pausedBuf: [],
  selSeq: null,
  follow: true,
  errors: 0,
  events: 0,
  busBad: null,
  renderQueued: false,
  dev: { gear: [], devices: [], cache: {}, sel: null, scanned: false },
  diag: null,
  job: null,
};

/* ------------------------------------------------------------
   Utilities
   ------------------------------------------------------------ */

function toast(title, body = '', kind = '') {
  const t = document.createElement('div');
  t.className = 'toast ' + kind;
  t.innerHTML = `<b>${esc(title)}</b>${body ? esc(body) : ''}`;
  $('#toasts').appendChild(t);
  setTimeout(() => { t.style.transition = 'opacity .3s'; t.style.opacity = '0'; setTimeout(() => t.remove(), 320); }, kind === 'err' ? 6000 : 3200);
}

function levelPct(l) {
  if (l === 0) return 0;
  if (l === 255 || l == null) return NaN;
  return Math.pow(10, (l - 1) / (253 / 3) - 1);
}
function levelText(l) {
  if (l == null) return '—';
  if (l === 255) return 'MASK';
  if (l === 0) return 'off';
  const p = levelPct(l);
  return (p < 1 ? p.toFixed(2) : p.toFixed(1)) + '%';
}
const fadeTimeText = (n) => (n == null ? '—' : n === 0 ? 'none / ext.' : (0.5 * Math.pow(2, n / 2)).toFixed(1) + ' s');
const fadeRateText = (n) => (n == null || n === 0 ? '—' : (506 / Math.pow(2, n / 2)).toFixed(1) + ' st/s');
const DT_NAMES = { 0: 'fluorescent', 1: 'emergency', 2: 'HID', 3: 'LV halogen', 4: 'incandescent', 5: '0-10V', 6: 'LED', 7: 'switching', 8: 'colour', 9: 'sequencer', 15: 'load ref.', 16: 'thermal prot.', 17: 'dimming curve', 20: 'demand resp.', 21: 'thermal lamp', 23: 'non-repl.', 49: 'bus PSU', 50: 'MB1 ext.', 51: 'energy', 52: 'diagnostics' };
const IT_NAMES = { 0: 'generic', 1: 'push button (301)', 2: 'absolute input (302)', 3: 'occupancy (303)', 4: 'light sensor (304)', 5: 'colour sensor (305)', 6: 'general purpose sensor (306)' };
const SCHEMES = ['instance', 'device', 'device/instance', 'device group', 'instance group'];
const versionText = (v) => (v == null ? '—' : `${v >> 2}.${v & 3}`);
const shortPort = (p) => String(p || '').replace(/^\/dev\//, '');

function err(e) {
  const m = typeof e === 'string' ? e : (e && e.message) || JSON.stringify(e);
  if (/bus voltage error/.test(m)) return 'There is no voltage on the DALI line. Check the wiring between the interface and the bus, and that a bus power supply is on.';
  return m;
}

async function call(cmd, args, quiet) {
  try { return await invoke(cmd, args); }
  catch (e) { if (!quiet) toast('Failed', err(e), 'err'); throw e; }
}

function promptDialog({ title, text = '', value = '', input = true, ok = 'OK', danger = false }) {
  const d = $('#dlg-prompt');
  $('#pr-title').textContent = title;
  $('#pr-text').textContent = text;
  $('#pr-input').classList.toggle('hidden', !input);
  $('#pr-input').value = value;
  $('#pr-ok').textContent = ok;
  $('#pr-ok').classList.toggle('danger', danger);
  d.returnValue = '';
  d.showModal();
  if (input) setTimeout(() => $('#pr-input').select(), 30);
  return new Promise((res) => {
    d.addEventListener('close', () => res(d.returnValue === 'ok' ? (input ? $('#pr-input').value : true) : null), { once: true });
  });
}

/* ------------------------------------------------------------
   Tabs
   ------------------------------------------------------------ */

function showView(name) {
  $$('.tab').forEach((t) => t.classList.toggle('active', t.dataset.view === name));
  $$('.view').forEach((v) => v.classList.toggle('active', v.id === 'view-' + name));
  if (name === 'monitor') scheduleRender();
  if (name === 'settings') refreshPorts();
  if (name === 'devices') renderTree();
}
$$('.tab').forEach((t) => t.addEventListener('click', () => showView(t.dataset.view)));

/* ------------------------------------------------------------
   Connection state (plug & play)
   ------------------------------------------------------------ */

function setConn(s) {
  const prev = S.conn.state;
  S.conn = s;
  const pill = $('#conn-pill');
  pill.className = 'pill ' + s.state;
  let text = '';
  switch (s.state) {
    case 'searching': {
      const n = (s.candidates || []).length;
      text = s.hint ? 'Interface found · needs attention' : n ? `Searching · ${n} candidate port${n > 1 ? 's' : ''}` : 'Waiting for DALI-2 USB…';
      $('#conn-pill').title = s.hint || '';
      break;
    }
    case 'probing': text = `Probing ${shortPort(s.port)}…`; break;
    case 'connected': {
      const i = s.info;
      text = `${ifaceName(i)} · ${i.simulated ? 'simulated' : shortPort(i.port)}`;
      break;
    }
    case 'disconnected': text = `Disconnected · ${s.reason}`; break;
    default: text = 'Stopped';
  }
  $('#conn-text').textContent = text;

  if (s.state === 'connected') {
    S.info = s.info;
    const hasPower = !!(s.info.descriptor && s.info.descriptor.has_bus_power) || /DALI-2 USB/i.test(ifaceName(s.info));
    $('#power-wrap').classList.toggle('hidden', !hasPower);
    $('#bus-power').checked = !!(s.info.settings.hardware & 0x80);
    if (!S.wasConnected) {
      pill.classList.remove('flash'); void pill.offsetWidth;
      pill.classList.add('flash');
      // A reconnect may be a different installation: forget cached device data.
      S.dev = { gear: [], devices: [], cache: {}, sel: null, scanned: false };
      if ($('#view-devices').classList.contains('active')) renderWelcome();
      toast('Interface connected', `${ifaceName(s.info)} on ${s.info.simulated ? 'the simulator' : s.info.port}`);
      S.busBad = null;
      setBusChip();
    }
    S.wasConnected = true;
  } else {
    if (S.wasConnected && (s.state === 'disconnected' || s.state === 'searching')) {
      toast('Interface disconnected', s.reason ? `${s.reason}. It will reconnect automatically.` : 'It will reconnect automatically.', 'warn');
      S.busBad = null;
      setBusChip();
    }
    S.wasConnected = false;
    S.diag = null;
    S.info = null;
    $('#power-wrap').classList.add('hidden');
  }
  if (prev !== s.state) renderPopover();
  updateEmpty();
  renderTreeHint();
}

function ifaceName(i) {
  if (!i) return 'Interface';
  return (i.info && i.info.article_text) || i.usb_product || i.name || 'LUBA interface';
}

function setBusChip() {
  const c = $('#bus-chip');
  if (S.conn.state !== 'connected') { c.className = 'chip muted'; c.textContent = 'Bus —'; return; }
  const line = S.diag && S.diag.line;
  if (line && line.bus_voltage_error) { c.className = 'chip bad'; c.textContent = 'No bus voltage'; c.title = 'The interface measures no voltage on the DALI line.'; return; }
  c.title = '';
  if (S.busBad === true) { c.className = 'chip bad'; c.textContent = 'Bus fault'; }
  else if (S.busBad === false) { c.className = 'chip ok'; c.textContent = 'Bus OK'; }
  else { c.className = 'chip muted'; c.textContent = 'Bus idle'; }
}

function renderPopover() {
  const b = $('#conn-pop-body');
  const s = S.conn;
  if (s.state === 'connected') {
    const i = s.info, d = i.descriptor, inf = i.info;
    const rows = [
      ['Interface', ifaceName(i)],
      ['Name', i.name || '—'],
      ['Port', i.simulated ? 'built-in simulator' : i.port],
      ['Article no.', inf.article_number || '—'],
      ['GTIN', inf.gtin || '—'],
      ['Serial', inf.serial || '—'],
      ['Hardware', `PCB ${inf.pcb_version} · assembly ${inf.assembly_version}`],
      ['Produced', inf.production_year ? `${inf.production_year} week ${inf.production_week}` : '—'],
      ['Protocol', `LUBAP ${d.protocol_major}.${d.protocol_minor}`],
      ['DALI lines', d.lines],
      ['Send buffer', `${d.buffer_size} frames`],
      ['Tick', `${d.tick_us} µs`],
      ['Bus power', d.has_bus_power ? (i.settings.hardware & 0x80 ? 'integrated, on' : 'integrated, off') : 'not reported'],
      ['Bus voltage', S.diag && S.diag.line ? (S.diag.line.bus_voltage_error ? 'NONE' : 'present') : '—'],
      ['Settings', `mode 0x${hex2(i.settings.mode)} · filter 0x${hex2(i.settings.event_filter)} · hw 0x${hex2(i.settings.hardware)}`],
      ['Events received', S.diag ? S.diag.events : '—'],
      ['USB', [i.usb_product, i.usb_serial].filter(Boolean).join(' · ') || '—'],
    ];
    b.innerHTML = `<dl>${rows.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join('')}</dl>`;
  } else {
    const cands = s.candidates || [];
    b.innerHTML = `<dl><dt>State</dt><dd>${esc(s.state)}${s.reason ? ' · ' + esc(s.reason) : ''}</dd>
      <dt>Candidates</dt><dd>${cands.length ? cands.map(esc).join('<br>') : 'none yet'}</dd>
      ${s.hint ? `<dt>Problem</dt><dd>${esc(s.hint)}</dd>` : ''}</dl>
      <p class="muted" style="margin:0 0 10px">Plug the DALI-2 USB into this computer. It's a standard USB serial device (no driver). Ports with Lunatone's vendor ID are probed automatically.</p>`;
  }
  $('#btn-identify-if').disabled = s.state !== 'connected';
}

$('#conn-pill').addEventListener('click', (e) => {
  e.stopPropagation();
  renderPopover();
  $('#conn-pop').classList.toggle('hidden');
});
document.addEventListener('click', (e) => {
  if (!$('#conn-pop').contains(e.target)) $('#conn-pop').classList.add('hidden');
});
$('#btn-identify-if').addEventListener('click', () => call('identify_interface').then(() => toast('Identify', 'The interface LED is flashing for 10 s.')));
$('#btn-reconnect').addEventListener('click', () => call('reconnect'));
$('#btn-rescan').addEventListener('click', () => call('rescan'));
$('#bus-power').addEventListener('change', async (e) => {
  const on = e.target.checked;
  try {
    const s = await invoke('set_bus_power', { on });
    e.target.checked = !!(s.hardware & 0x80);
    if (S.info) S.info.settings = s;
    if (on && !e.target.checked) { toast('Bus power', 'The interface did not switch its supply on.', 'warn'); return; }
    toast('Bus power ' + (e.target.checked ? 'on' : 'off'), e.target.checked ? 'Integrated 30 mA supply enabled. Keep total supply under 250 mA.' : '');
  } catch (x) {
    e.target.checked = !on;
    toast('Bus power', err(x), 'err');
  }
});

/* ------------------------------------------------------------
   Monitor: filters
   ------------------------------------------------------------ */

const CHIPS = [
  { k: 'tx', label: 'Sent', c: 'var(--accent)' },
  { k: 'rx', label: 'Received', c: 'var(--blue)' },
  { k: 'gear', label: 'Gear', c: 'var(--blue)' },
  { k: 'device', label: 'Devices', c: 'var(--violet)' },
  { k: 'event', label: 'Events', c: 'var(--amber)' },
  { k: 'special', label: 'Special', c: 'var(--accent)' },
  { k: 'answer', label: 'Answers', c: 'var(--green)' },
  { k: 'status', label: 'Status', c: 'var(--amber)' },
  { k: 'buffer', label: 'Buffer', c: 'var(--faint)' },
  { k: 'luba', label: 'Interface', c: 'var(--faint)' },
];

function famKey(r) {
  switch (r.family) {
    case 'gear': return 'gear';
    case 'device': return 'device';
    case 'event': return 'event';
    case 'gear_special': case 'device_special': case 'other': return 'special';
    case 'backward': return 'answer';
    case 'buffer': return 'buffer';
    case 'note': return 'note';
    case 'luba': return 'luba';
    default: return 'status';
  }
}

let searchFn = null;
function compileSearch(q) {
  q = q.trim().toLowerCase();
  if (!q) return null;
  if (/^(a|g|d|dg|i|ig|it)\d{1,2}$/.test(q)) {
    return (r) => r.target.toLowerCase().split(/[\s·]+/).includes(q);
  }
  const hx = q.replace(/^0x/, '').replace(/\s+/g, '');
  const isHex = /^0x/.test(q) || (/^[0-9a-f]{2,7}$/.test(hx) && hx.length % 2 === 0);
  return (r) =>
    (isHex && r.raw.replace(/\s/g, '').toLowerCase().includes(hx)) ||
    r.command.toLowerCase().includes(q) || r.detail.toLowerCase().includes(q) || r.target.toLowerCase().includes(q);
}

function pass(r) {
  const f = prefs.filters;
  const fk = famKey(r);
  if (fk === 'note') return !searchFn;
  if (r.dir === 'tx' && !f.tx) return false;
  if (r.dir === 'ans' && !f.tx) return false;
  if (r.dir === 'rx' && !f.rx) return false;
  if (!f[fk]) return false;
  if ($('#f-errors').checked && !r.error) return false;
  if (searchFn && !searchFn(r)) return false;
  return true;
}

function rebuildView() {
  S.view = [];
  for (let i = 0; i < S.recs.length; i++) if (pass(S.recs[i])) S.view.push(i);
  scheduleRender(true);
}

function renderChips() {
  const box = $('#filter-chips');
  box.innerHTML = CHIPS.filter((c) => c.k !== 'luba' || S.config.raw_log).map((c) => `<button class="fchip ${prefs.filters[c.k] ? 'on' : ''}" data-k="${c.k}" style="--c:${c.c}"><i></i>${esc(c.label)}</button>`).join('');
  $$('.fchip', box).forEach((b) => b.addEventListener('click', () => {
    prefs.filters[b.dataset.k] = !prefs.filters[b.dataset.k];
    savePrefs();
    renderChips();
    rebuildView();
  }));
}

let searchTimer;
$('#f-search').addEventListener('input', (e) => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => { searchFn = compileSearch(e.target.value); rebuildView(); }, 120);
});
$('#f-errors').addEventListener('change', rebuildView);

/* ------------------------------------------------------------
   Monitor: ingest & render
   ------------------------------------------------------------ */

function ingest(recs) {
  for (const r of recs) {
    // The boot snapshot and the live stream can overlap; seq numbers are monotonic.
    if (r.seq <= S.lastSeq) continue;
    S.lastSeq = r.seq;
    S.recs.push(r);
    if (r.bits) S.frameCount++;
    if (r.error) S.errors++;
    if (r.family === 'event') S.events++;
    if (r.family === 'status') {
      if (/BUS ERROR|SYSTEM FAILURE|SUPPLY WARNING/.test(r.command)) S.busBad = true;
      else if (r.command === 'BUS RESTORED') S.busBad = false;
    } else if (r.bits && S.busBad !== true) {
      S.busBad = false;
    }
    if (pass(r)) S.view.push(S.recs.length - 1);
  }
  const cap = Number(prefs.cap) || 100000;
  if (S.recs.length > cap) {
    S.recs = S.recs.slice(S.recs.length - Math.floor(cap * 0.8));
    S.frameCount = S.recs.reduce((n, r) => n + (r.bits ? 1 : 0), 0);
    rebuildView();
  }
  setBusChip();
  updateEmpty();
  scheduleRender();
}

function onBatch(recs) {
  if (S.paused) {
    S.pausedBuf.push(...recs);
    const cap = Number(prefs.cap) || 100000;
    if (S.pausedBuf.length > cap) S.pausedBuf.splice(0, S.pausedBuf.length - cap);
    $('#st-paused-n').textContent = S.pausedBuf.length;
    return;
  }
  ingest(recs);
}

function scheduleRender(force) {
  if (S.renderQueued && !force) return;
  S.renderQueued = true;
  requestAnimationFrame(() => { S.renderQueued = false; render(); });
}

const fmtTime = (r) => {
  if (prefs.time === 'bus') return r.bus_us != null ? (r.bus_us / 1e6).toFixed(4) : '—';
  const d = new Date(r.host_ms);
  return d.toTimeString().slice(0, 8) + '.' + String(d.getMilliseconds()).padStart(3, '0');
};
const DIRS = { tx: 'TX', rx: 'RX', ans: 'ANS', sys: 'SYS', note: '' };

function rowHtml(r) {
  const cls = ['r', 'f-' + r.family];
  if (r.error) cls.push('err');
  if (r.seq === S.selSeq) cls.push('sel');
  if (r.dir === 'note') {
    cls.push('note');
    return `<div class="row ${cls.join(' ')}" data-seq="${r.seq}"><div class="c-seq">${r.seq}</div><div class="c-time">${fmtTime(r)}</div><div class="c-dt"></div><div class="c-cmd">${esc(r.command)}</div></div>`;
  }
  const dt = r.delta_ms != null ? r.delta_ms.toFixed(1) : '';
  return `<div class="row ${cls.join(' ')}" data-seq="${r.seq}">` +
    `<div class="c-seq">${r.seq}</div><div class="c-time">${fmtTime(r)}</div><div class="c-dt">${dt}</div>` +
    `<div class="c-dir"><span class="dir ${r.dir}">${DIRS[r.dir]}</span></div>` +
    `<div class="c-raw">${esc(r.raw)}</div><div class="c-tgt">${esc(r.target)}</div>` +
    `<div class="c-cmd" title="${esc(r.command)}">${esc(r.command)}</div><div class="c-det" title="${esc(r.detail)}">${esc(r.detail)}</div></div>`;
}

const scroller = $('#mon-scroll');
let programmaticScroll = false;

function render() {
  if (!$('#view-monitor').classList.contains('active')) return;
  const total = S.view.length;
  $('#mon-spacer').style.height = total * ROW_H + 'px';
  if (S.follow) {
    const target = Math.max(0, total * ROW_H - scroller.clientHeight);
    if (Math.abs(scroller.scrollTop - target) > 1) {
      programmaticScroll = true;
      scroller.scrollTop = target;
    }
  }
  const h = scroller.clientHeight;
  const top = scroller.scrollTop;
  const first = Math.max(0, Math.floor(top / ROW_H) - 8);
  const last = Math.min(total, Math.ceil((top + h) / ROW_H) + 8);
  let html = '';
  for (let i = first; i < last; i++) html += rowHtml(S.recs[S.view[i]]);
  const rows = $('#mon-rows');
  rows.style.transform = `translateY(${first * ROW_H}px)`;
  rows.innerHTML = html;
  $('#st-rows').textContent = total.toLocaleString() + (total !== S.recs.length ? ` / ${S.recs.length.toLocaleString()}` : '');
}

scroller.addEventListener('scroll', () => {
  if (programmaticScroll) { programmaticScroll = false; return; }
  const atBottom = scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - ROW_H * 2;
  if (S.follow !== atBottom) {
    S.follow = atBottom;
    $('#f-follow').checked = atBottom;
  }
  render();
});
new ResizeObserver(() => scheduleRender()).observe(scroller);
$('#f-follow').addEventListener('change', (e) => { S.follow = e.target.checked; scheduleRender(); });

function updateEmpty() {
  const empty = $('#mon-empty');
  const connected = S.conn.state === 'connected';
  const show = S.frameCount === 0;
  empty.classList.toggle('hidden', !show);
  if (!show) return;
  $('.plug').classList.toggle('hidden', connected);
  $('#btn-use-sim').classList.toggle('hidden', connected || !!S.config.simulate);
  $('#empty-diag').classList.toggle('hidden', !connected);
  if (connected) {
    const g = S.diag;
    const noV = g && g.line && g.line.bus_voltage_error;
    $('#empty-title').textContent = noV ? 'Connected · but the DALI line has no voltage' : 'Connected · listening to the bus';
    $('#empty-sub').textContent = noV
      ? 'Nothing can be sent or received until the bus is powered.'
      : 'A DALI bus stays silent until something transmits. Drivers only speak when asked, so send a test query, scan the bus, or trigger a sensor.';
    $('#empty-ports').innerHTML = '';
    renderDiag();
  } else {
    $('#empty-title').textContent = S.conn.state === 'disconnected' && S.config.paused ? 'Auto-connect is paused' : 'Plug in your DALI-2 USB';
    $('#empty-sub').textContent = "It's detected automatically. No drivers or setup needed. Traffic appears here the moment it connects.";
    const cands = S.conn.candidates || [];
    const probing = S.conn.state === 'probing' ? S.conn.port : null;
    if (S.conn.state === 'searching') S.lastHint = S.conn.hint || null;
    $('#empty-ports').innerHTML = (probing
      ? `<span class="probing">● probing ${esc(probing)}</span>`
      : cands.map((c) => `<span>○ ${esc(c)}</span>`).join('')) +
      (S.lastHint ? `<span class="hint">${esc(S.lastHint)}</span>` : '');
  }
}
function renderDiag() {
  const box = $('#empty-diag');
  if (box.classList.contains('hidden')) return;
  const g = S.diag;
  const i = S.info;
  const ok = (b, y, n) => `<dd class="${b ? 'good' : 'bad'}">${b ? y : n}</dd>`;
  if (!g) { box.innerHTML = '<span class="note">Checking the line…</span>'; return; }
  const line = g.line;
  const powerCapable = !$('#power-wrap').classList.contains('hidden');
  box.innerHTML = `
    <dt>Bus voltage</dt>${line ? ok(!line.bus_voltage_error, 'present', 'none') : '<dd>unknown</dd>'}
    <dt>Interface bus supply</dt><dd>${g.bus_power ? 'on' : 'off'}</dd>
    <dt>Event reporting</dt>${ok(!g.events_blocked, 'on', `off (filter 0x${hex2(g.event_filter)})`)}
    <dt>Events received</dt><dd>${g.events}</dd>
    ${g.bad_packets ? `<dt>Corrupt packets</dt><dd class="bad">${g.bad_packets}</dd>` : ''}
    ${line && line.bus_voltage_error ? `<span class="note">${g.bus_power
      ? "The interface's own supply is on but it still sees no voltage. The interface probably isn't wired to the same DALI pair as the luminaire, or the bus is shorted. Check the DA/DA terminals and the luminaire's DALI connection (on streetlights, often the NEMA/Zhaga socket)."
      : 'Turn on the interface\'s bus supply, or power the bus from the luminaire (many D4i drivers supply it) or a separate DALI PSU.'}</span>` : ''}
    <div class="actions">
      ${line && line.bus_voltage_error && !g.bus_power && powerCapable ? '<button class="btn primary small" id="d-power">Turn on bus power</button>' : ''}
      <button class="btn small" id="d-test" ${line && line.bus_voltage_error ? 'disabled' : ''}>Send test query</button>
      <button class="btn small" id="d-raw">${S.config.raw_log ? 'Hide' : 'Show'} interface packets</button>
    </div>`;
  const p = $('#d-power');
  if (p) p.onclick = () => { $('#bus-power').checked = true; $('#bus-power').dispatchEvent(new Event('change')); };
  $('#d-test').onclick = testQuery;
  $('#d-raw').onclick = () => saveConfig({ ...S.config, raw_log: !S.config.raw_log });
  void i;
}

async function testQuery() {
  try {
    const r = await invoke('send_raw', { frame: { bits: 16, value: 0xFF91, send_twice: false, expect_answer: true } });
    if (r.result === 'answer' || r.result === 'collision') toast('Control gear answered', r.result === 'collision' ? 'Several drivers replied to QUERY CONTROL GEAR PRESENT.' : 'One driver replied to QUERY CONTROL GEAR PRESENT.');
    else toast('No answer', 'The query went out but no control gear replied.', 'warn');
  } catch (e) { toast('Test query failed', err(e), 'err'); }
}

$('#btn-use-sim').addEventListener('click', () => saveConfig({ ...S.config, simulate: true, paused: false }));

/* stats */
setInterval(() => {
  const now = Date.now();
  let frames = 0, busy = 0;
  for (let i = S.recs.length - 1; i >= 0; i--) {
    const r = S.recs[i];
    if (r.host_ms < now - 5000) break;
    if (r.bits) { frames++; busy += (r.bits + 3) * 0.8333; }
  }
  $('#st-fps').textContent = (frames / 5).toFixed(1);
  const load = Math.min(100, (busy / 5000) * 100);
  $('#st-load').textContent = load.toFixed(load < 10 ? 1 : 0) + '%';
  $('#st-load-bar').style.width = load + '%';
  $('#st-err').textContent = S.errors;
  $('#st-ev').textContent = S.events;
}, 1000);

/* pause / clear / marker */
function setPaused(p) {
  S.paused = p;
  $('#btn-pause .ico').textContent = p ? '▶' : '❚❚';
  $('#btn-pause .lbl').textContent = p ? 'Resume' : 'Pause';
  $('#st-paused').classList.toggle('hidden', !p);
  if (!p && S.pausedBuf.length) { const b = S.pausedBuf; S.pausedBuf = []; ingest(b); }
  $('#st-paused-n').textContent = '0';
}
$('#btn-pause').addEventListener('click', () => setPaused(!S.paused));
async function clearMonitor() {
  await call('monitor_clear');
  S.recs = []; S.view = []; S.pausedBuf = []; S.errors = 0; S.events = 0; S.selSeq = null; S.frameCount = 0;
  $('#inspector').classList.add('hidden');
  updateEmpty();
  scheduleRender(true);
}
$('#btn-clear').addEventListener('click', clearMonitor);
let markerN = 0;
$('#btn-mark').addEventListener('click', () => call('monitor_note', { text: `Marker ${++markerN} · ${new Date().toLocaleTimeString()}` }));

/* export */
$('#btn-export').addEventListener('click', async () => {
  const stamp = new Date().toISOString().slice(0, 19).replace(/[:T]/g, '-');
  let path;
  try {
    path = await invoke('plugin:dialog|save', {
      options: { defaultPath: `dali-capture-${stamp}.csv`, filters: [{ name: 'CSV', extensions: ['csv'] }, { name: 'JSON Lines', extensions: ['jsonl'] }] },
    });
  } catch (e) { toast('Export', err(e), 'err'); return; }
  if (!path) return;
  const filtered = S.view.length !== S.recs.length;
  const seqs = filtered ? S.view.map((i) => S.recs[i].seq) : null;
  const n = await call('monitor_export', { path, seqs });
  toast('Exported', `${n.toLocaleString()} rows to ${path}`);
});

/* ------------------------------------------------------------
   Inspector
   ------------------------------------------------------------ */

function findRec(seq) {
  for (let i = S.recs.length - 1; i >= 0; i--) if (S.recs[i].seq === seq) return S.recs[i];
  return null;
}

function bitCells(v, n) {
  let s = '';
  for (let i = n - 1; i >= 0; i--) { const b = (v >> i) & 1; s += `<span class="${b ? 'one' : ''}">${b}</span>`; }
  return `<div class="cells">${s}</div>`;
}

function showInspector(seq) {
  S.selSeq = seq;
  const r = findRec(seq);
  if (!r) return;
  const kv = [
    ['Seq', r.seq], ['Wall time', fmtTimeFull(r.host_ms)], ['Bus time', r.bus_us != null ? (r.bus_us / 1e6).toFixed(6) + ' s' : '—'],
    ['Δ previous', r.delta_ms != null ? r.delta_ms.toFixed(2) + ' ms' : '—'], ['Direction', { tx: 'sent by this app', rx: 'from the bus', ans: 'answer to our query', sys: 'interface / line status', note: 'note' }[r.dir]],
    ['Bits', r.bits || '—'], ['Frame', r.raw || '—'], ['Target', r.target || '—'], ['Command', r.command], ['Detail', r.detail || '—'],
  ];
  if (r.frame_id != null) kv.push(['Frame ID', r.frame_id]);
  let bits = '';
  const v = parseInt((r.raw || '').replace(/\s/g, ''), 16);
  if (r.bits && !isNaN(v)) {
    const groups = r.bits === 16 ? [['Address', 15, 8], ['Opcode / level', 7, 0]]
      : r.bits === 24 ? (r.family === 'event' ? [['Source', 23, 10], ['Event info', 9, 0]] : [['Address', 23, 16], ['Instance', 15, 8], ['Opcode', 7, 0]])
      : [['Data', r.bits - 1, 0]];
    bits = `<div class="bits">${groups.map(([l, hi, lo]) => `<div class="bitgroup"><label>${l}</label>${bitCells((v >> lo) & ((1 << (hi - lo + 1)) - 1), hi - lo + 1)}</div>`).join('')}</div>`;
  }
  $('#insp-body').innerHTML = `<dl class="kv">${kv.map(([k, x]) => `<dt>${esc(k)}</dt><dd>${esc(x)}</dd>`).join('')}</dl>${bits}
    <div class="insp-actions">
      ${r.bits && r.bits !== 8 && r.dir !== 'ans' ? '<button class="btn small" id="i-load">Load into send bar</button>' : ''}
      ${r.target && /^(A|G|D)\d/.test(r.target) ? `<button class="btn small" id="i-filter">Filter ${esc(r.target.split(' ')[0])}</button>` : ''}
      <button class="btn small" id="i-copy">Copy</button>
    </div>`;
  $('#inspector').classList.remove('hidden');
  const load = $('#i-load');
  if (load) load.onclick = () => { $('#tx-bits').value = String(r.bits); $('#tx-hex').value = r.raw; $('#tx-answer').checked = /^QUERY|COMPARE|VERIFY|READ MEMORY/.test(r.command); txPreview(); $('#tx-hex').focus(); };
  const f = $('#i-filter');
  if (f) f.onclick = () => { $('#f-search').value = r.target.split(' ')[0]; searchFn = compileSearch($('#f-search').value); rebuildView(); };
  $('#i-copy').onclick = () => navigator.clipboard.writeText(`${fmtTimeFull(r.host_ms)}\t${r.dir.toUpperCase()}\t${r.raw}\t${r.target}\t${r.command}\t${r.detail}`).then(() => toast('Copied'));
  scheduleRender(true);
}
function fmtTimeFull(ms) { const d = new Date(ms); return d.toLocaleDateString() + ' ' + d.toTimeString().slice(0, 8) + '.' + String(d.getMilliseconds()).padStart(3, '0'); }

$('#mon-rows').addEventListener('click', (e) => {
  const row = e.target.closest('.r');
  if (row) showInspector(Number(row.dataset.seq));
});
$('#insp-close').addEventListener('click', () => { $('#inspector').classList.add('hidden'); S.selSeq = null; scheduleRender(true); });

scroller.addEventListener('keydown', (e) => {
  if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
  e.preventDefault();
  let idx = S.view.findIndex((i) => S.recs[i].seq === S.selSeq);
  idx = idx < 0 ? S.view.length - 1 : Math.max(0, Math.min(S.view.length - 1, idx + (e.key === 'ArrowDown' ? 1 : -1)));
  if (idx < 0) return;
  S.follow = false; $('#f-follow').checked = false;
  showInspector(S.recs[S.view[idx]].seq);
  const y = idx * ROW_H;
  if (y < scroller.scrollTop) scroller.scrollTop = y;
  else if (y + ROW_H > scroller.scrollTop + scroller.clientHeight) scroller.scrollTop = y + ROW_H - scroller.clientHeight;
});

/* ------------------------------------------------------------
   Send bar
   ------------------------------------------------------------ */

const TEMPLATES = [
  ['16', 'FF 00', 'BC · OFF'],
  ['16', 'FF 05', 'BC · RECALL MAX LEVEL'],
  ['16', 'FE FE', 'BC · DAPC 254'],
  ['16', 'FF A0', 'BC · QUERY ACTUAL LEVEL', 1],
  ['16', 'FF 91', 'BC · QUERY CONTROL GEAR PRESENT', 1],
  ['16', 'FF 25', 'BC · IDENTIFY DEVICE', 0, 1],
  ['16', '01 99', 'A0 · QUERY DEVICE TYPE', 1],
  ['16', '01 97', 'A0 · QUERY VERSION NUMBER', 1],
  ['16', 'A1 00', 'TERMINATE'],
  ['24', 'FF FE 30', 'BC · QUERY DEVICE STATUS (103)', 1],
  ['24', 'FF FE 00', 'BC · IDENTIFY DEVICE (103)', 0, 1],
  ['24', '01 FE 35', 'D0 · QUERY NUMBER OF INSTANCES', 1],
  ['24', '01 00 80', 'D0 · I0 · QUERY INSTANCE TYPE', 1],
  ['24', '01 00 8C', 'D0 · I0 · QUERY INPUT VALUE', 1],
  ['24', 'FF FE 1D', 'BC · START QUIESCENT MODE', 0, 1],
  ['24', 'FF FE 1E', 'BC · STOP QUIESCENT MODE', 0, 1],
];
$('#tx-template').innerHTML += TEMPLATES.map((t, i) => `<option value="${i}">${esc(t[2])} · ${t[1]}</option>`).join('');
$('#tx-template').addEventListener('change', (e) => {
  const t = TEMPLATES[Number(e.target.value)];
  e.target.value = '';
  if (!t) return;
  $('#tx-bits').value = t[0];
  $('#tx-hex').value = t[1];
  $('#tx-answer').checked = !!t[3];
  $('#tx-twice').checked = !!t[4];
  txPreview(true);
  $('#tx-hex').focus();
});

function parseTx() {
  const bits = Number($('#tx-bits').value);
  const hx = $('#tx-hex').value.replace(/0x/gi, '').replace(/[\s_:,-]/g, '');
  if (!hx || !/^[0-9a-f]+$/i.test(hx) || hx.length > Math.ceil(bits / 4)) return null;
  const value = parseInt(hx, 16);
  if (bits < 32 && value >= Math.pow(2, bits)) return null;
  return { bits, value };
}

let previewTimer;
function txPreview(keepFlags) {
  clearTimeout(previewTimer);
  previewTimer = setTimeout(async () => {
    const p = parseTx();
    $('#tx-hex').classList.toggle('bad', !p && $('#tx-hex').value.trim() !== '');
    if (!p) { $('#tx-preview').textContent = ''; return; }
    try {
      const d = await invoke('decode_frame', p);
      $('#tx-preview').innerHTML = `<b>${esc(d.target)}</b> · ${esc(d.command)}${d.detail ? ' · ' + esc(d.detail) : ''}`;
      if (!keepFlags) {
        $('#tx-twice').checked = !!d.config;
        $('#tx-answer').checked = !!d.query;
      }
    } catch { $('#tx-preview').textContent = ''; }
  }, 80);
}
$('#tx-hex').addEventListener('input', () => txPreview());
$('#tx-bits').addEventListener('change', () => txPreview());

let histIdx = -1;
async function sendTx() {
  const p = parseTx();
  if (!p) { toast('Invalid frame', `Enter up to ${Math.ceil(Number($('#tx-bits').value) / 4)} hex digits.`, 'warn'); return; }
  if (S.conn.state !== 'connected') { toast('Not connected', 'Plug in the interface first.', 'warn'); return; }
  const entry = `${p.bits}:${$('#tx-hex').value.trim()}`;
  prefs.history = [entry, ...prefs.history.filter((h) => h !== entry)].slice(0, 40);
  savePrefs();
  histIdx = -1;
  try {
    await invoke('send_raw', { frame: { bits: p.bits, value: p.value, send_twice: $('#tx-twice').checked, expect_answer: $('#tx-answer').checked } });
  } catch (e) { toast('Send failed', err(e), 'err'); }
}
$('#btn-send').addEventListener('click', sendTx);
$('#tx-hex').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') { e.preventDefault(); sendTx(); }
  else if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
    e.preventDefault();
    if (!prefs.history.length) return;
    histIdx = Math.max(-1, Math.min(prefs.history.length - 1, histIdx + (e.key === 'ArrowUp' ? 1 : -1)));
    if (histIdx < 0) { $('#tx-hex').value = ''; txPreview(); return; }
    const [b, h] = prefs.history[histIdx].split(':');
    $('#tx-bits').value = b; $('#tx-hex').value = h; txPreview();
  }
});

/* keyboard shortcuts */
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') {
    if (closeDropdown()) return;
    if (!$('#conn-pop').classList.contains('hidden')) { $('#conn-pop').classList.add('hidden'); return; }
    if (!$('#inspector').classList.contains('hidden')) { $('#insp-close').click(); return; }
  }
  const inField = /INPUT|SELECT|TEXTAREA|BUTTON/.test(document.activeElement.tagName) || document.activeElement.isContentEditable;
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') { e.preventDefault(); clearMonitor(); return; }
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'f') { e.preventDefault(); showView('monitor'); $('#f-search').focus(); return; }
  if (inField || !$('#view-monitor').classList.contains('active')) return;
  if (e.key === ' ') { e.preventDefault(); setPaused(!S.paused); }
  else if (e.key.toLowerCase() === 'm') $('#btn-mark').click();
  else if (e.key === 'End') { S.follow = true; $('#f-follow').checked = true; scheduleRender(); }
});

/* ------------------------------------------------------------
   Devices
   ------------------------------------------------------------ */

const keyOf = (proto, a) => (proto === 'gear' ? 'g' : 'd') + a;

function renderTreeHint() {
  if ($('#view-devices').classList.contains('active')) renderTree();
}

function gearSummary(a, e) {
  const g = S.dev.cache[keyOf('gear', a)];
  const types = g ? g.device_types : (e && e.types) || [];
  const dtsOnly = types.map((t) => `DT${t} ${DT_NAMES[t] || ''}`.trim()).join(' + ');
  if (!g) return dtsOnly || 'control gear';
  const dts = dtsOnly;
  const groups = g.groups != null ? [...Array(16).keys()].filter((i) => g.groups & (1 << i)).map((i) => 'G' + i).join(' ') : '';
  return [dts, groups].filter(Boolean).join(' · ');
}
function devSummary(a, e) {
  const d = S.dev.cache[keyOf('device', a)];
  const list = d ? d.instances.map((i) => i.instance_type) : (e && e.types) || [];
  const n = d ? d.instance_count ?? 0 : list.length;
  const types = list.map((t) => (IT_NAMES[t] || 'type ' + t).replace(/ \(\d+\)/, '')).join(', ');
  if (!d && !list.length) return 'control device';
  return `${n} instance${n === 1 ? '' : 's'}${types ? ' · ' + types : ''}`;
}

function renderTree() {
  const t = $('#dev-tree');
  const connected = S.conn.state === 'connected';
  $('#btn-scan').disabled = !connected || !!S.job;
  $('#btn-address').disabled = !connected || !!S.job;
  if (!S.dev.scanned) {
    t.innerHTML = `<div class="dev-empty">${connected ? 'Scan the bus to list addressed control gear and control devices.' : 'Connect the interface to scan the bus.'}</div>`;
    if (!S.dev.sel) renderWelcome();
    return;
  }
  const item = (proto, e) => {
    const k = keyOf(proto, e.address);
    const sel = S.dev.sel && keyOf(S.dev.sel.proto, S.dev.sel.addr) === k;
    return `<div class="dev-item ${sel ? 'sel' : ''}" data-proto="${proto}" data-addr="${e.address}">
      <span class="addr">${proto === 'gear' ? 'A' : 'D'}${e.address}</span>
      <span class="what">${esc(proto === 'gear' ? gearSummary(e.address, e) : devSummary(e.address, e))}</span>
      ${e.duplicate ? '<span class="dup" title="Several devices answered: duplicate short address">DUP</span>' : '<span></span>'}</div>`;
  };
  t.innerHTML =
    `<h4><span>Control gear</span><span>${S.dev.gear.length}</span></h4>` +
    (S.dev.gear.length ? S.dev.gear.map((e) => item('gear', e)).join('') : '<div class="dev-empty">None found.</div>') +
    `<h4><span>Control devices</span><span>${S.dev.devices.length}</span></h4>` +
    (S.dev.devices.length ? S.dev.devices.map((e) => item('device', e)).join('') : '<div class="dev-empty">None found.</div>');
  $$('.dev-item', t).forEach((el) => el.addEventListener('click', () => selectDevice(el.dataset.proto, Number(el.dataset.addr))));
}

function renderWelcome() {
  $('#dev-detail').innerHTML = `<div class="welcome">
    <h2>Commissioning</h2>
    <ol>
      <li><b>Scan bus</b> lists every addressed driver (A0–A63) and sensor/input (D0–D63).</li>
      <li><b>Address…</b> gives unaddressed devices short addresses using the random-address search. <i>System extension</i> keeps existing addresses.</li>
      <li>Select a device to read and edit its parameters, groups, scenes, instances and timers. Every change is verified by reading it back.</li>
    </ol>
    <p>All traffic shows up in the bus monitor, so you can see exactly what was sent.</p>
  </div>`;
}

function jobStart(text) {
  S.job = { text };
  $('#job').classList.remove('hidden');
  $('#job-text').textContent = text;
  $('#job-bar').style.width = '0%';
  renderTree();
}
function jobEnd() {
  S.job = null;
  $('#job').classList.add('hidden');
  renderTree();
}
$('#btn-cancel-job').addEventListener('click', () => invoke('cancel_job'));

async function doScan(quiet) {
  if (S.job) return;
  jobStart('Scanning…');
  try {
    const r = await invoke('scan');
    S.dev.gear = r.gear;
    S.dev.devices = r.devices;
    S.dev.scanned = true;
    if (!quiet) toast('Scan complete', `${r.gear.length} control gear · ${r.devices.length} control device${r.devices.length === 1 ? '' : 's'}`);
    const dups = [...r.gear, ...r.devices].filter((e) => e.duplicate);
    if (dups.length) toast('Duplicate addresses', `${dups.map((d) => (d.proto === 'gear' ? 'A' : 'D') + d.address).join(', ')}: several devices answered. Run addressing (new installation) to fix.`, 'warn');
  } catch (e) {
    if (!/cancel/i.test(err(e))) toast('Scan failed', err(e), 'err');
  } finally { jobEnd(); }
}
$('#btn-scan').addEventListener('click', () => doScan());

$('#btn-address').addEventListener('click', () => {
  const d = $('#dlg-address');
  d.returnValue = '';
  d.showModal();
  d.addEventListener('close', async () => {
    if (d.returnValue !== 'ok') return;
    const gear = $('#ad-gear').checked, devices = $('#ad-dev').checked;
    if (!gear && !devices) return;
    const mode = $('input[name=ad-mode]:checked').value;
    if (mode === 'new_installation') {
      const ok = await promptDialog({ title: 'Delete all addresses?', text: 'Every short address on this bus is deleted and reassigned. Groups and scenes stay in the devices, but anything that refers to the old addresses will need updating.', input: false, ok: 'Delete and readdress', danger: true });
      if (!ok) return;
    }
    jobStart('Addressing…');
    S.dev.sel = null;
    S.dev.cache = {};
    $('#dev-detail').innerHTML = `<div class="card"><h3>Addressing log</h3><div id="addr-log" class="hexgrid"></div></div>`;
    try {
      const reps = await invoke('address', { gear, devices, mode });
      const n = reps.reduce((a, r) => a + r.assigned.length, 0);
      const log = $('#addr-log');
      if (log) log.insertAdjacentHTML('beforeend', `<div style="margin-top:6px;color:var(--accent)">Done · ${n} new address${n === 1 ? '' : 'es'}${reps.some((r) => r.cancelled) ? ' (cancelled)' : ''}</div>`);
      const fails = reps.flatMap((r) => r.failures);
      const cancelled = reps.some((r) => r.cancelled);
      toast(cancelled ? 'Addressing cancelled' : 'Addressing complete', `${n} new address${n === 1 ? '' : 'es'} assigned${fails.length ? ` · ${fails.length} failure(s)` : ''}`, fails.length ? 'warn' : '');
      if (reps.some((r) => r.out_of_addresses)) toast('Out of addresses', 'All 64 short addresses are in use on this bus.', 'warn');
    } catch (e) {
      toast('Addressing failed', err(e), 'err');
    } finally {
      jobEnd();
      doScan(true);
    }
  }, { once: true });
});

async function selectDevice(proto, addr, force) {
  if (S.job) return;
  S.dev.sel = { proto, addr };
  renderTree();
  const k = keyOf(proto, addr);
  if (S.dev.cache[k] && !force) { renderDetail(); return; }
  $('#dev-detail').innerHTML = `<div class="muted">Reading ${proto === 'gear' ? 'A' : 'D'}${addr}…</div>`;
  jobStart(`Reading ${proto === 'gear' ? 'A' : 'D'}${addr}…`);
  try {
    S.dev.cache[k] = await invoke(proto === 'gear' ? 'read_gear' : 'read_device', { address: addr });
  } catch (e) {
    $('#dev-detail').innerHTML = `<div class="card"><h3>Could not read ${proto === 'gear' ? 'A' : 'D'}${addr}</h3><p class="muted">${esc(err(e))}</p></div>`;
    jobEnd();
    return;
  }
  jobEnd();
  renderDetail();
}

function renderDetail() {
  const s = S.dev.sel;
  if (!s) return renderWelcome();
  const d = S.dev.cache[keyOf(s.proto, s.addr)];
  if (!d) return;
  if (s.proto === 'gear') renderGear(d); else renderDev(d);
}

/* gear-side helpers */
async function gq(a, op) {
  const r = await invoke('gear_command', { target: 'A' + a, opcode: op });
  return r.result === 'answer' ? r.value : null;
}
async function devq(a, inst, op) {
  const value = (((a << 1) | 1) << 16) | (inst << 8) | op;
  const r = await invoke('send_raw', { frame: { bits: 24, value, send_twice: false, expect_answer: true } });
  return r.result === 'answer' ? r.value : r.result === 'collision' ? 255 : null;
}

function flashOk(el, ok) {
  if (el._dd) el = el._dd.btn;
  el.classList.remove('ok-flash', 'err-flash');
  void el.offsetWidth;
  el.classList.add(ok ? 'ok-flash' : 'err-flash');
  if (ok) setTimeout(() => el.classList.remove('ok-flash'), 1000);
}

function headHtml(label, tags, proto) {
  return `<div class="dd-head"><div class="big">${label}</div>${tags.map((t) => `<span class="tag ${t[1] ? 'acc' : ''}">${esc(t[0])}</span>`).join('')}
    <div class="spacer"></div>
    <button class="btn small" data-act="identify">Identify</button>
    <button class="btn small" data-act="refresh">Read again</button>
    <button class="btn small" data-act="readdr">Change address…</button>
    <button class="btn small danger" data-act="delete">Delete address</button></div>`;
}

function bindHeadActions(proto, a) {
  const root = $('#dev-detail');
  root.querySelector('[data-act=identify]').onclick = () => call('identify', { proto, address: a }).then(() => toast('Identify sent', `${proto === 'gear' ? 'A' : 'D'}${a} should flash or blink now.`));
  root.querySelector('[data-act=refresh]').onclick = () => selectDevice(proto, a, true);
  root.querySelector('[data-act=readdr]').onclick = async () => {
    const v = await promptDialog({ title: `New short address for ${proto === 'gear' ? 'A' : 'D'}${a}`, text: 'Enter 0–63. The address must be free.', value: String(a) });
    if (v == null) return;
    const n = Number(v);
    if (!Number.isInteger(n) || n < 0 || n > 63) { toast('Invalid address', 'Use 0–63.', 'warn'); return; }
    await call('set_short_address', { proto, address: a, newAddress: n });
    delete S.dev.cache[keyOf(proto, a)];
    toast('Address changed', `${proto === 'gear' ? 'A' : 'D'}${a} → ${proto === 'gear' ? 'A' : 'D'}${n}`);
    await doScan(true);
    selectDevice(proto, n, true);
  };
  root.querySelector('[data-act=delete]').onclick = async () => {
    const ok = await promptDialog({ title: `Delete ${proto === 'gear' ? 'A' : 'D'}${a}?`, text: 'The device keeps working but becomes unaddressed until you run addressing again.', input: false, ok: 'Delete address', danger: true });
    if (!ok) return;
    await call('set_short_address', { proto, address: a, newAddress: null });
    delete S.dev.cache[keyOf(proto, a)];
    S.dev.sel = null;
    toast('Address deleted');
    await doScan(true);
    renderWelcome();
  };
}

function bankCard(proto, a) {
  return `<section class="card"><h3>Memory bank <span class="inline"><input type="number" id="mb-n" min="0" max="255" value="0" style="width:64px"> <button class="btn small" id="mb-read">Read</button></span></h3>
    <div id="mb-out" class="hexgrid muted">Read any bank to see its raw contents.</div></section>`;
}
function bindBank(proto, a) {
  $('#mb-read').onclick = async () => {
    const bank = Number($('#mb-n').value) || 0;
    $('#mb-out').textContent = 'Reading…';
    try {
      const data = await invoke('read_bank', { proto, address: a, bank });
      if (!data.length) { $('#mb-out').textContent = `Bank ${bank} is not implemented (no answer).`; return; }
      let html = '';
      for (let i = 0; i < data.length; i += 8) {
        const cells = data.slice(i, i + 8).map((x) => (x == null ? '<span class="na">··</span>' : hex2(x))).join(' ');
        html += `<div><span class="off">${hex2(i)}:</span> ${cells}</div>`;
      }
      $('#mb-out').classList.remove('muted');
      $('#mb-out').innerHTML = html;
    } catch (e) { $('#mb-out').textContent = err(e); }
  };
}

function identHtml(b0, extra) {
  const rows = [...extra];
  if (b0) {
    rows.push(
      ['GTIN', b0.gtin ?? '—'],
      ['Identification no.', b0.identification ?? '—'],
      ['Firmware', b0.firmware ? b0.firmware.join('.') : '—'],
      ['Hardware', b0.hardware ? b0.hardware.join('.') : '—'],
      ['Part 101 / 102 / 103', [b0.version_101, b0.version_102, b0.version_103].map((v) => (v == null || v === 255 ? '—' : versionText(v))).join(' / ')],
      ['Logical units', `${b0.gear_units ?? '—'} gear · ${b0.device_units ?? '—'} device · index ${b0.unit_index ?? '—'}`],
      ['Last memory bank', b0.last_bank]
    );
  } else rows.push(['Memory bank 0', 'not readable']);
  return `<dl class="ident">${rows.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join('')}</dl>`;
}

/* ---------- gear ---------- */

function renderGear(g) {
  const a = g.address;
  const tags = g.device_types.map((t) => [`DT${t} ${DT_NAMES[t] || ''}`.trim(), t === 6 || t === 8]);
  tags.push([`102 v${versionText(g.version)}`, false]);
  const fadeOpts = (sel, from) => [...Array(16).keys()].slice(from).map((n) => `<option value="${n}" ${n === sel ? 'selected' : ''}>${n}</option>`).join('');
  const P = [
    ['max_level', 'Max level', g.max_level, 0xA1, 'level'],
    ['min_level', 'Min level', g.min_level, 0xA2, 'level', `phys. min ${g.physical_min ?? '—'}`],
    ['power_on_level', 'Power-on level', g.power_on_level, 0xA3, 'level'],
    ['system_failure_level', 'System failure level', g.system_failure_level, 0xA4, 'level'],
    ['fade_time', 'Fade time', g.fade_time, 0xA5, 'fadetime'],
    ['fade_rate', 'Fade rate', g.fade_rate, 0xA5, 'faderate'],
    ['extended_fade_time', 'Extended fade time', g.extended_fade, 0xA8, 'raw'],
    ['operating_mode', 'Operating mode', g.operating_mode, 0x9E, 'raw'],
  ];
  const conv = (kind, v) => (kind === 'level' ? levelText(v) : kind === 'fadetime' ? fadeTimeText(v) : kind === 'faderate' ? fadeRateText(v) : v == null ? '—' : '0x' + hex2(v));
  const ctl = ([k, , v, , kind]) => {
    if (kind === 'fadetime') return `<select data-p="${k}">${fadeOpts(v, 0)}</select>`;
    if (kind === 'faderate') return `<select data-p="${k}">${fadeOpts(v, 1)}</select>`;
    return `<input type="number" min="0" max="255" data-p="${k}" value="${v ?? ''}">`;
  };
  const isDt6 = g.device_types.includes(6);
  $('#dev-detail').innerHTML = headHtml('A' + a, tags, 'gear') + `
  <div class="grid2">
    <section class="card"><h3>Light level <span class="muted" id="lv-actual">actual ${esc(levelText(g.actual_level))}</span></h3>
      <div class="live">
        <input type="range" id="lv" min="0" max="254" value="${g.actual_level ?? 0}">
        <span class="lv" id="lv-text">${g.actual_level ?? 0} · ${esc(levelText(g.actual_level ?? 0))}</span>
      </div>
      <div class="pop-actions" style="margin-top:10px">
        <button class="btn small" data-op="0">Off</button><button class="btn small" data-op="6">Min</button>
        <button class="btn small" data-op="5">Max</button><button class="btn small" data-op="10">Last active</button>
      </div>
      ${g.dt8_colour_temperature ? `<p class="muted" style="margin:10px 0 0">DT8 colour temperature: ${g.dt8_colour_temperature} mirek ≈ ${Math.round(1e6 / g.dt8_colour_temperature)} K</p>` : ''}
    </section>
    <section class="card"><h3>Parameters</h3><div class="form">
      ${P.map((p) => `<label>${p[1]}${p[5] ? `<small>${esc(p[5])}</small>` : ''}</label><div class="ctl"><span class="conv" data-c="${p[0]}">${esc(conv(p[4], p[2]))}</span>${ctl(p)}</div>`).join('')}
      ${isDt6 ? `<label>Dimming curve (DT6)</label><div class="ctl"><select data-p="dt6_dimming_curve"><option value="0" ${g.dt6_dimming_curve === 0 ? 'selected' : ''}>standard (log)</option><option value="1" ${g.dt6_dimming_curve === 1 ? 'selected' : ''}>linear</option></select></div>` : ''}
    </div></section>
    <section class="card"><h3>Groups</h3><div class="toggles">
      ${[...Array(16).keys()].map((i) => `<button class="tg ${g.groups != null && g.groups & (1 << i) ? 'on' : ''}" data-g="${i}">G${i}</button>`).join('')}
    </div></section>
    <section class="card"><h3>Scenes <span class="muted" style="font-weight:400">empty = not in scene</span></h3><div class="scenes">
      ${g.scenes.map((v, i) => `<label>S${i}<input data-s="${i}" value="${v ?? ''}" placeholder="—"></label>`).join('')}
    </div></section>
    <section class="card"><h3>Identity</h3>${identHtml(g.bank0, [
      ['Random address', g.random_address != null ? '0x' + g.random_address.toString(16).toUpperCase().padStart(6, '0') : '—'],
      ['Light source type', g.light_source_type ?? '—'],
      ['Status', g.status != null ? '0x' + hex2(g.status) : '—'],
      ...(g.dt6_gear_type != null ? [['DT6 gear type', '0x' + hex2(g.dt6_gear_type)]] : []),
      ...(g.dt8_colour_type_features != null ? [['DT8 colour features', '0x' + hex2(g.dt8_colour_type_features)]] : []),
    ])}</section>
    ${bankCard('gear', a)}
  </div>`;
  bindHeadActions('gear', a);
  bindBank('gear', a);

  // live level
  let lvTimer;
  $('#lv').addEventListener('input', (e) => {
    const v = Number(e.target.value);
    $('#lv-text').textContent = `${v} · ${levelText(v)}`;
    clearTimeout(lvTimer);
    lvTimer = setTimeout(() => invoke('gear_level', { target: 'A' + a, level: v }).catch((x) => toast('Level', err(x), 'err')), 90);
  });
  $$('[data-op]').forEach((b) => b.addEventListener('click', async () => {
    await call('gear_command', { target: 'A' + a, opcode: Number(b.dataset.op) });
    const v = await gq(a, 0xA0);
    if (v != null) { $('#lv').value = v; $('#lv-text').textContent = `${v} · ${levelText(v)}`; $('#lv-actual').textContent = 'actual ' + levelText(v); }
  }));

  // parameters
  $$('[data-p]').forEach((el) => el.addEventListener('change', async () => {
    const key = el.dataset.p;
    const val = Number(el.value);
    if (!Number.isInteger(val) || val < 0 || val > 255) { flashOk(el, false); return; }
    el.classList.add('saving');
    try {
      await invoke('write_gear', { target: 'A' + a, param: { param: key }, value: val });
      const spec = P.find((p) => p[0] === key);
      let actual = val;
      if (spec) {
        const r = await gq(a, spec[3]);
        actual = key === 'fade_time' ? (r ?? 0) >> 4 : key === 'fade_rate' ? (r ?? 0) & 15 : r;
        const cache = { max_level: 'max_level', min_level: 'min_level', power_on_level: 'power_on_level', system_failure_level: 'system_failure_level', fade_time: 'fade_time', fade_rate: 'fade_rate', extended_fade_time: 'extended_fade', operating_mode: 'operating_mode' }[key];
        g[cache] = actual;
        const c = $(`[data-c=${key}]`);
        if (c) c.textContent = conv(spec[4], actual);
      } else if (key === 'dt6_dimming_curve') g.dt6_dimming_curve = val;
      el.value = actual ?? '';
      flashOk(el, actual === val);
      if (actual !== val) toast('Value adjusted by device', `${key.replace(/_/g, ' ')}: requested ${val}, device reports ${actual ?? 'no answer'}`, 'warn');
    } catch (x) { flashOk(el, false); toast('Write failed', err(x), 'err'); }
    el.classList.remove('saving');
  }));

  // groups
  $$('[data-g]').forEach((b) => b.addEventListener('click', async () => {
    const i = Number(b.dataset.g);
    const want = !b.classList.contains('on');
    b.classList.add('saving');
    try {
      await invoke('write_gear', { target: 'A' + a, param: { param: 'group', index: i }, value: want ? 1 : 0 });
      const lo = await gq(a, 0xC0), hi = await gq(a, 0xC1);
      g.groups = ((hi ?? 0) << 8) | (lo ?? 0);
      $$('[data-g]').forEach((x) => x.classList.toggle('on', !!(g.groups & (1 << Number(x.dataset.g)))));
      flashOk(b, !!(g.groups & (1 << i)) === want);
      renderTree();
    } catch (x) { toast('Group change failed', err(x), 'err'); }
    b.classList.remove('saving');
  }));

  // scenes
  $$('[data-s]').forEach((el) => el.addEventListener('change', async () => {
    const i = Number(el.dataset.s);
    const raw = el.value.trim();
    const val = raw === '' ? 255 : Number(raw);
    if (!Number.isInteger(val) || val < 0 || val > 255) { flashOk(el, false); return; }
    el.classList.add('saving');
    try {
      await invoke('write_gear', { target: 'A' + a, param: { param: 'scene', index: i }, value: val });
      const r = await gq(a, 0xB0 + i);
      g.scenes[i] = r === 255 ? null : r;
      el.value = g.scenes[i] ?? '';
      flashOk(el, (r ?? 255) === val);
    } catch (x) { flashOk(el, false); toast('Scene write failed', err(x), 'err'); }
    el.classList.remove('saving');
  }));
}

/* ---------- control devices ---------- */

const OCC_FILTER = ['occupied', 'vacant', 'repeat', 'movement', 'no movement'];

function renderDev(d) {
  const a = d.address;
  const tags = [[`103 v${versionText(d.version)}`, false], [`${d.instance_count ?? 0} instance${d.instance_count === 1 ? '' : 's'}`, true]];
  const grpOpts = (v) => `<option value="255" ${v === 255 || v == null ? 'selected' : ''}>none</option>` + [...Array(32).keys()].map((n) => `<option value="${n}" ${n === v ? 'selected' : ''}>${n}</option>`).join('');
  const statusBits = (v) => v == null ? '—' : ['input device error', 'quiescent', 'no short address', 'app. active', 'app. controller error', 'power cycle seen', 'reset state'].filter((_, i) => v & (1 << i)).join(', ') || 'OK';
  const inst = (i) => {
    const occ = i.occupancy;
    const isOcc = i.instance_type === 3;
    const filt = i.event_filter ?? 0;
    return `<div class="inst" data-i="${i.index}">
      <h4><span>Instance ${i.index} · ${esc(IT_NAMES[i.instance_type] ?? 'type ' + i.instance_type)}</span>
        <label class="switch"><input type="checkbox" data-ip="instance_enabled" ${i.enabled ? 'checked' : ''}><span class="track"></span><span class="switch-label">Enabled</span></label></h4>
      <div class="form">
        <label>Event scheme</label><div class="ctl"><select data-ip="event_scheme">${SCHEMES.map((s, n) => `<option value="${n}" ${n === i.event_scheme ? 'selected' : ''}>${n} · ${s}</option>`).join('')}</select></div>
        <label>Event priority</label><div class="ctl"><select data-ip="event_priority">${[2, 3, 4, 5].map((n) => `<option ${n === i.event_priority ? 'selected' : ''}>${n}</option>`).join('')}</select></div>
        <label>Primary instance group</label><div class="ctl"><select data-ip="primary_group">${grpOpts(i.primary_group)}</select></div>
        <label>Instance group 1</label><div class="ctl"><select data-ip="instance_group1">${grpOpts(i.group1)}</select></div>
        <label>Instance group 2</label><div class="ctl"><select data-ip="instance_group2">${grpOpts(i.group2)}</select></div>
        <label>Event filter</label><div class="ctl">${isOcc
          ? `<div class="filterbits">${OCC_FILTER.map((n, b) => `<label class="check"><input type="checkbox" data-fb="${b}" ${filt & (1 << b) ? 'checked' : ''}> ${n}</label>`).join('')}</div>`
          : `<input class="mono" data-ip="event_filter" value="0x${filt.toString(16).toUpperCase().padStart(6, '0')}" style="width:100px">`}</div>
        ${isOcc && occ ? `
        <label>Deadtime timer<small>× 50 ms</small></label><div class="ctl"><span class="conv" data-c="deadtime">${occ.deadtime != null ? occ.deadtime * 50 + ' ms' : '—'}</span><input type="number" min="0" max="255" data-ip="deadtime" value="${occ.deadtime ?? ''}"></div>
        <label>Hold timer<small>× 10 s (not used by presence sensors)</small></label><div class="ctl"><span class="conv" data-c="hold">${occ.hold != null && occ.hold !== 255 ? occ.hold * 10 + ' s' : '—'}</span><input type="number" min="0" max="255" data-ip="hold" value="${occ.hold ?? ''}"></div>
        <label>Report timer<small>seconds</small></label><div class="ctl"><span class="conv" data-c="report">${occ.report != null ? occ.report + ' s' : '—'}</span><input type="number" min="0" max="255" data-ip="report" value="${occ.report ?? ''}"></div>` : ''}
        <label>Input value</label><div class="ctl mono">${i.input_value != null ? `${i.input_value} <span class="muted">(${i.resolution ?? '?'} bit)</span>` : '—'}</div>
        <label>Status / error</label><div class="ctl mono">${i.status != null ? '0x' + hex2(i.status) : '—'} / ${i.error != null ? '0x' + hex2(i.error) : 'none'}</div>
      </div></div>`;
  };
  $('#dev-detail').innerHTML = headHtml('D' + a, tags, 'device') + `
  <div class="grid2">
    <section class="card"><h3>Device</h3><div class="form">
      <label>Application controller</label><div class="ctl"><label class="switch"><input type="checkbox" data-dp="application_controller" ${d.application_enabled ? 'checked' : ''}><span class="track"></span></label></div>
      <label>Power cycle notification</label><div class="ctl"><label class="switch"><input type="checkbox" data-dp="power_cycle_notification" ${d.power_cycle_notification ? 'checked' : ''}><span class="track"></span></label></div>
      <label>Operating mode</label><div class="ctl"><input type="number" min="0" max="255" data-dp="operating_mode" value="${d.operating_mode ?? ''}"></div>
      <label>Status</label><div class="ctl mono">${esc(statusBits(d.status))}</div>
      <label>Capabilities</label><div class="ctl mono">${d.capabilities != null ? '0x' + hex2(d.capabilities) : '—'}</div>
    </div></section>
    <section class="card"><h3>Device groups</h3><div class="toggles">
      ${[...Array(32).keys()].map((i) => `<button class="tg ${d.groups != null && d.groups & (2 ** i) ? 'on' : ''}" data-dg="${i}">${i}</button>`).join('')}
    </div></section>
    <section class="card" style="grid-column:1/-1"><h3>Instances</h3>${d.instances.length ? d.instances.map(inst).join('') : '<p class="muted">This device reports no instances.</p>'}</section>
    <section class="card"><h3>Identity</h3>${identHtml(d.bank0, [['Random address', d.random_address != null ? '0x' + d.random_address.toString(16).toUpperCase().padStart(6, '0') : '—']])}</section>
    ${bankCard('device', a)}
  </div>`;
  bindHeadActions('device', a);
  bindBank('device', a);

  const write = (param) => invoke('write_device', { address: a, param });

  // device-level
  $$('[data-dp]').forEach((el) => el.addEventListener('change', async () => {
    const k = el.dataset.dp;
    try {
      if (k === 'operating_mode') {
        const v = Number(el.value);
        await write({ param: k, value: v });
        const r = await devq(a, 0xFE, 0x3E);
        el.value = r ?? ''; flashOk(el, r === v);
      } else {
        const want = el.checked;
        await write({ param: k, enabled: want });
        const r = await devq(a, 0xFE, k === 'application_controller' ? 0x3D : 0x45);
        const on = r != null;
        el.checked = on;
        flashOk(el.parentElement, on === want);
        if (on !== want) toast('Not applied', `The device reports ${k.replace(/_/g, ' ')} ${on ? 'enabled' : 'disabled'}.`, 'warn');
      }
    } catch (x) { toast('Write failed', err(x), 'err'); }
  }));
  $$('[data-dg]').forEach((b) => b.addEventListener('click', async () => {
    const g = Number(b.dataset.dg);
    const want = !b.classList.contains('on');
    try {
      await write({ param: 'group', group: g, member: want });
      const r = await devq(a, 0xFE, 0x41 + Math.floor(g / 8));
      const on = r != null && !!(r & (1 << (g % 8)));
      b.classList.toggle('on', on);
      if (d.groups != null) d.groups = on ? d.groups | (2 ** g) : d.groups & ~(2 ** g);
      flashOk(b, on === want);
    } catch (x) { toast('Group change failed', err(x), 'err'); }
  }));

  // instance-level
  const QOP = { event_scheme: 0x8B, event_priority: 0x84, primary_group: 0x88, instance_group1: 0x89, instance_group2: 0x8A, deadtime: 0x2C, hold: 0x2D, report: 0x2E };
  $$('.inst').forEach((card) => {
    const i = Number(card.dataset.i);
    const instObj = d.instances.find((x) => x.index === i);
    $$('[data-ip]', card).forEach((el) => el.addEventListener('change', async () => {
      const k = el.dataset.ip;
      try {
        if (k === 'instance_enabled') {
          const want = el.checked;
          await write({ param: k, instance: i, enabled: want });
          const r = await devq(a, i, 0x86);
          el.checked = r != null;
          flashOk(el.parentElement, el.checked === want);
          instObj.enabled = el.checked;
          return;
        }
        if (k === 'event_filter') {
          const v = parseInt(el.value.replace(/^0x/i, ''), 16);
          if (isNaN(v) || v < 0 || v > 0xFFFFFF) { flashOk(el, false); return; }
          await write({ param: k, instance: i, value: v });
          const b0 = await devq(a, i, 0x90), b1 = await devq(a, i, 0x91), b2 = await devq(a, i, 0x92);
          const got = (b0 ?? 0) | ((b1 ?? 0) << 8) | ((b2 ?? 0) << 16);
          el.value = '0x' + got.toString(16).toUpperCase().padStart(6, '0');
          instObj.event_filter = got;
          flashOk(el, got === v);
          return;
        }
        const v = Number(el.value);
        if (!Number.isInteger(v) || v < 0 || v > 255) { flashOk(el, false); return; }
        await write({ param: k, instance: i, value: v });
        const r = await devq(a, i, QOP[k]);
        el.value = r ?? '';
        flashOk(el, r === v);
        if (r !== v) toast('Value adjusted by device', `${k.replace(/_/g, ' ')}: requested ${v}, device reports ${r ?? 'no answer'}`, 'warn');
        const c = $(`[data-c=${k}]`, card);
        if (c && r != null) c.textContent = k === 'deadtime' ? r * 50 + ' ms' : k === 'hold' ? r * 10 + ' s' : r + ' s';
        if (k === 'event_scheme') instObj.event_scheme = r;
        if (instObj.occupancy && k in instObj.occupancy) instObj.occupancy[k] = r;
      } catch (x) { flashOk(el, false); toast('Write failed', err(x), 'err'); }
    }));
    $$('[data-fb]', card).forEach((cb) => cb.addEventListener('change', async () => {
      let v = 0;
      $$('[data-fb]', card).forEach((x) => { if (x.checked) v |= 1 << Number(x.dataset.fb); });
      try {
        await write({ param: 'event_filter', instance: i, value: v });
        const r = await devq(a, i, 0x90);
        flashOk(cb.parentElement, r === v);
        instObj.event_filter = r;
      } catch (x) { toast('Write failed', err(x), 'err'); }
    }));
  });
}

/* ------------------------------------------------------------
   Settings
   ------------------------------------------------------------ */

async function saveConfig(cfg) {
  S.config = cfg;
  await call('set_config', { config: cfg });
  fillSettings();
  renderChips();
  updateEmpty();
}

function fillSettings() {
  const c = S.config;
  $('#s-auto').checked = !c.paused;
  $('#s-probe-all').checked = !!c.probe_all_usb;
  $('#s-sim').checked = !!c.simulate;
  $('#s-raw').checked = !!c.raw_log;
  const pinned = c.pinned_port || '';
  const known = S.ports.some((p) => p.path === pinned);
  $('#s-port').innerHTML = `<option value="">Automatic</option>` + S.ports.map((p) => `<option value="${esc(p.path)}" ${p.path === pinned ? 'selected' : ''}>${esc(p.path)}${p.lunatone ? ' · Lunatone' : ''}</option>`).join('');
  $('#s-port-manual').value = known ? '' : pinned;
  $('#s-time').value = prefs.time;
  $('#s-theme').value = prefs.theme;
  $('#s-cap').value = String(prefs.cap);
}

async function refreshPorts() {
  try { S.ports = await invoke('list_ports'); } catch { S.ports = []; }
  $('#s-ports').innerHTML = S.ports.length
    ? S.ports.map((p) => `<tr><td>${esc(p.path)}</td><td>${p.vid != null ? hex2(p.vid >> 8) + hex2(p.vid & 255) + ':' + hex2(p.pid >> 8) + hex2(p.pid & 255) : '—'}</td><td>${esc(p.product || '—')}</td><td>${esc(p.serial || '—')}</td><td>${p.lunatone ? '<span class="tag acc">Lunatone</span>' : ''}</td></tr>`).join('')
    : `<tr><td colspan="5" class="muted">No serial ports found.</td></tr>`;
  const sel = $('#s-port');
  if (document.activeElement !== sel && document.activeElement !== $('#s-port-manual')) fillSettings();
}
setInterval(() => { if ($('#view-settings').classList.contains('active')) refreshPorts(); }, 2000);

$('#s-auto').addEventListener('change', (e) => saveConfig({ ...S.config, paused: !e.target.checked }));
$('#s-probe-all').addEventListener('change', (e) => saveConfig({ ...S.config, probe_all_usb: e.target.checked }));
$('#s-sim').addEventListener('change', (e) => saveConfig({ ...S.config, simulate: e.target.checked }));
$('#s-raw').addEventListener('change', (e) => saveConfig({ ...S.config, raw_log: e.target.checked }));
$('#s-port').addEventListener('change', (e) => saveConfig({ ...S.config, pinned_port: e.target.value || null }));
$('#s-port-manual').addEventListener('change', (e) => saveConfig({ ...S.config, pinned_port: e.target.value.trim() || null }));
$('#s-rescan').addEventListener('click', () => call('rescan'));
$('#s-reconnect').addEventListener('click', () => call('reconnect'));
function applyTheme() {
  if (prefs.theme === 'light' || prefs.theme === 'dark') document.documentElement.dataset.theme = prefs.theme;
  else delete document.documentElement.dataset.theme;
}
applyTheme();
$('#s-theme').addEventListener('change', (e) => { prefs.theme = e.target.value; savePrefs(); applyTheme(); });
$('#s-time').addEventListener('change', (e) => { prefs.time = e.target.value; savePrefs(); scheduleRender(true); });
$('#s-cap').addEventListener('change', (e) => { prefs.cap = Number(e.target.value); savePrefs(); });

/* ------------------------------------------------------------
   Dropdowns
   Native <select> popups render as detached Wayland popups under
   WebKitGTK (and log Gdk warnings on WSLg), so every <select> gets a
   styled button + menu. The real <select> stays in the DOM, keeps its id,
   value and 'change' events, so the rest of the code is unaware.
   ------------------------------------------------------------ */

const VALUE_DESC = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value');
let ddOpen = null;

function enhanceSelect(sel) {
  if (sel._dd) return;
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'dd-btn';
  const wrap = document.createElement('span');
  wrap.className = 'dd';
  sel.replaceWith(wrap);
  wrap.append(sel, btn);
  sel.classList.add('dd-native');
  sel.tabIndex = -1;
  const sync = () => {
    const o = sel.options[sel.selectedIndex];
    btn.textContent = o ? o.textContent : '';
    btn.disabled = sel.disabled;
    if (sel.title) btn.title = sel.title;
  };
  Object.defineProperty(sel, 'value', {
    configurable: true,
    get() { return VALUE_DESC.get.call(this); },
    set(v) { VALUE_DESC.set.call(this, v); sync(); },
  });
  new MutationObserver(sync).observe(sel, { childList: true, subtree: true, attributes: true, attributeFilter: ['disabled'] });
  sel.addEventListener('change', sync);
  btn.addEventListener('click', (e) => { e.stopPropagation(); ddOpen && ddOpen.sel === sel ? closeDropdown() : openDropdown(sel, btn); });
  btn.addEventListener('keydown', (e) => {
    if (ddOpen && ddOpen.sel === sel) return; // the open menu handles keys
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp' || e.key === 'Enter' || e.key === ' ') { e.preventDefault(); e.stopPropagation(); openDropdown(sel, btn); }
  });
  sel._dd = { sync, btn };
  sync();
}

function openDropdown(sel, btn) {
  closeDropdown();
  const menu = document.createElement('div');
  menu.className = 'dd-menu';
  menu.setAttribute('role', 'listbox');
  const opts = Array.from(sel.options);
  let active = Math.max(0, sel.selectedIndex);
  menu.innerHTML = opts.map((o, i) =>
    `<div class="dd-opt ${i === sel.selectedIndex && o.value !== '' ? 'sel' : ''} ${o.value === '' && i === 0 && sel.id === 'tx-template' ? 'placeholder' : ''}" data-i="${i}" role="option">${esc(o.textContent)}</div>`).join('');
  document.body.appendChild(menu);
  const r = btn.getBoundingClientRect();
  menu.style.minWidth = r.width + 'px';
  const h = Math.min(menu.scrollHeight, 320);
  const below = window.innerHeight - r.bottom - 8;
  menu.style.left = Math.min(r.left, window.innerWidth - menu.offsetWidth - 8) + 'px';
  menu.style.top = (below >= h || below > r.top ? r.bottom + 4 : r.top - h - 4) + 'px';
  const items = $$('.dd-opt', menu);
  const setActive = (i) => {
    items.forEach((x) => x.classList.remove('active'));
    active = Math.max(0, Math.min(items.length - 1, i));
    items[active].classList.add('active');
    items[active].scrollIntoView({ block: 'nearest' });
  };
  const choose = (i) => {
    closeDropdown();
    btn.focus();
    if (i === sel.selectedIndex && sel.id !== 'tx-template') return;
    sel.selectedIndex = i;
    sel._dd.sync();
    sel.dispatchEvent(new Event('change', { bubbles: true }));
  };
  items.forEach((x) => {
    x.addEventListener('mousemove', () => setActive(Number(x.dataset.i)));
    x.addEventListener('click', (e) => { e.stopPropagation(); choose(Number(x.dataset.i)); });
  });
  const onKey = (e) => {
    const stop = () => { e.preventDefault(); e.stopPropagation(); };
    if (e.key === 'ArrowDown') { stop(); setActive(active + 1); }
    else if (e.key === 'ArrowUp') { stop(); setActive(active - 1); }
    else if (e.key === 'Home') { stop(); setActive(0); }
    else if (e.key === 'End') { stop(); setActive(items.length - 1); }
    else if (e.key === 'Enter' || e.key === ' ') { stop(); choose(active); }
    else if (e.key === 'Escape') { stop(); closeDropdown(); btn.focus(); }
    else if (e.key === 'Tab') closeDropdown();
  };
  document.addEventListener('keydown', onKey, true);
  setActive(active);
  btn.classList.add('open');
  ddOpen = { sel, btn, menu, onKey };
}

function closeDropdown() {
  if (!ddOpen) return false;
  ddOpen.menu.remove();
  ddOpen.btn.classList.remove('open');
  document.removeEventListener('keydown', ddOpen.onKey, true);
  ddOpen = null;
  return true;
}

function initDropdowns() {
  $$('select').forEach(enhanceSelect);
  new MutationObserver((muts) => {
    for (const m of muts) {
      for (const n of m.addedNodes) {
        if (n.nodeType !== 1) continue;
        if (n.tagName === 'SELECT') enhanceSelect(n);
        else n.querySelectorAll && n.querySelectorAll('select').forEach(enhanceSelect);
      }
    }
  }).observe(document.body, { childList: true, subtree: true });
  document.addEventListener('mousedown', (e) => { if (ddOpen && !ddOpen.menu.contains(e.target) && e.target !== ddOpen.btn) closeDropdown(); });
  window.addEventListener('resize', closeDropdown);
  // Close only when something that contains the button scrolls (not the live monitor list).
  document.addEventListener('scroll', (e) => {
    if (ddOpen && !ddOpen.menu.contains(e.target) && e.target.contains && e.target.contains(ddOpen.btn)) closeDropdown();
  }, true);
}

/* ------------------------------------------------------------
   Boot
   ------------------------------------------------------------ */

(async function boot() {
  initDropdowns();
  renderChips();
  listen('monitor://batch', (e) => onBatch(e.payload));
  listen('dongle://state', (e) => setConn(e.payload));
  listen('dongle://diag', (e) => {
    const prev = S.diag;
    S.diag = e.payload;
    setBusChip();
    if (!prev || JSON.stringify(prev.line) !== JSON.stringify(S.diag.line) || prev.events !== S.diag.events || prev.bus_power !== S.diag.bus_power) updateEmpty();
    if (!$('#conn-pop').classList.contains('hidden')) renderPopover();
  });
  listen('job://progress', (e) => {
    const p = e.payload.progress;
    if (!S.job) return;
    $('#job-text').textContent = p.message || p.phase;
    $('#job-bar').style.width = Math.round((p.fraction || 0) * 100) + '%';
    const log = $('#addr-log');
    if (log && e.payload.job === 'address' && p.phase === 'address') {
      log.insertAdjacentHTML('beforeend', `<div>${esc(p.message)}</div>`);
    }
  });
  const snap = await invoke('get_snapshot');
  S.config = snap.config;
  renderChips();
  S.ports = snap.ports;
  $('#s-version').textContent = 'v' + snap.version;
  fillSettings();
  setConn(snap.state);
  const prior = await invoke('monitor_snapshot', { max: Number(prefs.cap) || 100000 });
  if (prior.length) ingest(prior);
  renderWelcome();
  updateEmpty();
  scheduleRender(true);
})();
