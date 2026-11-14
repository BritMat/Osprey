'use strict';
// The new tab page. Typing here works like the address bar.
if (location.hash === '#private') {
  document.body.classList.add('private');
  document.title = 'Private tab';
  document.getElementById('note').innerHTML =
    '<b>Private tab.</b> Cookies, history and site data from this tab are gone when you close it, and WebRTC is off so it cannot leak your real address. Downloads you save stay on disk.';
}

document.getElementById('search').addEventListener('submit', (e) => {
  e.preventDefault();
  const q = document.getElementById('q').value.trim();
  if (!q) return;
  let target;
  if (/^https?:\/\//i.test(q)) target = q;
  else if (!/\s/.test(q) && /\.[a-z]{2,}(\/|$|:)/i.test(q)) target = 'https://' + q;
  else target = 'https://duckduckgo.com/?q=' + encodeURIComponent(q);
  location.href = target;
});
