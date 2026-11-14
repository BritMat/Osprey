// ==UserScript==
// @name         BM Osprey page save test
// @namespace    bm-osprey-tests
// @version      1.0
// @license      MIT
// @description  Saves files the way a page does, with no GM functions: a.download with a blob, and showSaveFilePicker.
// @include      http://127.0.0.*/test/page-save*
// @exclude      http://127.0.0.*/test/page-save-never*
// @grant        none
// ==/UserScript==

(async function () {
  const a = document.createElement('a');
  a.href = URL.createObjectURL(new Blob(['saved by an anchor']));
  a.download = 'page-anchor.txt';
  a.click();
  URL.revokeObjectURL(a.href);

  const handle = await unsafeWindow.showSaveFilePicker({ suggestedName: 'page-picker.txt' });
  const writer = await handle.createWritable();
  await writer.write(new TextEncoder().encode('saved '));
  await writer.write({ type: 'write', data: new Blob(['by a picker']) });
  await writer.close();
  document.title = GM_info.script.name + ' finished';
})();
