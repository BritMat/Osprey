# BM Osprey

A cross platform media grabber for Windows, macOS and Linux, inspired by 1DM+ on Android.

It finds the media on a page, downloads it fast over several connections, and when a video arrives in hundreds of small pieces (HLS streams) it fetches every piece, decrypts the ones that use standard AES-128, puts them back in order and merges video and audio into one normal MP4. It can then convert the result to other formats.

The engine is a plain Rust library (`osprey-core`) with no UI, so the same code can power the command line tool today, a desktop app next, and BM Aegis later.

## What works now

**Downloading**
- Multi connection downloads for normal files, split into parts that download in parallel
- Resume after a crash or a dropped connection, for both files and streams
- HLS streams (.m3u8): picks the best quality or the one you ask for, grabs separate audio tracks, handles byte range playlists and fMP4 init sections
- AES-128 decryption for the standard clear key encryption most HLS sites use
- Merging of video and audio with ffmpeg, without re-encoding
- Conversion to mp4, mkv, webm, mp3, m4a, opus, flac, wav or gif
- Give `osprey get` a web page or a short link and it finds the media itself

**Built in add-ons**
- **Link bypass** follows short links and redirect pages to where they really go, and strips tracking tags from the result
- **Filtering** blocks ads and trackers with the real uBlock Origin and EasyList filter lists, plus 1DM style rules for size, type, extension, host and keywords
- **Text-only mode** shows any page as just its words and links. Images, GIFs, video, scripts and ads are removed
- **Anonymity** sends everything through Tor, a proxy or a VPN proxy, with encrypted DNS (1.1.1.1, Quad9 or Google) and a generic browser identity. One switch, `--anon`, turns it all on

**BM Osprey Browser**, a desktop browser with all of the above built in: tabs, private tabs, ad blocking, a Media panel that catches what pages play, downloads that look like the page's own player, and a choice of network routes from fast to anonymous. See [The browser](#the-browser).

DASH (.mpd) is detected but not downloaded yet. It is the next big item on the [roadmap](ROADMAP.md).

### Not supported on purpose

DRM protected streams (Widevine, FairPlay, PlayReady, SAMPLE-AES) are refused with a clear message. BM Osprey does not try to break DRM. Shorteners that need a captcha or a server side timer are not skipped either, and `osprey bypass` says so when it meets one. Please only download content you have the right to save.

## Install

You need [Rust](https://rustup.rs) and, for merging and converting, [ffmpeg](https://ffmpeg.org/download.html) on your PATH. On Windows Rust also needs the Visual Studio C++ build tools.

### Windows, the easy way

**To check everything and get a results zip, double click `RUN-CHECKS.bat`.** It builds BM Osprey, runs every check and saves `osprey-results-<date>.zip` in this folder, which opens in Explorer when it is done. Upload that zip to get the results read. The first run compiles everything and takes 5 to 15 minutes.

What the zip holds: `summary.txt` (PASS, FAIL, WARN or SKIP for each check), `report.log` (the full output) and `osprey-diagnostic.log` (BM Osprey's own step by step log). Your IP address is masked, and no downloads or settings are included. Build and tests are required checks. To keep it quick on any computer, lint and tests cover the engine and the command line, and the browser is built once in release mode and checked with its self-test. GitHub's CI lints and tests every part on every push. A progress line shows every 30 seconds while a long step runs. Internet, encrypted DNS, Tor and ffmpeg checks are optional and only count as warnings, because they depend on your network rather than the code. The Tor check only passes while Tor Browser is open.

If Windows shows "Windows protected your PC" the first time, click **More info**, then **Run anyway**. That happens with any batch file downloaded from the internet.

For everything else, double click `dev.bat` and pick an option from the menu, or use it from a terminal:

```bat
dev check       :: is everything installed? tells you what is missing and how to get it
dev full        :: format, lint, tests and a release build in one go
dev run get "https://site.example/master.m3u8"
```

Every action writes a log to the `logs` folder. When something fails, `dev.bat` prints the important lines, explains known problems in plain words, and keeps a copy at `logs\latest.log` that you can send for help.

### Any system

```sh
git clone https://github.com/BritMat/bm-osprey
cd bm-osprey
cargo build --release
```

The binary ends up in `target/release/osprey` (`osprey.exe` on Windows). To use an ffmpeg that is not on your PATH, set `OSPREY_FFMPEG` to its full path.

Tagged versions also get prebuilt binaries for all three systems on the Releases page.

## Usage

```sh
# What is this URL? Size, resume support, available qualities
osprey probe https://example.com/stream/master.m3u8

# Download the best quality and merge it into one MP4
osprey get https://example.com/stream/master.m3u8

# Cap the quality at 720p, save into a folder, use 16 connections
osprey get https://example.com/stream/master.m3u8 -q 720 -o ~/Videos -c 16

# Give it the page you watch on. It finds the video, sets the Referer and downloads
osprey get https://example.com/watch/1
osprey get https://example.com/watch/1 --pick 2      # when the page has several

# Sites that check where the request came from
osprey get https://cdn.example.com/v.m3u8 -H "Referer: https://example.com/watch/1"

# Convert. The output extension picks the format
osprey convert video.mp4 audio.mp3
osprey convert video.ts video.mp4 --copy
```

Add `--json` to most commands for machine readable output.

### Link bypass

```sh
osprey bypass https://short.example/abc
```

```
start   https://short.example/abc
  301 redirect     https://short.example/interstitial
  meta refresh     https://short.example/wait
  script jump      https://out.example/go?url=https%3A%2F%2Fdest.example%2Fpage...
  ?url= link       https://dest.example/page?id=7&utm_source=short
  tracking removed https://dest.example/page?id=7
final   https://dest.example/page?id=7
```

It handles HTTP redirects, redirect wrappers like `?url=` and `google.com/url?q=`, targets hidden in base64, meta refresh pages and small pages that jump on with a script.

### Filtering

```sh
osprey filters update      # download EasyList, EasyPrivacy and the uBlock Origin lists
osprey filters list        # what is installed, and where your own rules file is
osprey filters check https://pubads.g.doubleclick.net/ad.mp4

# Find media on a page. Ads are filtered out automatically
osprey sniff https://example.com/watch/1
osprey sniff https://example.com/watch/1 --show-blocked     # see what was removed and why

# 1DM style rules
osprey sniff https://example.com/watch/1 --probe --min-size 1MB --type hls,direct --ext mp4
osprey sniff https://example.com/watch/1 --block-host ads.example --block-word preroll
```

A small built in set works before the first update. Your own rules go in `my-filters.txt` in the filter folder, written in uBlock Origin syntax. Lists live in `%APPDATA%\bm-osprey\filters` on Windows, `~/Library/Application Support/bm-osprey/filters` on macOS and `~/.local/share/bm-osprey/filters` on Linux. Set `OSPREY_HOME` to use another folder.

### Text-only mode

```sh
osprey text https://news.example/article                   # plain text in the terminal
osprey text https://news.example/article -o article.html   # clean, light web page
osprey text saved-page.html --no-images                    # works on saved files too
```

Images with a description become small `[image: description]` links so you can still open one. Images without a description, tracking pixels, GIFs, video, audio, iframes, scripts, styles and forms are removed. Ads are removed with the uBlock Origin cosmetic filters, links to ad domains are dropped and tracking tags are cut from the remaining links. Add `--no-filter` to keep everything that is not an image or script.

### Privacy and anonymity

```sh
osprey --anon get https://example.com/watch/1            # Tor + generic identity + no log
osprey --tor sniff https://example.com/watch/1           # just Tor
osprey --dns cloudflare get https://example.com/v.m3u8   # encrypted DNS through 1.1.1.1
osprey --proxy socks5://127.0.0.1:1080 get ...           # your own proxy or VPN app

osprey privacy on            # anonymous mode for every run from now on
osprey privacy dns quad9     # or keep a direct connection with encrypted DNS
osprey privacy show          # what is saved, and what this run uses
osprey privacy check         # which IP do sites see? is Tor really on? does DNS work?
osprey privacy off
```

For Tor, open [Tor Browser](https://www.torproject.org/download/) and leave it running, or install the Tor service. BM Osprey finds it on port 9150 or 9050 by itself (`--tor-address` for anything else).

How it protects you:

- **Everything goes the same way.** Downloads, page loads, link bypassing and filter list updates all use the route you chose. There is one place in the code where connections are made, so nothing can skip it.
- **It fails closed.** If Tor or your proxy is not running, BM Osprey stops with an error and sends nothing. It never quietly falls back to a direct connection.
- **No DNS leaks.** Through Tor or a proxy, site names are looked up by the proxy (`socks5://` is upgraded to `socks5h://` automatically). With encrypted DNS, the DoH servers are reached by IP address, so even the first lookup never touches your ISP.
- **Generic identity.** Requests carry Tor Browser's user agent and language headers instead of "BM-Osprey".
- **No trace.** In anonymous mode the diagnostic log is off, since it would record the sites you visit. Tracking tags are stripped from links.

What it does not do, so you can judge it honestly:

- Encrypted DNS alone hides the names you look up from your ISP. It does not hide your IP address from websites. Only Tor or a proxy does that.
- A proxy or VPN sees where you go, so it must be one you trust.
- BM Osprey is not Tor Browser. Sites can still tell it apart from a real browser by deeper details such as how it sets up encrypted connections. It hides who you are, not that you are using a download tool.
- Big downloads over Tor are slow. Tor is shared by millions of people, so heavy use is better kept for when you need it.

### Diagnostic log

```sh
osprey --log osprey.log get https://example.com/watch/1
```

Records the version, the arguments, each step and the full chain of causes if something fails, plus a backtrace if the program ever crashes. The `OSPREY_LOG` environment variable does the same. `dev.bat run` turns it on for you.

## The browser

On Windows, double click `dev.bat` and press **B**, or run `dev browser`. It builds the browser the first time, runs its self-test, then opens it. Elsewhere: `cargo run --release -p osprey-app`.

It uses the web engine your system already has (Edge WebView2 on Windows, WebKit on macOS and Linux) instead of bundling a whole Chromium, so it starts fast and stays light. Measured at rest with four tabs open and a video looping in a background tab, the whole browser used about 1 percent of one CPU core. Background tabs are hidden so the engine throttles them, and after 20 minutes they are put to sleep and reload when you come back.

**What it does**

- **Tabs and private tabs.** A private tab keeps no cookies, history or site data after it closes, and has WebRTC, camera and microphone switched off. **Private mode** (Network panel) makes every new tab private.
- **Media panel, checked like 1DM.** Pages and the frames their players live in report what they load. Each candidate is then checked with the server, through your route and with the tab's cookies, so only real media is listed. Error pages, template links, images and tiny ad clips are dropped. Streams show every quality with its resolution, bitrate and estimated size, so you pick the one you want. Files show their real size, and MP4s their resolution and length.
- **Preview before you download**, like 1DM. Every item in the Media panel has a Preview button that plays it in the panel, streams included, using the tab's cookies and route.
- **A download manager that keeps going.** Pause, resume and retry. A dropped connection is retried by itself (twice, with a growing wait). If a handful of stream pieces are gone from the server, the video is still joined with short gaps instead of failing, as 1DM's "merge with missing parts" does. If ffmpeg cannot make an MP4, the raw stream is kept rather than lost. Speed and time left are shown in bytes for streams too.
- **Speed limit** for downloads (Network panel), shared by every download and changeable while they run.
- **Reopens your tabs** next time you start it. Private tabs are never saved. Can be turned off in the Network panel.
- **Pop-up blocker.** A new window opens as a tab only when you clicked a link that opens one, or it stays on the same site. Blank and ad pop-ups are blocked, and the title bar offers to open one if you wanted it.
- **Tabs in the title bar** on Windows, like Chrome, with BM Osprey's own minimise, maximise and close buttons. Drag the empty part of the tab strip to move the window, double click it to maximise.
- **Ad and tracker blocking** with the uBlock Origin and EasyList lists, in two layers: ad domains are refused before anything is sent, and ad boxes on pages are hidden (`##.adsbygoogle` style rules).
- **Text-only view** of any page, with the Aa button.
- **Network routes**, switchable at any time without restarting:

| Route | Speed | Sites see | Your ISP sees | Who must you trust |
|---|---|---|---|---|
| Direct | full | your IP | everything | nobody new |
| Secure DNS (1.1.1.1, Quad9) | full | your IP | which servers you connect to, not the names you look up | the DNS provider |
| Cloudflare WARP (proxy mode) | fast | a Cloudflare IP | encrypted traffic to Cloudflare | Cloudflare |
| VPN proxy (your VPN's SOCKS5) | as fast as the VPN | the VPN's IP | encrypted traffic to the VPN | the VPN provider |
| Tor | slow | a Tor exit IP | that you use Tor | nobody single |

For private downloads that are not as slow as Tor, use **WARP** (free) or your **VPN provider's proxy**. If you do use Tor, downloads spread over several Tor circuits at once (3 by default, set in the Network panel), which is noticeably faster than one. Tor is shared by millions of people, so big downloads are better done over WARP or a VPN.

Every route fails closed. If WARP, the VPN or Tor stops answering, connections are refused and nothing goes out directly. While WARP, a VPN or Tor is on, WebRTC is switched off in every tab, since it could reveal your real IP.

**Downloads that look like watching**

A download started from the browser copies the tab it came from: the same user agent, browser hints, languages, cookies and referring page, and it leaves through the same route, so the site sees one viewer and IP-locked links keep working. The sniffer that finds media is passive. It does not change any page functions that a site could check.

Two download styles, in the Network panel:

- **Natural** (default): four connections at a time with short random pauses, like a player filling its buffer.
- **Fast**: eight or more connections, no pauses. Quickest, easier for a site to notice.

To be honest about the limits: this makes downloads look like the browser fetching media, but a determined site can still tell some tools apart by deeper details, such as how the encrypted connection is set up. It does not get around DRM, logins or paywalls, and it should only be used for media you are allowed to save.

**Why preview is easy on Android and harder on Windows**

1DM+ on Android plays previews with ExoPlayer, the player built into most Android apps, which understands HLS streams natively. It can also see every request a page makes, because Android's WebView lets an app look at each one before it goes out. Both come with the system, so the app stays small.

On Windows, the web engine (Edge WebView2) cannot play HLS streams by itself, and the app framework gives no hook to see a page's requests. BM Osprey works around both. A small player script (hls.js, about 330 KB, loaded only when you open a preview) plays streams on every system. A local preview server sends the player's requests out with the tab's cookies, referrer and route, so the site sees the same viewer it already knows.

**Testing a page**

`dev page "https://site/episode" 30` (or `bm-osprey --probe-page URL --probe-wait 30 --probe-out report.json`) opens the page, mutes and starts its video, waits, and writes a JSON report of everything found: what the Media panel lists, with qualities and sizes, and every other candidate with the reason it was left out. The window closes by itself and your saved tabs are not touched. It is meant for checking real sites quickly, by a person or by Claude Code running on your own computer (see CLAUDE.md).

**Requirements:** Windows 10 or 11 with WebView2 (built in on current Windows, `dev check` tells you if it is missing). macOS 14 or newer. Linux with WebKitGTK 4.1. The browser's self-test (`bm-osprey --self-test`) checks the gateway, ad blocking and fail-closed behaviour without opening a window.

## Project layout

```
crates/
  osprey-core/          the engine (library)
    src/probe.rs       asks the server what a URL is
    src/direct.rs      multi connection downloads with resume
    src/hls/           playlist parser and segment downloader
    src/ffmpeg.rs      merge, remux and convert
    src/detect.rs      media sniffing
    src/bypass.rs      link bypass add-on
    src/filter.rs      ad, tracker and 1DM style filtering
    src/textmode.rs    text-only pages
    src/privacy.rs     Tor, proxies, encrypted DNS, generic identity
    src/settings.rs    saved settings (settings.json)
    src/gateway.rs     local gateway every tab and download goes through
    tests/             end to end tests against local servers
  osprey-cli/           the osprey command
  osprey-app/           the browser (Tauri v2)
    src/main.rs        tabs, panels, text view, page events
    src/downloads.rs   download queue that copies the tab's identity
    src/settings.rs    routes, private mode, download style
    scripts/           sniffer, WebRTC guard and private tab scripts
    ui/                the browser's own interface (plain HTML, CSS, JS)
scripts/
  dev_server.py        local server with Range support for testing
  make_test_stream.sh  builds an encrypted multi quality test stream with ffmpeg
RUN-CHECKS.bat       one click: every check, then a results zip to upload
dev.bat              Windows build, test, run and diagnose tool
samples/             offline sample page used by the checks
```

## Testing

```sh
cargo test
```

The tests start their own tiny HTTP servers, so they need no internet. To try the real thing against a genuine ffmpeg stream:

```sh
scripts/make_test_stream.sh test-stream
python3 scripts/dev_server.py test-stream 8765
osprey get http://127.0.0.1:8765/master.m3u8
```

## Using it from BM Aegis

Add the engine as a dependency and call it from a Tauri command:

```toml
osprey-core = { git = "https://github.com/BritMat/bm-osprey" }
```

```rust
let path = osprey_core::fetch(&url, &osprey_core::FetchOptions::default(), progress).await?;
```

Progress arrives as events through a callback, so it can be forwarded to the frontend with `app.emit(...)`. For the browser side, `osprey_core::classify` decides whether a captured request is media, `Filters::is_blocked` checks it against the ad lists, and `textmode::simplify` turns the current page into its text-only version.

## License

MIT
