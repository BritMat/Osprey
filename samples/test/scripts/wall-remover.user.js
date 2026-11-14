// ==UserScript==
// @name         BM Osprey wall remover test
// @namespace    bm-osprey-tests
// @version      1.0
// @license      MIT
// @description  Removes an anti-adblock wall with a library loaded by @require, the common shape of such scripts.
// @match        http://127.0.0.1/test/wall*
// @require      http://127.0.0.1:8099/test/lib.js
// @grant        GM_addStyle
// ==/UserScript==

(function () {
  'use strict';
  GM_addStyle('body { margin: 3px; }');
  if (typeof ospreyTestLib === 'object') document.getElementById('abnotify').remove();
})();
