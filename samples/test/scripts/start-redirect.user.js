// ==UserScript==
// @name         BM Osprey start redirect test
// @name:ja      開始時リダイレクトのテスト
// @namespace    bm-osprey-tests
// @version      1.0
// @license      MIT
// @description  Runs before the page and leaves for the address after the #, as a redirect skipper does.
// @match        *://127.0.0.1/test/warning*
// @run-at       document-start
// ==/UserScript==

// No @grant at all, so this runs in the page itself.
(function () {
  const target = decodeURIComponent(location.hash.slice(1));
  if (/^https?:\/\//.test(target)) location.replace(target);
})();
