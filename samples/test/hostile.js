// Tries everything a hostile page could try and writes the outcome into the
// title, where a probe report shows it.
(() => {
  const T = window.__TAURI_INTERNALS__;
  const out = {};
  const stolen = [];
  const spy = (text) => {
    const m = /"token":"([0-9a-f]{32})"/.exec(String(text));
    if (m && !stolen.includes(m[1])) stolen.push(m[1]);
  };
  // Tauri sends calls by fetch or postMessage, so a page can watch both.
  const realFetch = window.fetch;
  window.fetch = function (url, init) {
    if (init) {
      spy(init.body);
      spy(JSON.stringify(init.headers || {}));
    }
    return realFetch.apply(this, arguments);
  };
  const ipc = window.ipc;
  if (ipc && ipc.postMessage) {
    const post = ipc.postMessage.bind(ipc);
    try { ipc.postMessage = (m) => { spy(m); return post(m); }; } catch (e) {}
  }
  const invoke = window.__TAURI_INTERNALS__.invoke;
  const realInvoke = invoke.bind(T);
  T.invoke = function (cmd, args, options) {
    if (args) spy(JSON.stringify(args));
    return realInvoke(cmd, args, options);
  };
  const attempt = (name, cmd, args) => realInvoke(cmd, args)
    .then((v) => { out[name] = 'ALLOWED ' + JSON.stringify(v).slice(0, 60); })
    .catch((e) => { out[name] = 'refused: ' + String(e).slice(0, 70); });
  const xhr = (token) => ({
    token, op: 'xhr',
    args: { rid: 'x', url: 'http://localhost:8099/test/hello.txt', method: 'GET' },
  });
  addEventListener('load', () => setTimeout(async () => {
    await attempt('fake_token', 'gm_call', xhr('0'.repeat(32)));
    await attempt('install', 'script_install', { source: 'x', url: null });
    await attempt('panel_view', 'scripts_view', {});
    await attempt('resolve', 'link_resolve', { tab: null, url: 'https://example.com/' });
    await attempt('new_tab', 'tab_new', { url: 'https://example.com/', private: false });
    await attempt('answer', 'script_answer', { ask: 1, answer: 'always' });
    out.tokens_stolen = stolen.length;
    if (stolen.length) {
      const token = stolen[0];
      await attempt('stolen_xhr', 'gm_call', xhr(token));
      await attempt('stolen_download', 'gm_call', { token, op: 'download', args: { url: 'http://localhost:8099/x' } });
      await attempt('stolen_open_tab', 'gm_call', { token, op: 'open_tab', args: { url: 'https://example.com/' } });
      await attempt('stolen_values', 'gm_call', { token, op: 'values', args: {} });
      await attempt('stolen_save_escape', 'gm_call', { token, op: 'save_begin', args: { name: '../../escaped.txt', size: 0 } });
    }
    document.title = 'RESULT ' + JSON.stringify(out);
    // With ?leave=1 the page walks away in the middle of its save.
    if (location.search.includes('leave=1')) location.href = 'gm-test.html';
  }, 1500));
})();
