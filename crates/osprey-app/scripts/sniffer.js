// BM Osprey media sniffer. Runs in every page and frame before the page's
// own scripts.
//
// It is passive on purpose. It does not wrap fetch, XMLHttpRequest or
// any other page function, because wrapped functions can be detected by
// sites and add work to every request. It reads the browser's own list of
// loaded resources (PerformanceObserver) and media element events.
//
// It only collects candidates. The app then asks the server what each
// one really is (like 1DM does) and drops everything that is not media.
//
// On Windows and Linux the web engine reports requests itself (see
// capture.rs), so this is the backup there, and it still adds the picture
// size and length of each video. On macOS it is the only source.
//
// Video players usually sit in a frame from another site. Frames cannot
// talk to the app directly on every platform, so a frame hands its finds
// to the top page, which passes them on. __OSPREY_NONCE__ is replaced at
// start up, so stray messages from pages are ignored.
(function () {
  'use strict';
  var NONCE = '__OSPREY_NONCE__';
  var TOP = window === window.top;
  var T = window.__TAURI_INTERNALS__;
  var invoke = T && typeof T.invoke === 'function' ? T.invoke.bind(T) : null;
  if (TOP && !invoke) return;
  var setTimer = window.setTimeout.bind(window);

  // Keep Tauri's helpers out of property listings that fingerprinting
  // scripts use. They still exist, so this hides them from casual
  // enumeration, not from a page that looks for them by name.
  ['__TAURI_INTERNALS__', '__TAURI_EVENT_PLUGIN_INTERNALS__', 'ipc'].forEach(function (k) {
    try {
      var d = Object.getOwnPropertyDescriptor(window, k);
      if (d && d.configurable) {
        d.enumerable = false;
        Object.defineProperty(window, k, d);
      }
    } catch (e) {}
  });

  // Media by extension.
  var MEDIA = /\.(m3u8|mpd|mp4|m4v|webm|mkv|mov|flv|mp3|m4a|aac|ogg|oga|opus|flac|wav)(?:[?#]|$)/i;
  // Playlists without an extension, as many players use.
  var MAYBE = /m3u8|\.mpd|manifest|playlist|master|\/hls\/|\/dash\/|videoplayback|\/stream\b|\/source\b/i;
  // Pieces of a stream (the playlist is reported instead) and things that
  // are never media.
  var PIECE = /\.(ts|m4s|cmfv|cmfa|aac\.ts)(?:[?#]|$)|[\/_-](seg|segment|chunk|frag)[-_]?\d+\.(mp4|m4a|m4v|aac)(?:[?#]|$)/i;
  var NEVER = /\.(js|mjs|css|png|jpe?g|gif|webp|avif|svg|ico|woff2?|ttf)(?:[?#]|$)/i;
  var LIMIT = 300;

  var seen = Object.create(null);
  var count = 0;
  var queue = [];
  var opened = [];
  var timer = 0;
  var hints = null;

  function brands() {
    if (hints) return hints;
    hints = { platform: '', mobile: false, brands: [] };
    try {
      var d = navigator.userAgentData;
      if (d) {
        hints.platform = d.platform || '';
        hints.mobile = !!d.mobile;
        hints.brands = (d.brands || []).map(function (b) { return { brand: b.brand, version: b.version }; });
      }
    } catch (e) {}
    return hints;
  }

  // The page's own preview picture, shown on its Media panel card.
  function pageImage() {
    try {
      var m = document.querySelector('meta[property="og:image"], meta[name="og:image"], meta[name="twitter:image"], meta[property="twitter:image"]');
      var c = m && m.getAttribute('content');
      return c ? new URL(c, location.href).href : null;
    } catch (e) { return null; }
  }

  function send(items, opens) {
    if (TOP) {
      var h = brands();
      invoke('media_found', {
        report: {
          page: location.href,
          ua: navigator.userAgent,
          languages: (navigator.languages || []).slice(0, 4),
          platform: h.platform,
          mobile: h.mobile,
          brands: h.brands,
          items: items,
          opened: opens,
          thumb: pageImage()
        }
      }).catch(function () {});
    } else {
      try { window.top.postMessage({ osprey: NONCE, items: items, opened: opens }, '*'); } catch (e) {}
    }
  }

  function flush() {
    timer = 0;
    if (!queue.length && !opened.length) return;
    send(queue.splice(0, 40), opened.splice(0, 10));
    if (queue.length) timer = setTimer(flush, 700);
  }

  function later(delay) { if (!timer) timer = setTimer(flush, delay); }

  function report(raw, how, extra) {
    if (!raw || count >= LIMIT) return;
    var url;
    try { url = new URL(raw, location.href).href; } catch (e) { return; }
    if (url.slice(0, 4) !== 'http' || PIECE.test(url) || NEVER.test(url)) return;
    var key = url + (extra ? '|' + extra.h : '');
    if (seen[key]) return;
    seen[key] = 1;
    count++;
    var item = { url: url, hint: how, frame: TOP ? null : location.href };
    if (extra) { item.w = extra.w; item.h = extra.h; item.dur = extra.dur; }
    queue.push(item);
    later(700);
  }

  // The top page passes on what its frames found.
  if (TOP) {
    window.addEventListener('message', function (e) {
      var d = e.data;
      if (!d || d.osprey !== NONCE) return;
      var items = Array.isArray(d.items) ? d.items.slice(0, 40) : [];
      var opens = Array.isArray(d.opened) ? d.opened.slice(0, 10) : [];
      if (items.length || opens.length) send(items, opens);
    }, true);
  }

  // 1. Everything the page downloads: players using fetch, XHR or Media
  //    Source Extensions all show up here, including HLS and DASH.
  try {
    new PerformanceObserver(function (list) {
      var entries = list.getEntries();
      for (var i = 0; i < entries.length; i++) {
        var n = entries[i].name;
        var kind = entries[i].initiatorType || '';
        if (MEDIA.test(n)) report(n, kind || 'resource');
        else if (kind === 'video' || kind === 'audio') report(n, kind);
        else if ((kind === 'xmlhttprequest' || kind === 'fetch' || kind === 'other') && MAYBE.test(n)) report(n, kind);
      }
    }).observe({ type: 'resource', buffered: true });
  } catch (e) {}

  // 2. Media elements, caught at the document level in the capture phase,
  //    so no element needs its own listener. Their picture size and length
  //    help name and sort what was found.
  function fromElement(el) {
    if (!el || !el.tagName) return;
    var tag = el.tagName.toLowerCase();
    if (tag !== 'video' && tag !== 'audio') return;
    var extra = { w: el.videoWidth || 0, h: el.videoHeight || 0, dur: isFinite(el.duration) ? el.duration : 0 };
    var src = el.currentSrc || el.src;
    if (src && src.slice(0, 5) !== 'blob:') report(src, tag, extra);
    var sources = el.querySelectorAll('source[src]');
    for (var i = 0; i < sources.length; i++) report(sources[i].src, 'source');
  }
  ['loadedmetadata', 'play'].forEach(function (type) {
    document.addEventListener(type, function (e) { fromElement(e.target); }, true);
  });

  // 3. Links you really click that open a new tab. The pop-up blocker
  //    lets those through and blocks pop-ups a page opens by itself.
  function onClick(e) {
    if (!e.isTrusted) return;
    var a = e.target && e.target.closest ? e.target.closest('a[href]') : null;
    if (!a) return;
    var newTab = a.target === '_blank' || e.button === 1 || e.ctrlKey || e.metaKey || e.shiftKey;
    if (!newTab) return;
    try { opened.push(new URL(a.getAttribute('href'), location.href).href); } catch (err) {}
    flush();
  }
  window.addEventListener('click', onClick, true);
  window.addEventListener('auxclick', onClick, true);

  // 4. Browser shortcuts while the page has focus. Only real key presses
  //    count, so a page cannot fake them.
  if (TOP) {
    window.addEventListener('keydown', function (e) {
      if (!e.isTrusted) return;
      var mod = e.ctrlKey || e.metaKey;
      var key = (e.key || '').toLowerCase();
      var action = '';
      if (mod && e.shiftKey && key === 'n') action = 'new_private_tab';
      else if (mod && !e.shiftKey && key === 't') action = 'new_tab';
      else if (mod && key === 'w') action = 'close_tab';
      else if (mod && key === 'l') action = 'focus_address';
      else if (mod && key === 'tab') action = e.shiftKey ? 'prev_tab' : 'next_tab';
      if (action) {
        e.preventDefault();
        invoke('tab_shortcut', { action: action }).catch(function () {});
      }
    }, true);
  }
})();
