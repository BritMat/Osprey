// BM Osprey user script runner. Runs in every frame of a normal tab, before
// the page's own scripts, and only when at least one script is switched on.
// Each installed script follows as its own start up script and registers
// itself with add(), so a syntax error in one cannot stop the others.
//
// A script that uses GM_ functions gets a sandbox: a window of its own that
// falls through to the real one. What it stores there, such as jQuery's $,
// stays away from the page. A script with no GM_ functions runs in the page.
//
// The real checks live in Rust. A page can watch what passes through here,
// so this file holds no power of its own: every GM_ call is checked again
// against the script's grants, its @connect list and its tab.
(function () {
  'use strict';
  var KEY = '__OSPREY_KEY__';
  var PROBE = __OSPREY_PROBE__;
  var W = window;
  var TOP = W === W.top;
  var T = W.__TAURI_INTERNALS__;
  var ipc = T && typeof T.invoke === 'function' ? T.invoke.bind(T) : null;

  // The page may replace these later, so they are kept from before it runs.
  var Url = W.URL;
  var createUrl = Url.createObjectURL;
  var revokeUrl = Url.revokeObjectURL;
  var Blob_ = W.Blob;
  var setTimer = W.setTimeout.bind(W);
  var fetch_ = W.fetch.bind(W);
  var atob_ = W.atob.bind(W);
  var btoa_ = W.btoa.bind(W);
  var fromCharCode = String.fromCharCode;
  var parseJson = JSON.parse;

  var seen = [];
  var firstToken = null;
  var hooked = false;
  var blobs = new Map();
  var menuId = 0;
  var instance = Math.random().toString(36).slice(2);

  function call(token, op, args) {
    if (!ipc) return Promise.reject(new Error('GM functions do not work in this frame yet'));
    return ipc('gm_call', { token: token, op: op, args: args || {} });
  }
  function quiet(token, op, args) {
    call(token, op, args).catch(function () {});
  }

  function toBase64(bytes) {
    var text = '';
    for (var i = 0; i < bytes.length; i += 0x8000) {
      text += fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    }
    return btoa_(text);
  }
  function fromBase64(text) {
    var raw = atob_(text);
    var bytes = new Uint8Array(raw.length);
    for (var i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
    return bytes;
  }

  // ---------- saving what a page or script made itself ----------
  var CHUNK = 4 * 1024 * 1024;

  function toBlob(data) {
    if (data instanceof Blob_) return data;
    return new Blob_([data]);
  }

  // Sends a Blob to the download manager, piece by piece.
  function saveBlob(token, blob, name) {
    return call(token, 'save_begin', { name: name || 'download', size: blob.size }).then(function (id) {
      var at = 0;
      function next() {
        if (at >= blob.size) return call(token, 'save_end', { id: id });
        var piece = blob.slice(at, at + CHUNK);
        return piece.arrayBuffer().then(function (buffer) {
          var data = toBase64(new Uint8Array(buffer));
          var position = at;
          at += piece.size;
          return call(token, 'save_write', { id: id, position: position, data: data }).then(next);
        });
      }
      return next().catch(function (e) {
        quiet(token, 'save_abort', { id: id });
        throw e;
      });
    });
  }

  function blobOf(url) {
    var known = blobs.get(url);
    if (known) return Promise.resolve(known);
    return fetch_(url).then(function (r) { return r.blob(); });
  }

  function isLocalData(url) {
    return /^(blob|data):/i.test(String(url));
  }

  // The stand-in for showSaveFilePicker. The file goes to the download
  // manager instead of a place the user picks.
  function fakeFileHandle(token, name) {
    return {
      kind: 'file',
      name: name,
      getFile: function () { return Promise.reject(new Error('this file has not been saved yet')); },
      createWritable: function () {
        return call(token, 'save_begin', { name: name, size: 0 }).then(function (id) {
          var position = 0;
          var queue = Promise.resolve();
          function writeNow(data, at) {
            var blob = toBlob(data);
            var offset = 0;
            function next() {
              if (offset >= blob.size) return undefined;
              var piece = blob.slice(offset, offset + CHUNK);
              return piece.arrayBuffer().then(function (buffer) {
                var where = at + offset;
                offset += piece.size;
                return call(token, 'save_write', {
                  id: id,
                  position: where,
                  data: toBase64(new Uint8Array(buffer)),
                }).then(next);
              });
            }
            return Promise.resolve(next()).then(function () {
              position = at + blob.size;
            });
          }
          function enqueue(job) {
            queue = queue.then(job);
            return queue;
          }
          return {
            write: function (chunk) {
              return enqueue(function () {
                if (chunk && chunk.type === 'seek') { position = chunk.position; return undefined; }
                if (chunk && chunk.type === 'truncate') return undefined;
                if (chunk && chunk.type === 'write') {
                  return writeNow(chunk.data, chunk.position === undefined ? position : chunk.position);
                }
                return writeNow(chunk, position);
              });
            },
            seek: function (at) { return enqueue(function () { position = at; }); },
            truncate: function () { return queue; },
            close: function () { return enqueue(function () { return call(token, 'save_end', { id: id }); }); },
            abort: function () { return enqueue(function () { return call(token, 'save_abort', { id: id }); }); },
          };
        });
      },
    };
  }

  // Downloads made in the page have no address the app can fetch, so they
  // are caught here and sent to the download manager.
  function catchDownload(a) {
    if (!a || !a.hasAttribute || !a.hasAttribute('download') || !isLocalData(a.href)) return false;
    var name = a.getAttribute('download') || 'download';
    blobOf(a.href).then(function (blob) {
      return saveBlob(firstToken, blob, name);
    }).catch(function (e) {
      quiet(firstToken, 'problem', { message: 'could not save ' + name + ': ' + e });
    });
    return true;
  }

  function installHooks(token) {
    firstToken = firstToken || token;
    if (hooked) return;
    hooked = true;
    Url.createObjectURL = function (object) {
      var url = createUrl.call(Url, object);
      if (object instanceof Blob_) blobs.set(url, object);
      return url;
    };
    Url.revokeObjectURL = function (url) {
      // A script often revokes right after click(), before the save starts.
      setTimer(function () { blobs.delete(url); }, 60000);
      return revokeUrl.call(Url, url);
    };
    var proto = W.HTMLAnchorElement.prototype;
    var click = proto.click;
    proto.click = function () {
      if (catchDownload(this)) return undefined;
      return click.apply(this, arguments);
    };
    var dispatch = W.EventTarget.prototype.dispatchEvent;
    W.EventTarget.prototype.dispatchEvent = function (event) {
      if (event && event.type === 'click' && this instanceof W.HTMLAnchorElement && catchDownload(this)) {
        return false;
      }
      return dispatch.call(this, event);
    };
    W.addEventListener('click', function (e) {
      var a = e.target && e.target.closest ? e.target.closest('a[download]') : null;
      if (a && catchDownload(a)) e.preventDefault();
    }, true);
    W.showSaveFilePicker = function (options) {
      var name = (options && options.suggestedName) || 'download';
      return Promise.resolve(fakeFileHandle(firstToken, name));
    };
  }

  // ---------- GM functions ----------
  function addStyle(css) {
    try {
      var sheet = new W.CSSStyleSheet();
      sheet.replaceSync(css);
      document.adoptedStyleSheets = document.adoptedStyleSheets.concat([sheet]);
      return {
        remove: function () {
          document.adoptedStyleSheets = document.adoptedStyleSheets.filter(function (s) {
            return s !== sheet;
          });
        },
      };
    } catch (e) {
      var style = document.createElement('style');
      style.textContent = css;
      (document.head || document.documentElement).appendChild(style);
      return style;
    }
  }

  function setClipboard(text) {
    var value = String(text);
    var done = false;
    if (document.body) {
      var box = document.createElement('textarea');
      box.value = value;
      box.style.position = 'fixed';
      box.style.opacity = '0';
      document.body.appendChild(box);
      box.select();
      try { done = document.execCommand('copy'); } catch (e) {}
      box.remove();
    }
    if (!done && W.navigator.clipboard) {
      W.navigator.clipboard.writeText(value).catch(function () {});
    }
  }

  function headerLines(list) {
    return list.map(function (h) { return h[0] + ': ' + h[1]; }).join('\r\n') + '\r\n';
  }

  function charsetOf(type, override) {
    var m = /charset=([^;\s]+)/i.exec(override || type || '');
    return m ? m[1].replace(/"/g, '') : 'utf-8';
  }

  function decodeText(bytes, type, override) {
    try {
      return new W.TextDecoder(charsetOf(type, override)).decode(bytes);
    } catch (e) {
      return new W.TextDecoder().decode(bytes);
    }
  }

  // Encodes what a script wants to send: text, bytes, a Blob or a form.
  function encodeBody(data) {
    if (data === undefined || data === null) return Promise.resolve(null);
    if (typeof data === 'string') {
      return Promise.resolve({ text: data });
    }
    if (W.FormData && data instanceof W.FormData || W.URLSearchParams && data instanceof W.URLSearchParams) {
      var r = new W.Response(data);
      var type = r.headers.get('content-type');
      return r.arrayBuffer().then(function (b) {
        return { data: toBase64(new Uint8Array(b)), type: type };
      });
    }
    return toBlob(data).arrayBuffer().then(function (b) {
      return { data: toBase64(new Uint8Array(b)) };
    });
  }

  function readBody(token, id, size) {
    var bytes = new Uint8Array(size);
    var at = 0;
    function next() {
      if (at >= size) return Promise.resolve(bytes);
      return call(token, 'xhr_body', { id: id, offset: at }).then(function (text) {
        var piece = fromBase64(text);
        if (!piece.length) throw new Error('the response ended early');
        bytes.set(piece, at);
        at += piece.length;
        return next();
      });
    }
    return next();
  }

  function shapeResponse(d, head, bytes) {
    var type = d.responseType || '';
    var contentType = head.content_type || '';
    var mime = d.overrideMimeType || contentType;
    var text = null;
    var readText = function () {
      if (text === null) text = decodeText(bytes, contentType, d.overrideMimeType);
      return text;
    };
    var out = {
      readyState: 4,
      status: head.status,
      statusText: head.status_text,
      finalUrl: head.final_url,
      responseHeaders: headerLines(head.headers),
      context: d.context,
    };
    Object.defineProperty(out, 'responseText', { enumerable: true, get: readText });
    var value;
    if (type === 'blob') value = new Blob_([bytes], { type: mime });
    else if (type === 'arraybuffer') value = bytes.buffer;
    else if (type === 'json') {
      try { value = parseJson(readText()); } catch (e) { value = undefined; }
    } else if (type === 'document') {
      value = new W.DOMParser().parseFromString(readText(), /xml/.test(mime) ? 'text/xml' : 'text/html');
    } else value = readText();
    out.response = value;
    Object.defineProperty(out, 'responseXML', {
      enumerable: true,
      get: function () {
        return new W.DOMParser().parseFromString(readText(), /html/.test(mime) ? 'text/html' : 'text/xml');
      },
    });
    return out;
  }

  function fire(d, name, value) {
    var f = d['on' + name];
    if (typeof f !== 'function') return;
    try { f.call(d, value); } catch (e) { W.console.error(e); }
  }

  function xhr(token, d) {
    var aborted = false;
    var rid = Math.random().toString(36).slice(2);
    var failed = function (kind, message) {
      var value = {
        readyState: 4, status: 0, statusText: '', responseText: '', response: null,
        responseHeaders: '', finalUrl: d.url, error: message, context: d.context,
      };
      fire(d, kind, value);
      fire(d, 'loadend', value);
      return value;
    };
    var done = encodeBody(d.data).then(function (body) {
      if (aborted) throw 'abort';
      fire(d, 'loadstart', { readyState: 1, context: d.context });
      var url = String(d.url);
      try { url = new Url(url, W.location.href).href; } catch (e) {}
      return call(token, 'xhr', {
        rid: rid,
        url: url,
        method: d.method || 'GET',
        headers: d.headers || {},
        body: body,
        timeout: d.timeout || 0,
        anonymous: !!d.anonymous,
      });
    }).then(function (head) {
      return readBody(token, head.id, head.size).then(function (bytes) {
        var value = shapeResponse(d, head, bytes);
        fire(d, 'readystatechange', value);
        fire(d, 'progress', {
          lengthComputable: true, loaded: head.size, total: head.size,
          position: head.size, totalSize: head.size, context: d.context,
        });
        fire(d, 'load', value);
        fire(d, 'loadend', value);
        return value;
      });
    }).catch(function (e) {
      var text = String(e && e.message ? e.message : e);
      if (aborted || e === 'abort') {
        failed('abort', 'aborted');
        throw new Error('aborted');
      }
      var kind = text.indexOf('timed out') !== -1 ? 'timeout' : 'error';
      failed(kind, text);
      throw new Error(text);
    });
    return {
      promise: done,
      abort: function () {
        aborted = true;
        quiet(token, 'xhr_abort', { rid: rid });
      },
    };
  }

  function download(token, a, b) {
    var d = typeof a === 'string' ? { url: a, name: b } : a;
    var stopped = false;
    var job = null;
    var finish = function (kind, value) { fire(d, kind, value); };
    var waitFor = function (id) {
      job = id;
      return call(token, 'download_state', { id: id }).then(function (s) {
        if (s.state === 'done') return s;
        if (s.state === 'failed' || s.state === 'cancelled') throw new Error(s.error || s.state);
        fire(d, 'progress', { loaded: s.done, total: s.total, lengthComputable: !!s.total });
        return new Promise(function (ok) { setTimer(ok, 1000); }).then(function () { return waitFor(id); });
      });
    };
    var run = isLocalData(d.url)
      ? blobOf(d.url).then(function (blob) { return saveBlob(token, blob, d.name); })
      : call(token, 'download', { url: new Url(d.url, W.location.href).href, name: d.name, headers: d.headers }).then(waitFor);
    var done = run.then(function () {
      finish('load', {});
    }).catch(function (e) {
      if (!stopped) finish('error', { error: 'unknown', details: String(e) });
      throw e;
    });
    return {
      promise: done,
      abort: function () {
        stopped = true;
        if (job !== null) quiet(token, 'download_cancel', { id: job });
      },
    };
  }

  function waitForMenu(token) {
    call(token, 'menu_wait', { instance: instance }).then(function (id) {
      if (id !== null) {
        var f = menus[id];
        if (f) {
          try { f(); } catch (e) { W.console.error(e); }
        }
      }
      waitForMenu(token);
    }).catch(function () {});
  }
  var menus = Object.create(null);
  var menuWaiting = Object.create(null);

  function registerMenu(token, caption, fn) {
    var id = String(++menuId);
    menus[id] = fn;
    quiet(token, 'menu_add', { instance: instance, id: id, caption: String(caption) });
    if (!menuWaiting[token]) {
      menuWaiting[token] = true;
      waitForMenu(token);
    }
    return id;
  }

  // Builds the functions a script asked for, and nothing else.
  function buildApi(s) {
    var token = s.token;
    var values = s.values || {};
    // One after the other, so the last value set is the one that stays.
    var saving = Promise.resolve();
    function saveValue(args) {
      saving = saving.then(function () {
        return call(token, 'value_set', args);
      }).catch(function () {});
    }
    var has = function (name) { return s.grants.indexOf(name) !== -1; };
    var api = {};
    var gm = {};
    gm.info = s.info;
    api.GM_info = s.info;
    function both(snake, camel, sync, promised) {
      if (!has(snake.toLowerCase())) return;
      api['GM_' + snake] = sync;
      gm[camel || snake] = promised || function () {
        var args = arguments;
        return new Promise(function (ok) { ok(sync.apply(null, args)); });
      };
    }
    both('setValue', null, function (k, v) {
      var stored = v === undefined ? null : parseJson(JSON.stringify(v));
      values[k] = stored;
      saveValue({ key: String(k), value: stored });
    });
    both('getValue', null, function (k, fallback) {
      return Object.prototype.hasOwnProperty.call(values, k) ? values[k] : fallback;
    });
    both('deleteValue', null, function (k) {
      delete values[k];
      saveValue({ key: String(k), value: null, remove: true });
    });
    both('listValues', null, function () { return Object.keys(values); });
    both('addStyle', null, addStyle);
    both('setClipboard', null, setClipboard);
    both('openInTab', null, function (url, options) {
      var active = typeof options === 'object' && options ? !!options.active : !options;
      quiet(token, 'open_tab', { url: new Url(url, W.location.href).href, active: active });
      return { close: function () {}, closed: false };
    });
    both('registerMenuCommand', null, function (caption, fn) { return registerMenu(token, caption, fn); });
    both('unregisterMenuCommand', null, function (id) {
      delete menus[id];
      quiet(token, 'menu_remove', { instance: instance, id: String(id) });
    });
    both('log', null, function () { W.console.log.apply(W.console, arguments); });
    both('xmlhttpRequest', 'xmlHttpRequest', function (d) {
      var r = xhr(token, d);
      r.promise.catch(function () {});
      return { abort: r.abort };
    }, function (d) {
      var r = xhr(token, d);
      var p = r.promise;
      p.abort = r.abort;
      return p;
    });
    both('download', null, function (a, b) {
      var r = download(token, a, b);
      r.promise.catch(function () {});
      return { abort: r.abort };
    }, function (a, b) { return download(token, a, b).promise; });
    api.GM = gm;
    return api;
  }

  // A window of the script's own. Reads fall through to the real one, and
  // browser functions are bound to it, or `setTimeout()` would be called on
  // the wrong object and throw.
  function makeScope(api) {
    var local = Object.create(null);
    var bound = new WeakMap();
    var scope;
    Object.keys(api).forEach(function (k) { local[k] = api[k]; });
    local.unsafeWindow = W;
    // A library looks for these to choose how to export itself. The page's
    // own loader must not catch it.
    local.define = undefined;
    local.module = undefined;
    local.exports = undefined;

    function setter(k) {
      for (var o = W; o; o = Object.getPrototypeOf(o)) {
        var d = Object.getOwnPropertyDescriptor(o, k);
        if (d) return d.set ? d : null;
      }
      return null;
    }
    var handler = {
      has: function (t, k) {
        return k in local || k in W;
      },
      get: function (t, k) {
        if (k === Symbol.unscopables) return undefined;
        if (k === 'window' || k === 'self' || k === 'globalThis') return scope;
        if (k in local) return local[k];
        var v = W[k];
        if (typeof v === 'function' && !Object.prototype.hasOwnProperty.call(v, 'prototype')) {
          var b = bound.get(v);
          if (!b) {
            b = v.bind(W);
            bound.set(v, b);
          }
          return b;
        }
        return v;
      },
      set: function (t, k, v) {
        if (!(k in local) && setter(k)) W[k] = v;
        else local[k] = v;
        return true;
      },
      deleteProperty: function (t, k) {
        delete local[k];
        return true;
      },
      defineProperty: function (t, k, d) {
        Object.defineProperty(local, k, d);
        return true;
      },
      getOwnPropertyDescriptor: function (t, k) {
        var d = Object.getOwnPropertyDescriptor(local, k);
        if (d) d.configurable = true;
        return d;
      },
      ownKeys: function () {
        return Reflect.ownKeys(local);
      },
    };
    scope = new Proxy(Object.create(null), handler);
    return scope;
  }

  function reportRun(s, error) {
    quiet(s.token, 'ran', {
      url: W.location.href.split('#')[0],
      frame: !TOP,
      error: error ? String(error && error.stack ? error.stack : error).slice(0, 600) : null,
    });
  }

  function runNow(s) {
    var api = buildApi(s);
    try {
      if (s.in_page) s.fn(s.info, api.GM, W, undefined, undefined, undefined);
      else {
        var scope = makeScope(api);
        s.fn(scope, scope);
      }
      reportRun(s, null);
    } catch (e) {
      reportRun(s, e);
    }
  }

  function when(runAt, go) {
    if (runAt === 'document-start') return go();
    var after = runAt === 'document-end' ? go : function () { setTimer(go, 0); };
    if (document.readyState === 'loading') {
      document.addEventListener('DOMContentLoaded', after, { once: true });
    } else {
      after();
    }
    return undefined;
  }

  function test(patterns) {
    var address = W.location.href.split('#')[0];
    return patterns.some(function (p) {
      try { return new RegExp(p, 'i').test(address); } catch (e) { return false; }
    });
  }

  function add(s, fn) {
    seen.push(s.token);
    if (s.noframes && !TOP) return;
    if (!test(s.include) || test(s.exclude)) return;
    s.fn = fn;
    installHooks(s.token);
    if (PROBE && TOP) probeErrors(s.token);
    // GM_getValue answers at once, but asking the app takes a moment, so
    // these scripts wait for their values before they start.
    var needsValues = s.grants.some(function (g) { return /value/.test(g); });
    if (!needsValues || !ipc) {
      when(s.run_at, function () { runNow(s); });
      return;
    }
    call(s.token, 'values').then(function (values) {
      s.values = values;
    }).catch(function () {
      s.values = {};
    }).then(function () {
      when(s.run_at, function () { runNow(s); });
    });
  }

  // Probe runs also list errors from later on, not only from the first run
  // of a script. Start up scripts have no file name, page scripts do.
  var probing = false;
  function probeErrors(token) {
    if (probing) return;
    probing = true;
    W.addEventListener('error', function (e) {
      if (!e.filename) quiet(token, 'problem', { message: String(e.message) });
    });
  }

  // The last start up script. A token that never reached add() means the
  // script did not even parse.
  function finish(tokens) {
    delete W[KEY];
    if (!TOP) return;
    tokens.forEach(function (token) {
      if (seen.indexOf(token) === -1) {
        quiet(token, 'ran', { url: W.location.href.split('#')[0], frame: false, error: 'the script has a syntax error' });
      }
    });
  }

  Object.defineProperty(W, KEY, {
    value: { add: add, finish: finish },
    configurable: true,
    enumerable: false,
  });
})();
