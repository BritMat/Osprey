// ==UserScript==
// @name         BM Osprey GM test
// @name:ja      BM Osprey GM テスト
// @namespace    bm-osprey-tests
// @version      1.0
// @license      MIT
// @description  Works every GM function on samples/test/gm-test.html and saves what happened with GM_setValue, so a probe report shows it.
// @match        http://127.0.0.1/*
// @exclude      http://127.0.0.1/never/*
// @require      http://127.0.0.1:8099/test/lib.js
// @connect      127.0.0.1
// @run-at       document-end
// @noframes
// @grant        GM_xmlhttpRequest
// @grant        GM.xmlHttpRequest
// @grant        GM_download
// @grant        GM_setValue
// @grant        GM_getValue
// @grant        GM_deleteValue
// @grant        GM_listValues
// @grant        GM_registerMenuCommand
// @grant        GM_addStyle
// @grant        GM_setClipboard
// @grant        GM_openInTab
// ==/UserScript==

(async function () {
  'use strict';
  // The tab GM_openInTab opens. It only says it arrived, or it would open
  // another one in turn.
  if (location.search.includes('opened=1')) {
    GM_setValue('opened_tab_ran', true);
    return;
  }
  const r = {};
  const save = () => GM_setValue('result', r);
  const step = async (name, f) => {
    try {
      r[name] = await f();
    } catch (e) {
      r[name] = 'THREW ' + e;
    }
    save();
  };
  const xhr = (d) => new Promise((ok) => {
    GM_xmlhttpRequest({
      ...d,
      onload: (x) => ok({ on: 'load', x }),
      onerror: (x) => ok({ on: 'error', x }),
      ontimeout: (x) => ok({ on: 'timeout', x }),
      onabort: (x) => ok({ on: 'abort', x }),
    });
  });
  const base = location.origin + '/test/';

  await step('previous_run', () => GM_getValue('runs', 0));
  GM_setValue('runs', GM_getValue('runs', 0) + 1);

  await step('sandbox', () => ({
    lib_seen_by_script: typeof ospreyTestLib,
    page_cannot_see_lib: typeof unsafeWindow.ospreyTestLib,
    script_global_private: (window.scriptOnly = 1, unsafeWindow.scriptOnly === undefined),
    page_global_readable: unsafeWindow.pageValue,
    window_differs: window !== unsafeWindow,
    timer_works: typeof setTimeout(() => {}, 0),
    info: GM_info.script.name,
    handler: GM_info.scriptHandler,
  }));

  await step('values', () => {
    GM_setValue('temp', { a: [1, 2] });
    const got = GM_getValue('temp');
    const listed = GM_listValues().includes('temp');
    GM_deleteValue('temp');
    return { got, listed, after_delete: GM_getValue('temp', 'gone') };
  });

  await step('style', () => {
    const el = document.createElement('div');
    el.className = 'osprey-styled';
    document.body.appendChild(el);
    GM_addStyle('.osprey-styled { width: 37px; }');
    return getComputedStyle(el).width;
  });

  await step('clipboard', () => {
    GM_setClipboard('bm fetch test');
    return 'called';
  });

  await step('xhr_text', async () => {
    const { on, x } = await xhr({ url: base + 'hello.txt' });
    return { on, status: x.status, text: x.responseText, final: x.finalUrl, headers: /content-type/i.test(x.responseHeaders) };
  });
  await step('xhr_json', async () => {
    const { x } = await xhr({ url: base + 'data.json', responseType: 'json' });
    return x.response;
  });
  await step('xhr_blob', async () => {
    const { x } = await xhr({ url: base + 'lib.js', responseType: 'blob' });
    return { blob: x.response instanceof Blob, size: x.response.size };
  });
  await step('xhr_big', async () => {
    const { x } = await xhr({ url: base + 'big.bin', responseType: 'arraybuffer' });
    return x.response.byteLength;
  });
  await step('xhr_promise', async () => {
    const x = await GM.xmlHttpRequest({ url: base + 'hello.txt' });
    return x.status;
  });
  await step('xhr_404', async () => {
    const { on, x } = await xhr({ url: base + 'missing.txt' });
    return { on, status: x.status };
  });
  await step('xhr_timeout', async () => {
    const { on } = await xhr({ url: location.origin + '/slow', timeout: 300 });
    return on;
  });
  await step('xhr_abort', () => new Promise((ok) => {
    const h = GM_xmlhttpRequest({ url: location.origin + '/slow', onabort: () => ok('abort'), onload: () => ok('load'), onerror: (e) => ok('error ' + e.error) });
    setTimeout(() => h.abort(), 100);
  }));
  // localhost is not in @connect. The probe denies the first question and
  // allows the second, and the third must not ask at all.
  const other = 'http://localhost:' + location.port + '/test/hello.txt';
  await step('connect_denied', async () => {
    const { on, x } = await xhr({ url: other });
    return { on, why: x.error };
  });
  await step('connect_allowed', async () => (await xhr({ url: other })).x.status);
  await step('connect_remembered', async () => (await xhr({ url: other })).x.status);

  await step('gm_download', () => new Promise((ok) => {
    GM_download({ url: base + 'hello.txt', name: 'gm-download.txt', onload: () => ok('load'), onerror: (e) => ok('error ' + JSON.stringify(e)) });
  }));
  await step('gm_download_blob', () => new Promise((ok) => {
    const url = URL.createObjectURL(new Blob(['from a blob']));
    GM_download({ url, name: 'gm-blob.txt', onload: () => ok('load'), onerror: (e) => ok('error ' + JSON.stringify(e)) });
  }));
  await step('anchor_blob', () => {
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob(['from an anchor']));
    a.download = 'anchor-blob.txt';
    a.click();
    URL.revokeObjectURL(a.href);
    return 'clicked';
  });
  await step('save_picker', async () => {
    const handle = await unsafeWindow.showSaveFilePicker({ suggestedName: 'picker.txt' });
    const w = await handle.createWritable();
    await w.write('hello ');
    await w.write(new Blob(['from the picker']));
    await w.close();
    return handle.name;
  });

  GM_registerMenuCommand('Test menu command', () => {
    r.menu_clicked = true;
    save();
  });
  GM_openInTab(base + 'gm-test.html?opened=1', { active: false });
  r.finished = true;
  save();
})();
