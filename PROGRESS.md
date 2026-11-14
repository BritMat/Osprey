# Progress

Last updated: 12 Nov 2026, by Claude Code.

## BM Osprey launch (plan: `osprey/LAUNCH.md`)

Bristo asked for steps 1 to 5 done in order, and to stop and show him
before step 6 (the new public repo).

- Step 1 done: `yt-dlp-tools` and `userscripts` squashed into main as two
  commits. Main never got commit ad428ea, which held the GPL sample scripts.
  fmt, clippy with `-D warnings` and all 145 tests pass.
- Step 2 done: BM Fetch is BM Osprey. Crates `osprey-core`, `osprey-cli`
  (binary `osprey`), `osprey-app` (binary `bm-osprey`), identifier
  `com.britmat.osprey`, data folder `bm-osprey`, downloads in
  `Downloads/BM Osprey`. `OSPREY_*` environment variables, with the old
  `BMF_*` names still read (`osprey_core::env_var`). The old data folder is
  moved on first start (`filter::home_dir`, unit test
  `old_home_folder_is_moved_once`). If the move fails, for example while the
  old browser is still open, the old folder is used as it is. Unfinished
  downloads keep working: old `.bmf.json` and `.bmf-*` work folders are
  renamed when found. The move was only tested in the unit test, not on
  Bristo's real profile.
  fmt, clippy and all 146 tests pass. Once, while the machine was busy, the
  two multi connection tests in `tests/engine.rs` failed (21.8 s run instead
  of 3 s). They passed in the three runs after. Cause not known.
- Step 3 done: the new icons from `osprey/brand/icons` are in
  `crates/osprey-app/icons`. `osprey/brand` stays in the repo. The small
  mark is in the title bar (`ui/index.html`) and on both new tab pages,
  which now use the new colours. Open: macOS `icon.icns`. The Tauri CLI is
  not installed here, so `cargo tauri icon osprey/brand/osprey-1024.png`
  still has to be run once.
- Steps 4 and 5 committed together (they change the same files), but not
  finished: they have not been run in the real app yet. See below.

### Steps 4 and 5, what is built

- `ui/index.html`, `ui/app.css`, `ui/app.js` follow `osprey/design/mockup.html`:
  the whole `:root` token block, four themes, `.calm` for Reduce effects,
  `prefers-reduced-motion`. Glass side panel 392 px wide with a 10 px gap
  (`PANEL_WIDTH` 402 in main.rs), tab strip 40 + toolbar 46 (`TOP_HEIGHT` 86).
  The route line is gone, the route pill dot pulses in the route colour.
- Media panel: groups Video, Audio, Images, Subtitles, Unidentified with
  counts, collapsible. One card per item with a picture, qualities as rows,
  best first and highlighted. Blocked items now show under Unidentified with
  the reason. Empty groups are hidden. Images and Subtitles stay empty until
  the engine collects them (plan items 7 and 2).
- Pictures: the sniffer sends the page's `og:image`, yt-dlp gives its
  thumbnail. `media_thumb` (main.rs) only accepts one of those two for that
  tab, and `Preview::open_image` (`/i/` in preview.rs) serves it only if it
  is a raster image up to 4 MB, never SVG. The content policy allows
  `img-src http://127.0.0.1:*` for this. Test: `pictures_pass_and_other_answers_do_not`.
- Downloads: glowing bar with a sheen while running, grey when paused or
  waiting, green when done, red when failed.
- Settings page (`src/prefs.rs`, gear button or Ctrl+comma, Esc closes). It
  hides the active tab while open (`layout` with `covered`). Sections:
  Downloads (folder typed as a full path, style, speed limit, downloads at
  the same time, which now really changes the queue: `Manager::set_parallel`),
  Performance (tab sleep, and on Windows only hardware acceleration and cache
  size, both passed to WebView2 at the next start, plus Clear cache through
  `ClearBrowsingData` with the disk cache kind), Privacy and network (the
  Network panel's controls, moved into the page while it is open), Ad and
  content filters (block ads, each list on or off, Update now, your own
  rules), Scripts and extensions (the script manager, moved the same way),
  Appearance (theme, Reduce effects), About (version, yt-dlp and Deno
  versions, Update now, licences, export and import). Export writes
  `BM Osprey settings.json` in the download folder, without the bypass.vip
  key unless its switch is on.
- Left out on purpose, no code behind them yet: connections per download as
  a number, carry on after a crash, tray mode, file name template, memory
  saver. Download style (Natural or Fast) stands in for connections.
- fmt, clippy and all 151 tests pass.

### Steps 4 and 5, how far it was tested

- The interface was looked at in Chrome through a local page that loads the
  real `ui/` files with fake data: Media groups and cards, Downloads bars,
  settings Downloads, Privacy and Filters sections, Ember theme. They render
  like the mock-up. That is not the real app.
- A fast profile build finished (`target/fast/bm-osprey.exe`) but was never
  started. Two helper servers were stopped by Claude Code because the
  machine ran low on memory, so the real run did not happen.
- Not tested at all: the real app window, pictures from a real page, the
  cover hiding the tab, hardware acceleration and cache arguments, Clear
  cache, export and import by real clicks, Update now for lists and tools,
  the data folder move on Bristo's real profile, macOS `icon.icns`.
- The CPU check with a download running and the panel open is not done.

## Next

1. Start `target/fast/bm-osprey.exe` with `OSPREY_HOME` set to an empty
   folder, open a page with a video, check the panel, pictures, settings
   cover, and run a probe on animekhor or anime4i to compare with older
   reports. Measure CPU with a download running and the panel open. If the
   sheen or blur costs more than a few percent, tone it down.
2. Then stop and show Bristo before step 6 (new public repo).


## Done (committed)

- On main (a18fcc6): item 1, native request capture, and the yt-dlp module.
- On branch `yt-dlp-tools`, not merged yet: first run download of yt-dlp
  and Deno, yt-dlp formats in the Media panel, and joined audio. fmt, clippy and
  all 125 tests pass.

## Item 1, tested on real pages (5 Nov, debug build)

- animekhor.one, Against the Gods S2 E31: 2 quality playlists listed.
- anime4i.com, I am Yingtai E12: 1 playlist listed, length 1098.8 s.
- ok.ru embed: DASH item stays listed as blocked until item 3, plus a
  Yandex ad shown twice.
- The animekhor run was before the last fix. It was not run again.
- VK was not tested, no current page used it.

## Item 2, what works now

- `tools.rs` downloads both tools on first run with SHA-256 checks and
  looks for updates once a day, through the gateway in the browser.
  Real run into an empty folder: yt-dlp 2026.10.26 and Deno 2.9.8 installed
  in 8 s. A second run downloaded nothing. An update was only tested on a
  local test server, not on a real new release.
- `media.rs` asks yt-dlp about a page once it has media, and shows one card
  with a row per format. Real probe of the ok.ru embed in the browser,
  through the gateway: 10 formats listed.
- A format downloaded with `osprey get` (ok.ru 144p): real file, 1098.97 s,
  h264 and aac, 29 MB against an estimate of 41 MB.

## Item 2, not done or not tested

- The new download and preview buttons on the card were never clicked. The
  probe does not press them. Bristo needs to try one.
- The first run download was tested with `osprey tools`, not by starting the
  browser, because probes skip it on purpose.
- YouTube refused with a bot check while WARP was on, so Deno is unproven.
- macOS and Linux downloads have the right names but never ran.
- See Known gaps in CLAUDE.md for no sound rows, subtitles and first run.

## Joined audio on yt-dlp cards, tested on a real page (5 Nov, debug build)

Bristo's request of 5 Nov: on yt-dlp cards a video only row downloads with
the best audio joined by ffmpeg, those rows show as having sound, audio only
rows stay separate, and the yt-dlp and HLS cards of one Dailymotion video
merge into one.

Test page: anime4i Big Brother episode 166, with `--probe-format`.

- `listed` shows one Dailymotion card with 7 formats, 2 of them audio only.
- 1080p (hls-1080-0 joined with hls-0_aac_q2): state Done, 546 MB, 1920x1080
  h264 and aac stereo, both tracks 934 s, mean volume -20.8 dB, so it has
  real sound.
- 480p: the same result, mean volume -20.8 dB.
- Preview of the 480p row: hls.js, 1 audio track, played 14.5 s in 15 s, with
  audio and video bytes decoded. The video was muted, so nobody heard it.
- The preview of the first run failed at 0 s. WebView2 156 answers "maybe"
  when asked if it can play HLS, so the panel skipped hls.js and the engine
  failed on the stream. hls.js now goes first in `startPreview`. Any other
  HLS preview on Windows had the same problem, so check this on other sites.
- fmt, clippy and all 125 tests pass. The stall seen once in
  `speed_limit_holds_for_streams` did not come back, cause still not known.

Not tested: the 1080p preview (only 480p ran after the fix), a release
build, and the real buttons. The probe calls the same functions as the card
buttons but does not click them.

## Item 6 and link bypass, on branch `userscripts` (5 Nov, debug build)

Bristo's spec is `docs/userscripts-and-bypass.md`. Done from it: section 1
(the script manager), the first bullet of section 2 (interstitial and
redirect bypass) and section 3 (bypass.vip). The other bullets of section 2
were not part of this request and are not done. Windows extensions from item
6 are done too. The branch starts from `yt-dlp-tools`, which is still not
merged.

There are 17 scripts, not 15. 7 are left out on purpose (section 4). The 10
real ones stay in `samples/userscripts` on Bristo's machine, outside git. They
are still in the history of commit ad428ea. Nothing has been pushed, the repo
has no remote.

Tested on the 10 real scripts, before the last changes below: all installed
and ran with no error, each in its own empty profile, on a real page, with
their `@require` files from the real CDNs. What "ran" proves differs.
bypass-link-redirects really redirected. x-media-downloader-zip showed its 2
menu commands. The MediaFire script removed the wall, on a local stand-in
page. The other 7 started logged out, so their download features did not run.

The automated test is now `python scripts/test_userscripts.py OUT`, with 4
small MIT scripts in `samples/test/scripts`. A cargo test checks that together
they use every feature. Latest run: 4 of 4 pass, 32 checks, on the debug exe
and on the fast profile exe.

Also tested, in the browser:

- Run twice with a restart: values kept, @connect deny, always, then no
  question, also after the restart.
- The Scripts panel by real clicks: paste and install, off and on, the reload
  note, reload brings the new script, menu buttons, link box.
- Test extension loaded, on Windows.
- `samples/test/hostile.js`: a fake token and 5 app commands were refused. It
  stole a real token, and functions outside that script's grants were still
  refused. A save name with `../../` stayed in the folder. Leaving mid save
  removed the `.part` file.

bypass.vip now: an optional key, and a switch that stays off without one. Its
address and header come from the code of their own web page.

- No key: the panel shows Open in bypass.vip when the local rules are stuck.
  Clicked in the browser: a tab opened on bypass.vip with the link in its box.
  Their page takes no link in its address, so the box is filled by BM Osprey,
  and nothing is sent until the user presses their button. That button was
  not pressed.
- Switch on with no key: no API call, same button.
- Private mode: no button, and `link_open_vip` refuses.
- With a key: a local test checks the address, the header and the link. A real
  key was never used, so a real answer has not been seen.

Fast profile: `cargo build --profile fast`, or `dev fast`. Full build with one
job at a time took 61 minutes while Chrome used most of the memory. target\fast
is 1.4 GB against 1.8 GB for target\release. A release build was not timed, so
that is not a comparison of time. target\fast is still there.

fmt, clippy and all 145 tests pass, also without `samples/userscripts`.

Not tested: the file picker and install by URL buttons, the key field in the
panel, the Update button in the browser (the update itself is tested in
`engine.rs`), GM functions in frames, macOS and Linux.

## In progress

Nothing half done. All work is committed on branch `userscripts`.

## Older next steps (before the launch plan)

1. Bristo tries the buttons on a yt-dlp card and in the Scripts panel, then
   merge `yt-dlp-tools`, then `userscripts`, into main.
2. Bristo tries the scripts logged in on X, Instagram, Telegram and DeviantArt.
3. With a real key, try one link on bypass.vip's API.
4. Pass tab cookies to yt-dlp and save subtitles. Then item 3 (DASH).
