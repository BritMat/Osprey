# BM Osprey launch plan

Bristo's decisions of 11 Nov 2026: BM Fetch becomes **BM Osprey**, it stays
its own app (not merged into BM Aegis), and it goes up as a new **public**
repo, `BritMat/bm-osprey`. It gets the new logo, the new look and a settings
page. Do the steps in order, commit after each, and keep PROGRESS.md current.

Files that come with this plan:

- `brand/osprey.svg` the logo source, `brand/osprey-1024.png`
- `brand/icons/` ready app icons: `32x32.png`, `128x128.png`,
  `128x128@2x.png`, `icon.png`, `icon.ico`
- `design/mockup.html` the new look as a working page. Open it in a browser.
  The gear button shows the settings page. `#settings` and `#theme=ember`
  in the address open those views directly.

## 1. Finish what is open

Merge `yt-dlp-tools` and `userscripts` into main with `--squash`, so the
commit that holds the GPL sample scripts never reaches main. fmt, clippy with
`-D warnings` and all tests must pass.

## 2. Rename

- Product name everywhere a person sees it: BM Osprey.
- Crates: `bmf-core` to `osprey-core`, `bmf-cli` to `osprey-cli` (binary
  `osprey`), `bmf-app` to `osprey-app` (binary `bm-osprey`).
- Tauri identifier `com.britmat.osprey`, window title, installer name.
- Data folder `bm-fetch` to `bm-osprey`. On first start, if the old folder
  exists and the new one does not, move it, so settings, scripts and tools
  are kept. Default download folder `BM Osprey`.
- Environment variables `BMF_*` to `OSPREY_*`. Keep reading the old names
  for now.
- README, ROADMAP, CLAUDE.md, PROGRESS.md, dev.bat, RUN-CHECKS.bat,
  STATUS.bat, CI workflows, UI text. Search for `fetch` and `bmf` when done.
  The word "fetch" as a plain verb in code is fine.

## 3. Logo

Copy `brand/icons/*` over `crates/osprey-app/icons/`. Keep `brand/` in the
repo. macOS needs `icon.icns`: make it from `brand/osprey-1024.png` with
`cargo tauri icon` if the Tauri CLI is installed, otherwise note it as open.
Use the small mark from the mock-up's title bar in the app's own title bar
and on the new tab page.

## 4. New look

Port `design/mockup.html` into `ui/app.css`, `ui/index.html` and `ui/app.js`.

- Take the whole `:root` token block as it is, with the four themes (Sea,
  Ember, Aurora, Slate) and the `.calm` rules for "Reduce effects".
- Depth comes from the `--raise`, `--raise-hi` and `--press` shadows and the
  glass side panel. No WebGL, no web fonts, no images. Animations are CSS
  only, and only on things that are moving anyway (progress, the route dot).
- Media panel: collapsible groups with counts (Video, Audio, Images,
  Subtitles, Unidentified), one card per video with a thumbnail, qualities
  as rows, the best quality highlighted.
- Thumbnails: use the page's `og:image` or yt-dlp's thumbnail, loaded
  through the preview server so the app's content policy stays strict. Show
  the gradient placeholder when there is none.
- Downloads: the glowing bar with a moving sheen while running, grey when
  paused, green when done.
- Check CPU use with a download running and the panel open. If the sheen or
  blur costs more than a few percent, tone it down.
- Honour `prefers-reduced-motion`.

## 5. Settings page

A gear button in the toolbar opens settings over the page, as in the
mock-up. Sections, each backed by the real setting:

- **Downloads**: folder, connections per download, speed limit, downloads
  at the same time, carry on after a crash, tray mode, file name template.
- **Performance**: tab sleep time, memory saver, hardware acceleration,
  cache size and a clear cache button.
- **Privacy and network**: what the Network panel has now (route, DNS, Tor
  circuits, always private, restore tabs), plus clear browsing data.
- **Ad and content filters**: lists on or off, update now, custom rules.
- **Scripts and extensions**: the script manager, the bypass.vip key.
- **Appearance**: theme, reduce effects.
- **About**: version, yt-dlp and Deno versions with update buttons, licences.
- Export and import of settings as one JSON file (never the bypass.vip key
  unless asked).

A setting that has no code behind it yet must not be shown as if it works.
Leave it out until it does.

## 6. The new repo

A fresh history, so nothing from the old one (the GPL samples, old logs) is
published. The old folder stays as it is, as a backup.

1. Copy the project to a new folder `bm-osprey`, without `target`, `logs`,
   `samples/userscripts`, `*.zip` and `.git`.
2. Before the first commit, check: no keys, tokens or personal paths
   (`git grep -n -i "api.key\|token\|C:\\\\Users"` after `git add`), MIT
   LICENSE present, a THIRD-PARTY notice listing hls.js (Apache-2.0) and
   the Rust crates' licences (`cargo about` or `cargo license`), `.gitignore`
   covers `target`, `logs`, `samples/userscripts`.
3. README for strangers: what it is, a screenshot of the new look, how to
   build, what works and what is untested (Linux capture hook, macOS).
   Say plainly that DRM is not handled and that downloading may be against
   a site's terms.
4. `git init -b main`, one first commit, authored as
   `Bristo Mathew <reach.bristo@gmail.com>`.
5. Create and push. With the GitHub CLI: `gh auth login` (Bristo does this
   once), then
   `gh repo create BritMat/bm-osprey --public --source . --remote origin --push`.
   Without it, Bristo makes an empty repo named `bm-osprey` on github.com
   and you add it as `origin` and push.
6. Set the repo description and topics, and check that CI passes on GitHub.
   Tag `v0.3.0` only after Bristo has tried the build.

## 7. After launch

The plan in CLAUDE.md carries on: DASH, then the download manager ideas from
Mister Download Manager (re-splitting, crash-safe resume, queues and
scheduler, tray mode, batch links), link grabber, developer mode.
