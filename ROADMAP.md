# Roadmap

## v0.1 Engine and CLI (done)

- [x] Probe URLs (type, size, range support, file name)
- [x] Multi connection downloads with resume
- [x] HLS master and media playlists, quality choice, separate audio
- [x] AES-128 decryption, byte ranges, fMP4 init sections
- [x] ffmpeg merge, remux and convert
- [x] Media sniffing from page source
- [x] CI on Windows, macOS and Linux, release builds on tags

## v0.1.1 Built in add-ons (done)

- [x] Link bypass: redirects, `?url=` wrappers, base64 targets, meta refresh, script jumps
- [x] Tracking tag removal (utm_*, fbclid, gclid and others)
- [x] Ad and tracker filtering with EasyList, EasyPrivacy and uBlock Origin lists
- [x] Your own rules file in uBlock Origin syntax
- [x] 1DM style media rules: size, type, extension, host, keywords
- [x] Text-only mode with cosmetic ad removal, HTML or plain text output
- [x] `osprey get` on a page or short link finds the media itself
- [x] Diagnostic log (`--log` / `OSPREY_LOG`) and `dev.bat` for Windows

## v0.1.2 Privacy and anonymity (done)

- [x] Tor routing through Tor Browser or the Tor service, found automatically
- [x] Proxy and VPN proxy support, always with remote DNS
- [x] Encrypted DNS over HTTPS: Cloudflare 1.1.1.1, Quad9, Google
- [x] Generic browser identity
- [x] `--anon` turns everything on, fails closed, no diagnostic log
- [x] Saved privacy settings and `osprey privacy check`

## v0.2 Browser (done)

- [x] Tauri v2 browser using the system web engine, tabs as separate web views
- [x] Private tabs and private mode, WebRTC guard
- [x] Local gateway for every tab and download: routes, fail closed, no DNS leaks
- [x] Network routes: direct, secure DNS, Cloudflare WARP, VPN proxy, Tor
- [x] Several Tor circuits per download
- [x] Media panel with a passive sniffer, downloads that copy the tab's identity
- [x] Natural and fast download styles
- [x] Ad domain blocking and uBlock style element hiding in tabs
- [x] Text-only view, tab sleeping, crash log, self-test
- [x] Media checked with the server like 1DM, quality picker with sizes
- [x] Media found inside embedded player frames
- [x] Pop-up blocker, tabs in the title bar on Windows

## v0.2.1 Download manager like 1DM (done)

- [x] Preview any found media in the panel, HLS included (hls.js)
- [x] Pause, resume, retry, and automatic retry on dropped connections
- [x] Join streams with a few missing pieces instead of failing
- [x] Keep the raw stream when ffmpeg cannot make an MP4
- [x] Speed limit shared by all downloads, speed and time left in bytes
- [x] Reopen tabs on start (never private ones)

## v0.3 DASH and smarter downloads

- [ ] DASH (.mpd) manifests: SegmentTemplate, SegmentList and SegmentBase
- [ ] Pick audio language and subtitle tracks
- [ ] Download subtitles (WebVTT) and mux them in
- [ ] Live streams: keep polling the playlist until it ends or the user stops
- [ ] Settings file so filter rules and defaults are remembered

## Browser, next steps

- [ ] Keep half-finished stream pieces on pause (now only whole pieces are kept)
- [ ] Scheduled downloads (start at a set time or only on Wi-Fi/unmetered)
- [ ] Clipboard watcher: copy a media link and get offered a download
- [ ] Settings page for the download folder, tab sleep time and ffmpeg path
- [ ] Filter list updates and a list of blocked sites from inside the browser
- [ ] Bundle ffmpeg so it does not need installing
- [ ] Built in Tor (Arti), so Tor Browser does not need to be open
- [ ] Bookmarks and history for normal tabs, never for private ones
- [ ] Pop-up logins (OAuth windows) currently open as tabs and may not
      finish signing in. Proper pop-up windows are needed for those

## v0.4 BM Aegis integration

- [ ] Use `osprey-core` inside BM Aegis as a crate dependency
- [ ] Media detection in Aegis's privacy browser tabs
- [ ] Share the download list between the two apps

## Privacy, next steps

- [ ] Built in Tor using Arti (the Tor Project's Rust implementation), so
      Tor Browser does not need to be running
- [ ] A new Tor circuit per download, so separate downloads cannot be linked
- [ ] Browser tab routed through the same Tor or proxy, with WebRTC and
      other known leak paths switched off
- [ ] Wipe button for history, cache and temporary files
- [ ] Be honest in the UI that a custom browser cannot match Tor Browser's
      fingerprint protection, and point people to Tor Browser when they
      need the strongest anonymity

## Later ideas

- [ ] Browser extension for Chrome and Firefox that sends links to the app
- [ ] Batch downloads from a list of URLs
- [ ] Trim and cut clips before saving
- [ ] Site specific bypass rules as data files, so new ones need no rebuild

## Out of scope

Breaking DRM (Widevine, FairPlay, PlayReady), and solving captchas or faking the human step on link shorteners.
