# User scripts and link bypass, Bristo's request of 5 Nov 2026

Bristo sent 17 Tampermonkey scripts and asked that their features and the
method of bypass.vip are used in BM Osprey. This file says how. It extends
item 6 (user scripts) and touches items 2, 9 and the existing `bypass.rs`.

## 1. The script manager must run these scripts unchanged

The 10 real scripts are in `samples/userscripts/`, on Bristo's machine only,
outside git. The 7 others are left out on purpose, see section 4. Each one must
install, match its sites and run without changes. `samples/test/scripts/` has
small MIT scripts with the same features, for the automated test. The features they use, and so the minimum the
manager needs:

- Headers: `@match`, `@include` (globs such as `https://bunkr.*/*`),
  `@exclude`, `@run-at document-start` and `document-end`, `@noframes`,
  `@require` (fetch once, cache, keep a hash, refetch only on version change),
  `@connect`, `@grant none` (runs in the page itself), `@name:xx` translations.
- `GM_xmlhttpRequest` and `GM.xmlHttpRequest`: cross-site, through BM Osprey's
  network route, with `responseType` blob, json and text, `responseHeaders`,
  `finalUrl`, `status`, `onload`, `onerror`, `ontimeout`, `onabort`. Only
  hosts allowed by `@connect` (ask the first time for others, like
  Tampermonkey does).
- `GM_download`: hand the file to BM Osprey's download manager, so speed
  limit, pause, resume and the private route all apply.
- `GM_setValue`, `GM_getValue`: saved per script, kept across restarts.
- `GM_registerMenuCommand`: listed under a script menu in the toolbar.
- `GM_addStyle`, `GM_setClipboard`, `GM_openInTab`, `GM_info`, `unsafeWindow`.
- Downloads a script starts in the page (`a.download` with a blob URL, or
  `showSaveFilePicker`) must also land in the download manager.

The panel: list scripts, turn each on or off, install from a file, from a
URL or by pasting, see which scripts ran on the current page, and update from
the script's own update URL.

## 2. Features to build into BM Osprey itself

These are useful everywhere, so they should not depend on a script.

- **Interstitial and redirect bypass** in `bypass.rs`: follow destination
  links carried in the address itself, plain, URL encoded or base64, as in
  the socialmediagirls (`?url=` base64) and Stremio (`/warning#https...`)
  rules. Treat these as general patterns, not only those two sites.
- **Anti-adblock walls**: support uBlock-style scriptlets and cosmetic
  filters and ship an annoyances or anti-adblock list, so walls like
  MediaFire's `#abnotify` are removed by the filter engine.
- **Link extraction** from file and album hosts (cyberdrop, files.fm, koofr,
  bunkr, dropbox and similar) goes into the link grabber (item 9).
- **Site downloads** for X, Instagram, DeviantArt and Mediaset: check that
  yt-dlp (item 2) lists them with all qualities. Where yt-dlp fails, the
  script manager is the fallback.
- **Telegram Web**: its videos stream from addresses like
  `.../stream/{json with fileName and size}` using Range requests. Native
  capture should list them, take the file name and size from that JSON,
  and download them in parts like the scripts do.
- **File name templates** in settings, like the scripts offer:
  `{site} {uploader} {id} {title} {date} {quality} {index}`, with a date
  format.
- **Download history**, so a card shows when something was already saved
  (the X script keeps one). Developer mode (item 10) turns the lock off.

## 3. The bypass.vip method

bypass.vip resolves short links and link lockers on its own servers and
offers a paid API that needs a key. `bypass.rs` already resolves the common
ones locally. Add bypass.vip as a fallback only when the local rules do not
know the link:

- Off by default, with a setting "Use bypass.vip for links BM Osprey cannot
  resolve" and an optional API key. It stays off without a key. When it is
  on, the link is sent to bypass.vip, and the setting must say that.
- Without a key, show an "Open in bypass.vip" button that opens their web page
  with the link.
- Never used in private mode, the button included.
- The request goes through BM Osprey's network route.
- Show which way the link was resolved (local rule or bypass.vip).

## 4. Not built into BM Osprey

These stay possible as scripts the user installs, but BM Osprey does not ship
them or build them in:

- Age check spoofing (AgebypassX) and login walls on adult or mature content
  (the artstation, deviantart, reddit and similar script). Age checks are a
  legal requirement in many countries.
- Instagram login wall removal and redirects to third-party Instagram
  viewers.
- The three "all-in-one" downloaders. They send every link to outside
  services (tenzaa.com, snapwc.com, grabshorts.com, y2mate and others).
  yt-dlp does the same job on the user's own machine.
