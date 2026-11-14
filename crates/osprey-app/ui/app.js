// BM Osprey browser interface. Plain JavaScript, no framework, so it
// loads instantly and costs almost nothing while idle. All state comes
// from the Rust side as events.
'use strict';

const T = window.__TAURI_INTERNALS__;
const invoke = (cmd, args = {}) => T.invoke(cmd, args);
const listen = (event, cb) =>
  invoke('plugin:event|listen', { event, target: { kind: 'Any' }, handler: T.transformCallback((e) => cb(e.payload)) });

const $ = (id) => document.getElementById(id);
const state = { tabs: [], active: null, media: {}, thumbs: {}, pageThumb: {}, closed: {}, downloads: [], net: null, panel: null, scripts: null, settings: null, settingsOpen: false };

function esc(s) {
  return String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}
function activeTab() { return state.tabs.find((t) => t.id === state.active); }
function bytes(n) {
  if (!n) return '0 B';
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return (i ? n.toFixed(1) : n) + ' ' + u[i];
}
function hostOf(url) { try { return new URL(url).host; } catch { return ''; } }
function letterOf(t) {
  const h = hostOf(t.url).replace(/^www\./, '');
  return (h || t.title || '·').charAt(0);
}
// Errors show for a few seconds in the tab strip, so nothing fails silently.
let toastTimer = 0;
function toast(text, action, kind) {
  const t = $('toast');
  $('toast-text').textContent = String(text).slice(0, 200);
  const b = $('toast-action');
  b.hidden = !action;
  if (action) { b.textContent = action.label; b.onclick = () => { t.hidden = true; action.run(); }; }
  t.classList.toggle('info', kind === 'info');
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, action ? 9000 : 6000);
}
function report(err) {
  console.warn(err);
  toast(err && err.message ? err.message : err);
}

// ---------- tabs ----------
function renderTabs() {
  const wrap = $('tabs');
  wrap.innerHTML = state.tabs.map((t) => `
    <div class="tab${t.id === state.active ? ' active' : ''}${t.private ? ' private' : ''}${t.loading ? ' loading' : ''}${t.sleeping ? ' sleeping' : ''}"
         data-id="${t.id}" role="tab" title="${esc(t.title || t.url)}">
      <span class="tab-letter">${esc(letterOf(t))}</span>
      <span class="tab-title">${esc(t.title || t.url || 'New tab')}</span>
      ${t.media ? `<span class="tab-media">${t.media}</span>` : ''}
      <button class="tab-close" data-close="${t.id}" title="Close tab (Ctrl+W)"><svg><use href="#i-close"/></svg></button>
    </div>`).join('');
  renderToolbar();
}

$('tabs').addEventListener('mousedown', (e) => {
  const close = e.target.closest('[data-close]');
  const tab = e.target.closest('.tab');
  if (!tab) return;
  const id = Number(tab.dataset.id);
  if (close || e.button === 1) {
    e.preventDefault();
    invoke('tab_close', { id }).catch(report);
  } else if (e.button === 0 && id !== state.active) {
    invoke('tab_activate', { id }).catch(report);
  }
});

// ---------- toolbar ----------
function renderToolbar() {
  const t = activeTab();
  const input = $('address');
  if (document.activeElement !== input) input.value = t ? t.url : '';
  document.body.classList.toggle('private-tab', !!(t && t.private));
  $('reload').innerHTML = `<svg><use href="#i-${t && t.loading ? 'stop' : 'reload'}"/></svg>`;
  $('reload').title = t && t.loading ? 'Stop' : 'Reload (F5)';
  $('text-mode').classList.toggle('on', !!(t && t.text_mode));
  const media = t ? (state.media[t.id] || []).filter((m) => m.state === 'ready').length : 0;
  $('media-count').hidden = !media;
  $('media-count').textContent = media;
  const running = state.downloads.filter((d) => ['queued', 'running', 'merging'].includes(d.state)).length;
  $('dl-count').hidden = !running;
  $('dl-count').textContent = running;
  $('media-btn').classList.toggle('on', state.panel === 'media');
  $('dl-btn').classList.toggle('on', state.panel === 'downloads');
  $('scripts-btn').classList.toggle('on', state.panel === 'scripts');
  $('settings-btn').classList.toggle('on', state.settingsOpen);
}

$('address-form').addEventListener('submit', (e) => {
  e.preventDefault();
  const t = activeTab();
  const input = $('address').value.trim();
  if (!input) return;
  $('address').blur();
  if (t) invoke('tab_navigate', { id: t.id, input }).catch(report);
  else invoke('tab_new', { url: input, private: false }).catch(report);
});
$('address').addEventListener('focus', (e) => e.target.select());
$('address').addEventListener('keydown', (e) => {
  if (e.key === 'Escape') { e.target.value = activeTab()?.url || ''; e.target.blur(); }
});

const onActive = (cmd) => () => { const t = activeTab(); if (t) invoke(cmd, { id: t.id }).catch(report); };
$('back').onclick = onActive('tab_back');
$('fwd').onclick = onActive('tab_forward');
$('reload').onclick = () => { const t = activeTab(); if (t) invoke(t.loading ? 'tab_stop' : 'tab_reload', { id: t.id }).catch(report); };
$('new-tab').onclick = () => invoke('tab_new', { url: null, private: false }).catch(report);
$('new-private').onclick = () => invoke('tab_new', { url: null, private: true }).catch(report);
$('text-mode').onclick = () => { const t = activeTab(); if (t) invoke('tab_text_mode', { id: t.id, on: !t.text_mode }).catch(report); };
$('media-btn').onclick = () => togglePanel('media');
$('dl-btn').onclick = () => togglePanel('downloads');
$('scripts-btn').onclick = () => togglePanel('scripts');
$('route-pill').onclick = () => togglePanel('network');
$('settings-btn').onclick = () => toggleSettings();

// ---------- side panel ----------
function togglePanel(name) {
  if (state.settingsOpen) toggleSettings(false);
  state.panel = state.panel === name ? null : name;
  if (state.panel !== 'media') stopPreview();
  $('panel').hidden = !state.panel;
  document.querySelectorAll('.panel-nav button').forEach((b) => b.classList.toggle('on', b.dataset.panel === state.panel));
  document.querySelectorAll('.panel-body').forEach((s) => s.classList.toggle('on', s.dataset.section === state.panel));
  invoke('layout', { panelOpen: !!state.panel, covered: null }).catch(report);
  if (state.panel === 'network') invoke('net_status').then(renderNet).catch(report);
  if (state.panel === 'scripts') invoke('scripts_view').then(renderScripts).catch(report);
  renderToolbar();
}
document.querySelector('.panel-nav').addEventListener('click', (e) => {
  const b = e.target.closest('button');
  if (b && b.dataset.panel !== state.panel) togglePanel(b.dataset.panel);
});

function clock(s) {
  if (!s || !isFinite(s)) return '';
  s = Math.round(s);
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  const two = (n) => String(n).padStart(2, '0');
  return h ? `${h}:${two(m)}:${two(sec)}` : `${m}:${two(sec)}`;
}
function rate(bps) { return bps >= 1e6 ? (bps / 1e6).toFixed(1) + ' Mbps' : Math.round(bps / 1e3) + ' kbps'; }
function resLabel(w, h) { return h ? `${h}p` : (w ? `${w}w` : 'auto'); }

// ---------- media cards ----------
// One card per video: a picture, the title, and a row per quality with the
// best one first and highlighted. Cards sit in groups by what they are.
// Anything the server check could not confirm goes under Unidentified, with
// the reason, so nothing disappears without a word.
const previewBtn = (m, name, extra = '') => `<button class="mini" data-preview="${esc(m.url)}" data-name="${esc(name)}" ${extra} title="Preview"><svg><use href="#i-play"/></svg></button>`;

// The page's own picture (og:image) or yt-dlp's thumbnail, loaded through
// the local preview server, so the interface never loads from the web.
function thumbFor(t, m) {
  const src = (m.extracted && m.extracted.thumbnail) || state.pageThumb[t.id];
  if (!src) return '';
  const key = t.id + ' ' + src;
  const have = state.thumbs[key];
  if (have === undefined) {
    state.thumbs[key] = null;
    invoke('media_thumb', { tab: t.id, url: src })
      .then((local) => { state.thumbs[key] = local; if (t.id === state.active) renderMedia(); })
      .catch(() => { state.thumbs[key] = ''; });
  }
  return have ? `<img src="${esc(have)}" alt="" draggable="false">` : '';
}

// The kind tag, then the facts with dots between them.
const subLine = (parts) => parts[0] + ' ' + parts.slice(1).join(' · ');

function cardHead(t, m, title, sub, len, audio) {
  const shade = ['', ' b', ' c'][(m.url.length + (m.host || '').length) % 3];
  return `
    <div class="head">
      <div class="thumb${shade}${audio ? ' audio' : ''}"${len ? ` data-len="${esc(len)}"` : ''}>${audio ? '' : thumbFor(t, m)}</div>
      <div class="meta">
        <div class="title" title="${esc(m.url)}">${esc(title)}</div>
        <div class="card-sub">${sub}</div>
      </div>
    </div>`;
}

// A page yt-dlp knows: a row for each format, best first, audio only at the
// end. A video without sound of its own is fetched with its paired audio,
// so its row counts both sizes. DASH rows wait for DASH support.
function extractedCard(t, m) {
  const x = m.extracted;
  const rank = (f) => [f.audio_only ? 0 : 1, f.height || 0, f.bitrate || 0];
  const best = (a, b) => {
    const [x1, x2] = [rank(a), rank(b)];
    return x2[0] - x1[0] || x2[1] - x1[1] || x2[2] - x1[2];
  };
  const formats = x.formats.filter((f) => f.kind !== 'Dash').sort(best);
  const rows = formats.map((f, i) => {
    const res = f.audio_only ? 'audio' : (f.height ? resLabel(f.width, f.height) : esc(f.id));
    const paired = x.formats.find((a) => a.id === f.pairs_with);
    const silent = f.video_only && !paired;
    const words = f.audio_only ? (f.note || '').replace(/[()]/g, '') : '';
    let note;
    if (f.audio_only) note = words || f.codec_name || f.ext || '';
    else if (silent) note = 'video only';
    else note = 'with sound';
    const total = f.size ? f.size + ((paired && paired.size) || 0) : 0;
    const size = total ? (f.size_is_estimate ? '~' : '') + bytes(total) : '';
    const top = i === 0;
    return `
      <div class="row">
        <b class="${top ? 'top' : ''}">${res}</b>
        <span>${esc(note)}</span>
        <span class="sz">${size}</span>
        ${previewBtn(m, m.name, `data-format="${esc(f.id)}"`)}
        <button class="mini${top ? ' go' : ''}" data-url="${esc(m.url)}" data-format="${esc(f.id)}" title="Download ${res}"><svg><use href="#i-down"/></svg></button>
      </div>`;
  }).join('');
  const video = formats.filter((f) => !f.audio_only).length;
  const sub = [`<span class="tag">${esc(x.extractor)}</span>`, esc(m.host)];
  if (video) sub.push(`${video} ${video === 1 ? 'quality' : 'qualities'}`);
  if (x.subtitles.length) sub.push(`${x.subtitles.length} subtitles`);
  if (x.drm_formats) sub.push(`${x.drm_formats} DRM formats left out`);
  const len = x.duration ? (x.live ? 'live' : clock(x.duration)) : '';
  return `<div class="card">${cardHead(t, m, x.title || m.name, subLine(sub), len, !video)}
    ${rows ? `<div class="rows">${rows}</div>` : ''}</div>`;
}

function mediaCard(t, m) {
  if (m.extracted) return extractedCard(t, m);
  const i = m.info || {};
  const stream = m.kind === 'Hls' || m.kind === 'Dash';
  // Streams are named after the page. A playlist's own name ("master",
  // "index") says nothing.
  const name = stream ? (t.title || m.name) : m.name;
  const w = i.width || m.player_w, h = i.height || m.player_h, dur = i.duration || m.player_dur;
  const label = m.kind === 'Direct' ? (i.audio_only ? 'audio' : 'file') : m.kind.toLowerCase();
  const tagClass = m.kind === 'Dash' ? 'dash' : (i.audio_only ? 'audio' : '');
  const sub = [`<span class="tag ${tagClass}">${label}</span>`, esc(m.host)];
  const len = dur ? (i.live ? 'live' : clock(dur)) : '';
  if (m.state === 'checking') {
    return `<div class="card checking">${cardHead(t, m, name, 'Checking with the server', len, i.audio_only)}</div>`;
  }
  if (m.state !== 'ready') {
    return `<div class="card">${cardHead(t, m, name, subLine(sub), len, i.audio_only)}
      <div class="card-why">${esc(i.blocked || 'The server check could not confirm this is media.')}</div></div>`;
  }
  let rows;
  if (i.qualities && i.qualities.length) {
    sub.push(`${i.qualities.length} ${i.qualities.length === 1 ? 'quality' : 'qualities'}`);
    rows = i.qualities.map((q, n) => `
      <div class="row">
        <b class="${n === 0 ? 'top' : ''}">${resLabel(q.width, q.height)}</b>
        <span>${q.bandwidth ? rate(q.bandwidth) : ''}${i.separate_audio ? ' · separate audio' : ''}</span>
        <span class="sz">${q.est_size ? '~' + bytes(q.est_size) : ''}</span>
        ${n === 0 ? previewBtn(m, name) : '<span></span>'}
        <button class="mini${n === 0 ? ' go' : ''}" data-url="${esc(m.url)}" data-height="${q.height || ''}" title="Download ${resLabel(q.width, q.height)}"><svg><use href="#i-down"/></svg></button>
      </div>`).join('');
  } else {
    const res = h ? resLabel(w, h) : (i.audio_only ? 'audio' : label);
    rows = `
      <div class="row">
        <b class="top">${res}</b>
        <span>${w && h ? `${w}×${h}` : ''}</span>
        <span class="sz">${i.size ? bytes(i.size) : ''}</span>
        ${previewBtn(m, name)}
        <button class="mini go" data-url="${esc(m.url)}" title="Download"><svg><use href="#i-down"/></svg></button>
      </div>`;
  }
  return `<div class="card">${cardHead(t, m, name, subLine(sub), len, i.audio_only)}<div class="rows">${rows}</div></div>`;
}

const GROUPS = [['video', 'Video'], ['audio', 'Audio'], ['images', 'Images'], ['subtitles', 'Subtitles'], ['unidentified', 'Unidentified']];
function groupOf(m) {
  if (m.state === 'blocked') return 'unidentified';
  if (m.extracted) return m.extracted.formats.some((f) => !f.audio_only) ? 'video' : 'audio';
  if (m.info && m.info.audio_only) return 'audio';
  return 'video';
}

function renderMedia() {
  const t = activeTab();
  const items = t ? state.media[t.id] || [] : [];
  const ready = items.filter((m) => m.state === 'ready').length;
  const checking = items.filter((m) => m.state === 'checking').length;
  $('media-intro').textContent = items.length
    ? `${ready} found on this page${checking ? `, ${checking} still being checked` : ''}. Each one was checked with the server.`
    : 'Nothing yet. Start playing the video on the page and it will show up here with its qualities. Ads are left out.';
  const by = {};
  for (const m of items) (by[groupOf(m)] ||= []).push(m);
  // Empty groups are left out. Images and subtitles are not collected yet.
  $('media-groups').innerHTML = GROUPS.filter(([k]) => by[k]).map(([k, label]) => `
    <details class="group" data-group="${k}" ${state.closed[k] ? '' : 'open'}>
      <summary><svg><use href="#i-chev"/></svg>${label}<b>${by[k].length}</b></summary>
      ${by[k].map((m) => mediaCard(t, m)).join('')}
    </details>`).join('');
  renderToolbar();
}
// A picture that does not load leaves the plain gradient behind.
$('media-groups').addEventListener('error', (e) => { if (e.target.tagName === 'IMG') e.target.remove(); }, true);
$('media-groups').addEventListener('toggle', (e) => {
  const g = e.target;
  if (g.classList && g.classList.contains('group')) state.closed[g.dataset.group] = !g.open;
}, true);

// ---------- preview ----------
// hls.js is loaded the first time a preview is opened, so it costs
// nothing until then. It plays HLS on every platform. WebView2 156 says it
// can play HLS ("maybe") but failed on a real stream, so hls.js goes first.
let hlsReady = null;
let hlsPlayer = null;
function loadHls() {
  if (window.Hls) return Promise.resolve();
  if (!hlsReady) {
    hlsReady = new Promise((ok, fail) => {
      const s = document.createElement('script');
      s.src = 'vendor/hls.min.js';
      s.onload = ok;
      s.onerror = () => fail(new Error('the preview player could not be loaded'));
      document.head.appendChild(s);
    });
  }
  return hlsReady;
}
function stopPreview() {
  if (hlsPlayer) { hlsPlayer.destroy(); hlsPlayer = null; }
  const v = $('preview-video');
  v.pause();
  v.removeAttribute('src');
  v.load();
  $('preview').hidden = true;
}
async function startPreview(tab, url, name, format) {
  stopPreview();
  $('preview-title').textContent = 'Loading preview of ' + name;
  $('preview').hidden = false;
  const p = await invoke('media_preview', { tab, url, format: format || null });
  const v = $('preview-video');
  if (p.kind === 'Hls') {
    // hls.js comes first. The engine's own HLS is only for a system where
    // hls.js cannot run, since an engine can say it plays HLS and then fail.
    await loadHls();
    if (window.Hls.isSupported()) {
      // No worker: keeps the app's strict security policy as it is.
      hlsPlayer = new window.Hls({ enableWorker: false, maxBufferLength: 20, backBufferLength: 10 });
      hlsPlayer.on(window.Hls.Events.ERROR, (_, data) => {
        if (data.fatal) { report('Preview stopped: ' + (data.details || 'stream error')); }
      });
      hlsPlayer.loadSource(p.url);
      hlsPlayer.attachMedia(v);
    } else if (v.canPlayType('application/vnd.apple.mpegurl')) {
      v.src = p.url;
    } else {
      throw new Error('this system cannot play stream previews');
    }
  } else {
    v.src = p.url;
  }
  $('preview-title').textContent = name;
  v.play().catch(() => {});
}
$('preview-close').onclick = stopPreview;

$('media-groups').addEventListener('click', (e) => {
  const pv = e.target.closest('[data-preview]');
  if (pv) {
    const t = activeTab();
    if (t) startPreview(t.id, pv.dataset.preview, pv.dataset.name, pv.dataset.format).catch((err) => { stopPreview(); report(err); });
    return;
  }
  const b = e.target.closest('[data-url]');
  const t = activeTab();
  if (!b || !t || b.disabled) return;
  b.disabled = true;
  b.title = 'Added to downloads';
  const height = b.dataset.height ? Number(b.dataset.height) : null;
  invoke('media_download', { tab: t.id, url: b.dataset.url, height, format: b.dataset.format || null }).catch((err) => {
    b.disabled = false;
    report(err);
  });
});

const STATE_TEXT = { queued: 'Waiting', running: 'Downloading', merging: 'Joining pieces', paused: 'Paused', done: 'Done', failed: 'Failed', cancelled: 'Cancelled' };
function eta(s) {
  if (s == null) return '';
  if (s < 60) return `${s} s left`;
  if (s < 3600) return `${Math.round(s / 60)} min left`;
  return `${Math.floor(s / 3600)} h ${Math.round((s % 3600) / 60)} min left`;
}
function dlButtons(d) {
  const b = (act, icon, label) => `<button class="chip" data-act="${act}" data-id="${d.id}"><svg><use href="#i-${icon}"/></svg>${label}</button>`;
  switch (d.state) {
    case 'queued': case 'running': return b('pause', 'pause', 'Pause') + b('cancel', 'close', 'Cancel');
    case 'merging': return '';
    case 'paused': return b('resume', 'play', 'Resume') + b('cancel', 'close', 'Cancel');
    case 'failed': case 'cancelled': return b('resume', 'retry', 'Retry');
    case 'done': return b('open', 'play', 'Play') + b('show', 'folder', 'Show in folder');
    default: return '';
  }
}
// The bar glows with a moving sheen while a download runs, turns grey when
// paused and green when done.
function renderDownloads() {
  const list = [...state.downloads].reverse();
  $('dl-list').innerHTML = list.length ? list.map((d) => {
    const pct = d.total ? Math.min(100, (d.done / d.total) * 100) : (d.state === 'done' ? 100 : 0);
    const amount = d.unit === 'segments'
      ? `<b>${d.done}${d.total ? ' / ' + d.total : ''}</b> pieces${d.bytes ? ' · ' + bytes(d.bytes) : ''}`
      : `<b>${bytes(d.done)}</b>${d.total ? ' of ' + bytes(d.total) : ''}`;
    const live = d.state === 'running';
    const left = d.state === 'done' ? `<b>${STATE_TEXT.done}</b>` : amount;
    const right = live
      ? `${d.speed ? `<b>${bytes(d.speed)}/s</b>` : ''}${d.eta != null ? ' · ' + eta(d.eta) : ''}`
      : `${STATE_TEXT[d.state] || d.state}${d.private ? ' · private' : ''}`;
    const note = d.note && ['running', 'paused', 'queued', 'merging', 'done'].includes(d.state) && !['video', 'audio', 'download'].includes(d.note) ? d.note : '';
    return `
    <div class="card dl">
      <div class="name" title="${esc(d.path || d.url)}">${esc(d.name)}</div>
      <div class="barx ${d.state}"><i style="width:${pct.toFixed(1)}%"></i></div>
      <div class="stat"><span>${left}</span><span>${d.state === 'done' ? '' : right}</span></div>
      ${note ? `<div class="dl-note">${esc(note)}</div>` : ''}
      ${d.error ? `<div class="dl-err">${esc(d.error)}</div>` : ''}
      <div class="acts">${dlButtons(d)}</div>
    </div>`;
  }).join('') : '<p class="panel-intro">No downloads yet. Find media in the Media tab.</p>';
  renderToolbar();
}
$('dl-list').addEventListener('click', (e) => {
  const b = e.target.closest('[data-act]');
  if (!b) return;
  const id = Number(b.dataset.id);
  const cmd = { pause: 'download_pause', resume: 'download_resume', cancel: 'download_cancel', open: 'download_open', show: 'download_show' }[b.dataset.act];
  if (cmd) invoke(cmd, { id }).catch(report);
});
$('open-folder').onclick = () => invoke('download_open_folder').catch(report);

// ---------- network ----------
function renderNet(n) {
  if (!n) return;
  state.net = n;
  document.body.dataset.route = n.key;
  document.body.classList.toggle('route-down', !n.ok);
  $('route-label').textContent = n.label;
  $('route-pill').title = n.route + (n.ok ? '' : ' (not reachable, nothing is being sent)');

  const s = n.stats;
  $('net-status').innerHTML = `
    <strong>${esc(n.label)}${n.ok ? '' : ' is not reachable'}</strong>
    <span>${esc(n.route)}</span>
    ${!n.ok || s.last_error ? `<span class="problem">${esc(n.problem || s.last_error || '')}</span>` : ''}`;
  document.querySelectorAll('.mode').forEach((m) => {
    const on = m.dataset.mode === n.key;
    m.classList.toggle('on', on);
    m.querySelector('input[type=radio]').checked = on;
  });
  if (n.mode.mode === 'secure_dns') $('dns-provider').value = n.mode.provider;
  if (n.mode.mode === 'vpn' && document.activeElement !== $('vpn-proxy')) $('vpn-proxy').value = n.mode.proxy;
  const found = (el, addr) => { el.textContent = addr ? 'running' : 'not found'; el.classList.toggle('found', !!addr); };
  found($('tor-found'), n.tor_found);
  found($('warp-found'), n.warp_found);
  $('block-ads').checked = n.block_ads;
  $('always-private').checked = n.always_private;
  if (document.activeElement !== $('set-style')) $('set-style').value = n.download_style;
  $('filters-ads').checked = n.block_ads;
  if (document.activeElement !== $('tor-circuits')) $('tor-circuits').value = String(n.tor_circuits);
  $('restore-tabs').checked = n.restore_tabs;
  if (document.activeElement !== $('speed-limit')) $('speed-limit').value = String(n.speed_limit_kbps);
  document.body.classList.toggle('always-private', !!n.always_private);
  $('stats').innerHTML = `
    <div><dt>Blocked</dt><dd>${s.blocked}</dd></div>
    <div><dt>Open now</dt><dd>${s.active}</dd></div>
    <div><dt>Received</dt><dd>${bytes(s.bytes_down)}</dd></div>`;
}

function chooseMode(key) {
  let mode;
  if (key === 'secure_dns') mode = { mode: 'secure_dns', provider: $('dns-provider').value };
  else if (key === 'vpn') {
    const proxy = $('vpn-proxy').value.trim();
    if (!proxy) { $('vpn-proxy').focus(); return; }
    mode = { mode: 'vpn', proxy };
  } else mode = { mode: key };
  invoke('net_set', { mode }).then(renderNet).catch((err) => {
    $('net-status').innerHTML = `<strong>Could not switch</strong><span class="problem">${esc(err)}</span>`;
  });
}
$('modes').addEventListener('change', (e) => {
  if (e.target.id === 'tor-circuits') return;
  if (e.target.name === 'mode') chooseMode(e.target.value);
  if (e.target.id === 'dns-provider') chooseMode('secure_dns');
});
$('vpn-proxy').addEventListener('keydown', (e) => { if (e.key === 'Enter') chooseMode('vpn'); });
const setAds = (on) => invoke('ads_set', { on }).then(() => invoke('net_status')).then(renderNet).catch(report);
$('block-ads').addEventListener('change', (e) => setAds(e.target.checked));
$('filters-ads').addEventListener('change', (e) => setAds(e.target.checked));
const setPrefs = (prefs) => invoke('prefs_set', { prefs }).then(renderNet).catch(report);
$('always-private').addEventListener('change', (e) => setPrefs({ always_private: e.target.checked }));
$('set-style').addEventListener('change', (e) => setPrefs({ download_style: e.target.value }));
$('tor-circuits').addEventListener('change', (e) => setPrefs({ tor_circuits: Number(e.target.value) }));
$('restore-tabs').addEventListener('change', (e) => setPrefs({ restore_tabs: e.target.checked }));
$('speed-limit').addEventListener('change', (e) => setPrefs({ speed_limit_kbps: Number(e.target.value) }));
$('clear-data').onclick = (e) => {
  const b = e.currentTarget;
  invoke('clear_browsing_data').then(() => { b.textContent = 'Cleared'; setTimeout(() => (b.textContent = 'Clear cookies and site data'), 2000); }).catch(report);
};

// ---------- keyboard (when the toolbar has focus) ----------
document.addEventListener('keydown', (e) => {
  const mod = e.ctrlKey || e.metaKey;
  const key = e.key.toLowerCase();
  const t = activeTab();
  if (mod && e.shiftKey && key === 'n') invoke('tab_new', { url: null, private: true });
  else if (mod && key === 't') invoke('tab_new', { url: null, private: false });
  else if (mod && key === 'w' && t) invoke('tab_close', { id: t.id });
  else if (mod && key === 'l') $('address').focus();
  else if (key === 'escape' && state.settingsOpen) toggleSettings(false);
  else if (mod && key === ',') toggleSettings();
  else if (mod && key === 'tab' && state.tabs.length > 1) {
    const i = state.tabs.findIndex((x) => x.id === state.active);
    const n = state.tabs[(i + (e.shiftKey ? state.tabs.length - 1 : 1)) % state.tabs.length];
    invoke('tab_activate', { id: n.id });
  } else if ((key === 'f5' || (mod && key === 'r')) && t) invoke('tab_reload', { id: t.id });
  else if (e.altKey && key === 'arrowleft' && t) invoke('tab_back', { id: t.id });
  else if (e.altKey && key === 'arrowright' && t) invoke('tab_forward', { id: t.id });
  else return;
  e.preventDefault();
});

// ---------- user scripts ----------
function renderScripts(v) {
  state.scripts = v;
  const ran = [...new Map(v.ran.map((r) => [r.id, r])).values()];
  $('scripts-count').textContent = ran.length;
  $('scripts-count').hidden = !ran.length;
  $('scripts-private').hidden = !v.private;
  $('scripts-stale').hidden = !v.stale;
  $('script-asks').innerHTML = v.asks.map((a) => `
    <div class="script-ask">
      <p><b>${esc(a.name)}</b> wants to connect to <b>${esc(a.host)}</b>.</p>
      <div class="panel-row">
        <button class="btn" data-ask="${a.ask}" data-answer="once">Once</button>
        <button class="btn" data-ask="${a.ask}" data-answer="always">Always</button>
        <button class="btn" data-ask="${a.ask}" data-answer="deny">Deny</button>
      </div>
    </div>`).join('');
  $('scripts-ran').innerHTML = ran.map((r) => `
    <p class="script-ran">${esc(r.name)}${r.error ? ` <span class="problem">${esc(r.error)}</span>` : ''}</p>`).join('')
    || '<p class="panel-intro">No script ran on this page.</p>';
  $('scripts-menus').innerHTML = v.menus.map((m) => `
    <button class="btn wide" data-menu="${esc(m.id)}" data-instance="${esc(m.instance)}" title="${esc(m.name)}">${esc(m.caption)}</button>`).join('');
  $('script-list').innerHTML = v.scripts.map((s) => `
    <li data-script="${esc(s.id)}">
      <div class="script-head">
        <span class="script-name" title="${esc(s.description)}">${esc(s.name)}</span>
        <input type="checkbox" class="sw" data-toggle title="On or off" ${s.enabled ? 'checked' : ''}>
      </div>
      <div class="panel-row">
        <span class="fine">${esc(s.version)}</span>
        <span>
          ${s.can_update ? '<button class="btn" data-update>Update</button>' : ''}
          <button class="btn" data-remove>Remove</button>
        </span>
      </div>
    </li>`).join('') || '<li class="panel-intro">No scripts installed.</li>';
  $('bypass-vip').checked = v.bypass_vip;
  $('bypass-vip').disabled = !v.bypass_vip_key_set;
  $('bypass-key').placeholder = v.bypass_vip_key_set ? 'Key saved. Save empty to clear it' : 'bypass.vip API key (paid, optional)';
  $('extensions-part').hidden = !v.extensions_supported;
  $('extension-list').innerHTML = v.extensions.map((n) => `
    <li><div class="script-head"><span class="script-name">${esc(n)}</span><button class="btn" data-extension="${esc(n)}">Remove</button></div></li>`).join('');
}
const installScript = (args, done) => invoke('script_install', { source: null, url: null, ...args })
  .then((name) => { toast(`Installed ${name}`, null, 'info'); if (done) done(); }).catch(report);
$('script-file').addEventListener('change', async (e) => {
  const file = e.target.files[0];
  e.target.value = '';
  if (file) installScript({ source: await file.text() });
});
$('script-url-go').onclick = () => installScript({ url: $('script-url').value }, () => { $('script-url').value = ''; });
$('script-paste-go').onclick = () => installScript({ source: $('script-paste').value }, () => { $('script-paste').value = ''; });
// The script manager moves into the settings page while it is open, so
// clicks are caught for both places.
document.addEventListener('click', (e) => {
  if (!e.target.closest('#panel, #settings')) return;
  const ask = e.target.closest('[data-ask]');
  if (ask) { invoke('script_answer', { ask: Number(ask.dataset.ask), answer: ask.dataset.answer }).then(() => invoke('scripts_view')).then(renderScripts).catch(report); return; }
  const menu = e.target.closest('[data-menu]');
  if (menu) { invoke('script_menu_run', { tab: state.scripts.tab, instance: menu.dataset.instance, id: menu.dataset.menu }).catch(report); return; }
  const gone = e.target.closest('[data-extension]');
  if (gone) { invoke('extension_remove', { name: gone.dataset.extension }).catch(report); return; }
  const row = e.target.closest('[data-script]');
  if (!row) return;
  const id = row.dataset.script;
  if (e.target.closest('[data-toggle]')) invoke('script_toggle', { id, on: e.target.checked }).catch(report);
  else if (e.target.closest('[data-remove]')) invoke('script_remove', { id }).catch(report);
  else if (e.target.closest('[data-update]')) {
    invoke('script_update', { id }).then((v) => toast(v ? `Updated to ${v}` : 'Already up to date', null, 'info')).catch(report);
  }
});
$('extension-go').onclick = () => invoke('extension_add', { path: $('extension-path').value })
  .then((n) => { $('extension-path').value = ''; toast(`Added ${n}. Restart BM Osprey to load it.`, null, 'info'); }).catch(report);
$('bypass-vip').addEventListener('change', (e) => setPrefs({ bypass_vip: e.target.checked }));
$('bypass-key-go').onclick = () => invoke('bypass_key_set', { key: $('bypass-key').value })
  .then(() => { $('bypass-key').value = ''; }).catch(report);
$('link-vip-open').onclick = () => invoke('link_open_vip', { tab: state.active, url: $('link-vip-open').dataset.link }).catch(report);
$('link-go').onclick = async () => {
  const out = $('link-result');
  const open = $('link-vip-open');
  out.textContent = 'Resolving...';
  open.hidden = true;
  try {
    const r = await invoke('link_resolve', { tab: state.active, url: $('link-input').value.trim() });
    const by = r.resolved_by === 'bypass_vip' ? 'bypass.vip' : 'local rule';
    out.textContent = `${r.final_url}\nResolved by: ${by}${r.note ? `\n${r.note}` : ''}`;
    open.dataset.link = r.final_url;
    open.hidden = !r.open_vip;
  } catch (err) { out.textContent = String(err); }
};

// ---------- settings page ----------
// Opens over the page. The network choices and the script manager are the
// same controls as in the side panel: they move into the page while it is
// open and back when it closes, so there is one copy of each.
function applyAppearance(v) {
  const root = document.documentElement;
  if (v.theme && v.theme !== 'sea') root.dataset.theme = v.theme; else delete root.dataset.theme;
  root.classList.toggle('calm', !!v.reduce_effects);
}

function showSection(name) {
  document.querySelectorAll('#settings-nav button').forEach((b) => b.classList.toggle('on', b.dataset.sec === name));
  document.querySelectorAll('#settings-pane .sec').forEach((s) => { s.hidden = s.dataset.sec !== name; });
  $('settings-pane').scrollTop = 0;
  if (name === 'about') loadTools();
  if (name === 'scripts') invoke('scripts_view').then(renderScripts).catch(report);
  if (name === 'privacy') invoke('net_status').then(renderNet).catch(report);
}

function toggleSettings(open = !state.settingsOpen) {
  if (open === state.settingsOpen) return;
  state.settingsOpen = open;
  if (open) {
    stopPreview();
    $('away-net').append($('net-manage'));
    $('away-scripts').append($('scripts-manage'));
    invoke('settings_view').then(renderSettings).catch(report);
    invoke('net_status').then(renderNet).catch(report);
    invoke('scripts_view').then(renderScripts).catch(report);
  } else {
    $('home-net').append($('net-manage'));
    $('home-scripts').append($('scripts-manage'));
  }
  $('settings').hidden = !open;
  invoke('layout', { panelOpen: !!state.panel, covered: open }).catch(report);
  renderToolbar();
}
$('settings-nav').addEventListener('click', (e) => {
  const b = e.target.closest('button[data-sec]');
  if (b) showSection(b.dataset.sec);
});

function dateOf(secs) {
  if (!secs) return '';
  try { return new Date(secs * 1000).toLocaleDateString(); } catch { return ''; }
}
const keep = (el, value) => { if (document.activeElement !== el) el.value = value; };

function renderSettings(v) {
  if (!v) return;
  state.settings = v;
  applyAppearance(v);
  keep($('set-dir'), v.downloads_dir);
  $('dl-where').textContent = 'Saved to ' + v.downloads_dir;
  keep($('set-parallel'), String(v.parallel_downloads));
  keep($('set-sleep'), String(v.sleep_after_minutes));
  if (!$('set-sleep').value) $('set-sleep').value = '20';
  document.querySelectorAll('[data-engine]').forEach((el) => { el.hidden = !v.engine_settings; });
  $('set-gpu').checked = v.hardware_acceleration;
  keep($('set-cache'), String(v.cache_mb));
  $('cache-used').textContent = `Pages and pictures kept on disk to load faster. Uses ${bytes(v.cache_used)} now. A new size needs a restart.`;
  $('restart-note').hidden = !v.restart_needed;
  $('filters-ads').checked = v.block_ads;
  $('lists').innerHTML = v.lists.map((l) => `
    <div class="opt"><div><strong>${esc(l.name)}</strong><small>${l.have ? 'Saved ' + esc(dateOf(l.saved)) : 'Not downloaded yet. Update to get it.'}</small></div>
      <input type="checkbox" class="sw" data-list="${esc(l.id)}" ${l.on ? 'checked' : ''} title="On or off"></div>`).join('');
  if (document.activeElement !== $('rules') && !$('rules').dataset.dirty) $('rules').value = v.custom_rules;
  document.querySelectorAll('.sw-theme').forEach((b) => b.classList.toggle('on', b.dataset.theme === v.theme));
  $('set-calm').checked = v.reduce_effects;
  $('about-version').textContent = v.version;
}

const change = (c) => invoke('settings_set', { change: c }).then(renderSettings).catch(report);
$('set-dir-go').onclick = () => change({ downloads_dir: $('set-dir').value }).then(() => toast('Download folder saved', null, 'info'));
$('set-dir').addEventListener('keydown', (e) => { if (e.key === 'Enter') $('set-dir-go').click(); });
$('set-dir-open').onclick = () => invoke('download_open_folder').catch(report);
$('set-parallel').addEventListener('change', (e) => change({ parallel_downloads: Number(e.target.value) }));
$('set-sleep').addEventListener('change', (e) => change({ sleep_after_minutes: Number(e.target.value) }));
$('set-gpu').addEventListener('change', (e) => change({ hardware_acceleration: e.target.checked }));
$('set-cache').addEventListener('change', (e) => change({ cache_mb: Number(e.target.value) }));
$('cache-clear').onclick = (e) => {
  const b = e.currentTarget;
  b.disabled = true;
  invoke('cache_clear').then((v) => { renderSettings(v); toast('Cache cleared', null, 'info'); }).catch(report).finally(() => { b.disabled = false; });
};
$('lists').addEventListener('change', (e) => {
  const id = e.target.dataset.list;
  if (id) change({ list: [id, e.target.checked] });
});
$('lists-update').onclick = (e) => {
  const b = e.currentTarget;
  b.disabled = true;
  $('lists-result').textContent = 'Updating through your network route';
  invoke('filters_update').then((r) => {
    const ok = r.filter((x) => x.ok).length;
    const bad = r.filter((x) => !x.ok);
    $('lists-result').textContent = `${ok} of ${r.length} lists updated.` + (bad.length ? ' Failed: ' + bad.map((x) => x.name).join(', ') : '');
  }).catch((err) => { $('lists-result').textContent = String(err); }).finally(() => { b.disabled = false; });
};
$('rules').addEventListener('input', (e) => { e.target.dataset.dirty = '1'; });
$('rules-save').onclick = () => invoke('filters_rules_set', { text: $('rules').value }).then((v) => {
  delete $('rules').dataset.dirty;
  renderSettings(v);
  $('rules-result').textContent = 'Saved. The rules apply to new requests.';
}).catch(report);
$('themes').addEventListener('click', (e) => {
  const b = e.target.closest('[data-theme]');
  if (b) change({ theme: b.dataset.theme });
});
$('set-calm').addEventListener('change', (e) => change({ reduce_effects: e.target.checked }));

function renderTools(v) {
  $('tool-ytdlp').textContent = v.ytdlp ? 'Version ' + v.ytdlp : 'Not installed yet. BM Osprey downloads it on its own.';
  $('tool-deno').textContent = v.deno ? v.deno : 'Not installed yet. yt-dlp needs it for some sites.';
  if (v.report.length) $('tool-report').textContent = v.report.join('. ');
}
function loadTools() { invoke('tools_view').then(renderTools).catch(report); }
$('tools-update').onclick = (e) => {
  const b = e.currentTarget;
  b.disabled = true;
  $('tool-report').textContent = 'Checking for new versions';
  invoke('tools_update').then(renderTools).catch((err) => { $('tool-report').textContent = String(err); }).finally(() => { b.disabled = false; });
};
$('export-go').onclick = () => invoke('settings_export', { withKey: $('export-key').checked })
  .then((path) => { $('export-result').textContent = 'Saved as ' + path; }).catch(report);
$('import-file').addEventListener('change', async (e) => {
  const file = e.target.files[0];
  e.target.value = '';
  if (!file) return;
  try {
    const v = await invoke('settings_import', { text: await file.text() });
    renderSettings(v);
    renderNet(await invoke('net_status'));
    $('export-result').textContent = 'Imported ' + file.name;
  } catch (err) { report(err); }
});

// ---------- start ----------
listen('tabs', (v) => {
  if (v.active !== state.active) { stopPreview(); if (state.settingsOpen) toggleSettings(false); }
  state.tabs = v.tabs; state.active = v.active; renderTabs(); renderMedia();
});
listen('media', (v) => { state.media[v.tab] = v.items; state.pageThumb[v.tab] = v.thumb; if (v.tab === state.active) renderMedia(); else renderToolbar(); });
listen('downloads', (list) => {
  const before = state.downloads.length;
  state.downloads = list;
  renderDownloads();
  if (list.length > before && state.panel !== 'downloads') togglePanel('downloads');
});
listen('net', renderNet);
listen('settings', renderSettings);
listen('scripts', renderScripts);
listen('script-ask', () => {
  if (state.panel !== 'scripts') togglePanel('scripts');
  else invoke('scripts_view').then(renderScripts).catch(report);
});
listen('focus-address', () => $('address').focus());
listen('popup-blocked', (p) => {
  let host = p.url;
  try { host = new URL(p.url).host; } catch {}
  toast(`Pop-up blocked: ${host}`, { label: 'Open', run: () => invoke('tab_new', { url: p.url, private: !!activeTab()?.private }).catch(report) }, 'info');
});

// ---------- own title bar ----------
const win = (cmd, extra = {}) => invoke('plugin:window|' + cmd, { label: 'main', ...extra });
async function syncMaxButton() {
  try {
    const max = await win('is_maximized');
    $('win-max').innerHTML = `<svg><use href="#i-${max ? 'restore' : 'max'}"/></svg>`;
    $('win-max').title = max ? 'Restore' : 'Maximise';
    $('edges').hidden = max || !document.body.classList.contains('custom-frame');
  } catch (e) { /* not available */ }
}
function setupFrame(on) {
  document.body.classList.toggle('custom-frame', on);
  $('win-btns').hidden = !on;
  $('edges').hidden = !on;
  if (!on) return;
  $('win-min').onclick = () => win('minimize').catch(report);
  $('win-max').onclick = () => win('toggle_maximize').then(syncMaxButton).catch(report);
  $('win-close').onclick = () => win('close').catch(report);
  $('edges').addEventListener('mousedown', (e) => {
    const dir = e.target.dataset.dir;
    if (dir && e.button === 0) { e.preventDefault(); win('start_resize_dragging', { value: dir }).catch(report); }
  });
  let t = 0;
  window.addEventListener('resize', () => { clearTimeout(t); t = setTimeout(syncMaxButton, 120); });
  syncMaxButton();
}

invoke('ui_ready').then((s) => {
  state.tabs = s.tabs.tabs;
  state.active = s.tabs.active;
  state.downloads = s.downloads;
  document.body.dataset.platform = s.platform;
  setupFrame(!!s.custom_frame);
  renderTabs();
  renderMedia();
  renderDownloads();
  renderNet(s.net);
  renderSettings(s.settings);
}).catch(report);
