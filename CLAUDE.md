# BM Osprey, notes for Claude Code

BM Osprey is a 1DM+-style media grabber for Windows, macOS and Linux: a
light browser that finds the media a page plays, shows its qualities and
sizes, previews it, and downloads it (HLS pieces joined and merged with
ffmpeg). It stays its own app, not part of BM Aegis. Owner: Bristo Mathew
(GitHub BritMat). Main machine: Windows 10/11, 16 GB RAM, Rust 1.96+.

It was called BM Fetch until 12 Nov 2026. The old names still work for now:
`BMF_*` environment variables are read when the `OSPREY_*` one is not set
(`osprey_core::env_var`), and the old data folder `bm-fetch` is moved to
`bm-osprey` on first start (`filter::home_dir`). The launch plan is
`osprey/LAUNCH.md`.

## How Bristo wants things written

- Plain, natural language. No em dashes, no semicolons, no AI-corporate
  phrasing. This applies to code comments, docs, commit messages and UI
  text as well as chat.
- Never make things up. If something was not tested, say so.
- Keep `PROGRESS.md` up to date: after each finished step, write what is
  done, what is in progress (with the file and function being changed)
  and the next step, then commit. Sessions can stop at a usage limit
  without warning, and this file plus `STATUS.bat` is how the next
  session (or Bristo) finds where the work stopped.
- Commits: `git -c user.name="Bristo Mathew" -c user.email="reach.bristo@gmail.com" commit ...`

## Layout

- `crates/osprey-core` the engine, no UI. `probe.rs` (what is at a URL),
  `inspect.rs` (checks a candidate with the server: qualities, size,
  resolution, length, junk filtering), `hls/` (playlist parser and piece
  downloader, AES, missing-piece tolerance), `direct.rs` (multi
  connection file download with resume), `limit.rs` (shared speed limit),
  `gateway.rs` (local SOCKS gateway every tab and download goes through:
  direct, DNS over HTTPS, WARP, VPN proxy, Tor, ad blocking, fails
  closed), `filter.rs` (uBlock/EasyList via the adblock crate),
  `bypass.rs` (link shorteners), `textmode.rs`, `ffmpeg.rs`, `ytdlp.rs`
  (asks yt-dlp what a page has, try it with `osprey formats URL`), `tools.rs`
  (downloads yt-dlp and Deno into `<BM Osprey home>/tools` on first run and
  updates them, try it with `osprey tools`), `userscript.rs` (reads a
  `.user.js` header, match rules, the store of scripts, `@require` files
  and saved values).
- `crates/osprey-cli` the `osprey` command line.
- `crates/osprey-app` the Tauri v2 browser (`bm-osprey`).
  - `src/main.rs` tabs, windows, commands, settings, session restore
  - `src/capture.rs` native request capture, what a response really is
    and which request headers can be sent again. `src/capture_win.rs` and
    `src/capture_linux.rs` are the engine hooks
  - `src/media.rs` Media panel state, server checks, pop-up rules
  - `src/downloads.rs` download manager (queue, pause, resume, retry,
    speed and time left)
  - `src/preview.rs` local preview server for the panel's player, and for
    card pictures (`/i/`, images only, nothing else is fetched)
  - `src/prefs.rs` the settings page: what it shows and changes, export
    and import, filter lists, tool versions, cache
  - `src/probe.rs` the `--probe-page` test run (see Testing)
  - `src/scripts.rs` the script manager and `gm_call`, the one command a
    script reaches. It checks every call in Rust: token, tab, `@grant`,
    `@connect`. `src/extensions.rs` real extensions on Windows
  - `scripts/sniffer.js` injected in every frame, reports media
  - `scripts/userscripts.js` runs scripts in the page: sandbox, GM_
    functions, saves made in the page. It has no power of its own, a page
    can watch it. Keep the checks in `gm_call`
  - `ui/` the browser chrome (plain HTML, CSS, JS, no framework). The look
    comes from `osprey/design/mockup.html`: keep its `:root` tokens and
    themes in `ui/app.css`,
    `ui/vendor/hls.min.js` for stream previews
- `dev.bat` the Windows dev tool, `RUN-CHECKS.bat` makes a results zip.
- `scripts/dev_server.py` local test server with Range support.
- `scripts/test_userscripts.py` runs the MIT scripts in
  `samples/test/scripts` and prints PASS or FAIL. `--real` also runs the third
  party scripts in `samples/userscripts`, which are on Bristo's machine only,
  never in git. `samples/test/` also has a hostile page and a test extension.
  The spec is `docs/userscripts-and-bypass.md`.

## Commands

- `dev check`, `dev build`, `dev release`, `dev fast`, `dev test`, `dev full`
- `dev browser` builds and opens the browser
- `dev page "URL" [SECONDS]` the page test, see below
- `dev report` every check plus a results zip
- Plain cargo works too: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`, `cargo build --release -p osprey-app`
- `dev fast` or `cargo build --profile fast` is a release style build in
  `target\\fast`, for testing when C: is short of space. Ship from `--release`.
- Before every commit: fmt, clippy with `-D warnings`, and all tests must pass.

The dev profile uses `debug = "line-tables-only"` and dependencies build
without debug info, and dev.bat picks CARGO_BUILD_JOBS from free memory.
That is because full builds ran out of memory on Bristo's machine. Keep it.

## Testing on real sites (do this instead of asking for screenshots)

```
target\release\bm-osprey.exe --probe-page "https://site/episode" --probe-wait 30 --probe-out logs\probe-latest.json
```

or `dev page "https://site/episode" 30`. The browser opens the page, mutes
and starts every video (clicking the usual play buttons in every frame),
waits, writes a JSON report and closes. The report has:

- `summary`: how many candidates were captured, reported, listed, dropped,
  ad filtered
- `listed`: exactly what the Media panel shows, with qualities and sizes,
  where each item came from (`source`), and what the engine saw
  (`captured`: status, type, referer, origin, header names). Cookie values
  are never written to a report
- `events`: every candidate in order with what happened to it, including
  the reason for each drop
- `network`: every response the web engine showed
- `sniffer_only`: finds the engine did not show, so native capture missed them

The probe loads lazy frames, presses the known play buttons, and clicks
the middle of embedded players. Most players load no media until clicked,
so if a report shows no media, first check the player was started (look
for its host in `network`). A new site may need its button added to
`AUTOPLAY_JS` in `src/probe.rs`.

`--probe-format 1080` also previews and downloads that height from a
yt-dlp card, the same way the panel's buttons do, and saves next to the
report. Result is in `format_test`.

If a build fails with "invalid metadata files" or "can't find crate", look
for "memory allocation failed" in the output. The page file is a fixed
10 GB, so rustc can run out when Chrome and WSL are open.

Saved tabs are not touched by a probe. Use it after every change that
affects media capture, and compare reports before and after.

If Bristo's browser is open, it locks `target\release\bm-osprey.exe`
and a release build fails at the very end with "Access is denied". Do not
close his browser. The new exe is at `target\release\deps\bm_osprey.exe`,
and `OSPREY_HOME` set to an empty folder gives a probe its own profile.

Script options: `--probe-script-dir`, `--probe-answers`, `--probe-panel`,
`--probe-link`, `--probe-extra-match`. They are described in `src/probe.rs`.
The report then has `scripts`, `script_installs` and `panel_test`.

Sites Bristo uses for testing: animekhor.one and anime4i.com episode pages
(VK, ok.ru and Dailymotion players inside frames). Ask him for current
episode links if needed.

## The build plan (Bristo's feedback, 4 Nov 2026), in order

1. **Native request capture, the way 1DM+ does it on Android.** Built on
   Windows with WebView2's `WebResourceResponseReceived`, on Linux with
   WebKitGTK's `resource-load-started` and the resource's response.
   macOS keeps the sniffer. Items are classified by the real response
   type, and checks, previews and downloads use the real request headers.
   Tested on WebView2 156: no filter is needed for frames. See Known gaps.
2. **yt-dlp as an extractor engine.** Bundle yt-dlp (and Deno, which yt-dlp
   now needs for YouTube) and keep them updated in the background. For a
   page, run `yt-dlp -J` first. If it knows the site, list all formats,
   audio only, subtitles and the thumbnail. Downloads still go through
   BM Osprey's engine and gateway (speed limit, pause, route, progress).
   Fall back to our own capture when yt-dlp does not know the site.
3. **DASH (.mpd)** in osprey-core: SegmentTemplate, SegmentList, SegmentBase,
   separate video and audio joined with ffmpeg. VK and ok.ru use DASH.
   Also fix the Media panel count: it says "0 found" while a DASH item is
   listed, because unsupported items are not counted.
4. **Previews that play like 1DM's.** Send each preview request with the
   referrer and origin of the frame that played it (Dailymotion refuses
   animekhor's page as referrer). Make hls.js retry and then skip broken
   pieces instead of stopping at 0:00. Show the real reason when a preview
   fails (for example "server refused (403)"). Allow previewing a download
   that is still running, from the pieces already saved.
5. **Settings page** behind a gear button: download folder, connections,
   ffmpeg path, tab sleep time, memory saver, hardware acceleration on or
   off, cache size, filter lists, clear data, import and export settings.
6. **User scripts.** A built-in Tampermonkey-style manager on all systems
   (install .user.js, @match, @run-at, the common GM_ functions). On
   Windows also allow loading real browser extensions through WebView2's
   extension support (`AreBrowserExtensionsEnabled`, `AddBrowserExtensionAsync`).
7. **Media panel groups**: Video, Audio, Images, Subtitles, Unidentified,
   collapsible with counts. Nothing is dropped silently: anything the
   check cannot confirm goes under Unidentified.
8. **Dailymotion cards.** The player loads quality playlists directly, so
   cards show only the length and the same video shows up several times.
   Work out resolution from the first piece and size from piece sizes,
   and merge the cards of one video into one card with qualities as rows.
9. **Link grabber and queue**, JDownloader style: paste a page, a list or
   a playlist, get everything downloadable grouped by site, with filters,
   folder rules and a scheduler.
10. **Developer mode** switch: no size or type filters, show the drop
    reason on every item, download anything, no "already downloaded" lock.

Out of scope: anything that removes DRM (Widevine and similar).

## Known gaps

- bypass.vip's paid API has never been called. The address and `x-api-key`
  come from the code of their own web page (`assets/js/scripts.js`), not from
  documentation, and no key was at hand. A local test checks what BM Osprey
  sends. What it does with a real answer is unknown, and so is the error
  shape of a bad key. If their page changes, `BYPASS_VIP_ENDPOINT` may too.
- The key is plain text in `browser.json`. The interface only learns whether
  one is saved.
- Their page takes no link in its address, so Open in bypass.vip fills their
  input box by its id, `bypassInput`. If they rename it, the page still opens
  but the box stays empty.
- GM functions fail in frames where Tauri's invoke is missing. Not tested.
  All 10 sample scripts ran only in the top page.
- A tab reads a script's saved values once when the page opens, so a change
  made in another tab shows after a reload. `GM_addValueChangeListener` does
  not exist.
- A script installed or changed needs that tab reloaded. Start up scripts are
  fixed when a web view is made, so reload builds a new one and page state
  is lost, as with a sleeping tab.
- A page can steal the token of a script running on it, seen with
  `samples/test/hostile.js`. It then gets only what that script was granted.
  Every script may also save files to the Downloads folder without a grant.
- `showSaveFilePicker` does not show a picker. The file goes to Downloads.
- Quitting during a script's save leaves a `.part` file. Leaving the page or
  closing the tab cleans it up.
- User scripts do not run in private tabs, and extensions do not load there.
- Extensions only load if one is present at start up. Tested with one small
  content script, nothing bigger, no toolbar buttons or pop-ups.
- WebView2's response event does not fire for requests made inside a
  worker. `WebResourceRequested` does see them, but as source kind
  "document" with no response, so finding them needs a `*` filter that
  stops every request on the window thread. Not done.
- The Linux capture hook has never run. It needs testing, in particular
  whether WebKitGTK gives Referer and Cookie in the request headers.
- Dailymotion's first `dmxleo` request answers 204 for anyone, curl too.
  It is not the stream and it does not stop the player. The player waits
  for a click, then loads the real playlists.
- The real Dailymotion master playlist (`cdndirector...m3u8?sec=`) was 200
  for the player but 403 for BM Osprey's check, even with the captured
  headers, so it is dropped. Why is not known yet. Curl also got 403 on a
  fresh link from the metadata JSON, and yt-dlp got "Access forbidden" for
  one video. WARP alone is not the cause: with WARP on, yt-dlp listed all
  formats of Big Brother 166 (k4MmWFA0CAZBI1MmMfs). Some Dailymotion videos
  work and some do not, and why is not known yet.
- yt-dlp 2026.10.26 fails on animekhor.one pages. Its generic scan picks up
  the frame address with the rest of the tag attached and then crashes
  inside its Dailymotion extractor. BM Osprey treats that as "not known"
  and falls back to its own capture.
- Dailymotion's quality playlists (`vod3.cf.dmcdn.net`) never reach the
  response event, though their pieces do. The sniffer finds them, so they
  are listed, but without captured headers. Why is not known yet. It is
  not a worker, since the sniffer cannot see those.
- ok.ru shows a Yandex pre-roll ad as a normal item, twice (the address
  and its redirect). The filter lists do not catch it.
- A candidate the sniffer reports before the engine answers is checked
  without real headers, and if that check fails the engine's later view
  is ignored. Not seen in a probe yet, found by reading the code.
- `anime1u.com` (anime4i's Multisub server) answered 403 for its playlist
  when the embed was opened on its own. Not tested inside anime4i.
- YouTube answered "Sign in to confirm you're not a bot" with WARP on, so
  yt-dlp with Deno has not been seen working on YouTube. yt-dlp gets no
  cookies from the tab yet.
- DASH formats from yt-dlp are hidden until item 3. Subtitles are counted
  on the card but cannot be saved yet.
- Preview of a video only row plays its audio only when both are HLS. A
  pair of plain files (YouTube style) previews with no sound. The download
  is joined either way.
- First run downloads about 65 MB (about 120 MB on disk) with nothing
  shown in the interface. Until it finishes, the panel uses own capture.
- The tool downloads check the published SHA-256. That catches a damaged
  download, not a hacked release, since the sum comes from the same place.
- Pausing a stream keeps finished pieces only, partly fetched pieces are
  downloaded again.
- With a speed limit, the speed reading can still swing when the server
  delivers very large chunks (seen on a local test server). The average
  holds the limit.
- Pop-up logins (OAuth windows) open as tabs and may not finish signing in.
