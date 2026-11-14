use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use osprey_core::filter::{self, parse_size};
use osprey_core::hls::{self, Playlist};
use osprey_core::privacy::{self as privacy_mod, Privacy, Route};
use osprey_core::settings::Settings;
use osprey_core::textmode::{self, TextModeOptions};
use osprey_core::{
    build_client, bypass, ffmpeg, probe, scan_text, FetchOptions, Filters, FoundMedia, MediaKind,
    MediaRules, Progress, ProgressFn, RequestKind, RequestOptions, Unit,
};

#[derive(Parser)]
#[command(
    name = "osprey",
    version,
    about = "BM Osprey: grab media, rebuild split streams, merge and convert"
)]
struct Cli {
    /// Write a diagnostic log (steps, errors, crash details) to this file.
    /// The OSPREY_LOG environment variable does the same.
    #[arg(long, global = true, value_name = "FILE")]
    log: Option<PathBuf>,

    #[command(flatten)]
    privacy: PrivacyArgs,

    #[command(subcommand)]
    command: Command,
}

/// Privacy switches. They work with every command and override the
/// settings saved with `osprey privacy`.
#[derive(Args, Clone, Default)]
struct PrivacyArgs {
    /// Anonymous mode: everything through Tor, generic browser identity, no diagnostic log.
    #[arg(long, global = true)]
    anon: bool,
    /// Send everything through Tor. Tor Browser or the Tor service must be running.
    #[arg(long, global = true)]
    tor: bool,
    /// Tor's SOCKS address, when it is not on the usual 9050 or 9150.
    #[arg(long, global = true, value_name = "HOST:PORT")]
    tor_address: Option<String>,
    /// Send everything through a proxy, like socks5://127.0.0.1:1080.
    #[arg(long, global = true, value_name = "URL")]
    proxy: Option<String>,
    /// Encrypted DNS: cloudflare (1.1.1.1), quad9, google, or system.
    #[arg(long, global = true, value_name = "NAME")]
    dns: Option<String>,
    /// Ignore the saved privacy settings for this run.
    #[arg(long, global = true)]
    direct: bool,
}

/// The privacy setup for this run, fixed once at start up.
static PRIVACY: std::sync::OnceLock<Privacy> = std::sync::OnceLock::new();

fn active_privacy() -> Privacy {
    PRIVACY.get().cloned().unwrap_or_default()
}

/// Request options carrying the run's privacy setup. Every network call
/// starts from here, so nothing can skip Tor or the chosen DNS.
fn base_options() -> RequestOptions {
    RequestOptions {
        privacy: active_privacy(),
        ..Default::default()
    }
}

fn resolve_privacy(a: &PrivacyArgs) -> Result<Privacy> {
    if (a.tor || a.anon) && a.proxy.is_some() {
        bail!("choose either Tor or a proxy, not both");
    }
    let mut p = if a.direct {
        Privacy::default()
    } else {
        Settings::load(&filter::home_dir())?.privacy.to_privacy()?
    };
    let saved_tor_address = match &p.route {
        Route::Tor(addr) => addr.clone(),
        _ => None,
    };
    if a.anon {
        p = Privacy::anonymous();
    }
    if a.anon || a.tor || a.tor_address.is_some() {
        p.route = Route::Tor(a.tor_address.clone().or(saved_tor_address));
    }
    if let Some(proxy) = &a.proxy {
        p.route = Route::Proxy(proxy.clone());
    }
    if let Some(dns) = &a.dns {
        p.dns = dns.parse()?;
    }
    Ok(p)
}

fn is_anonymous(p: &Privacy) -> bool {
    p.is_tor() && p.generic_identity
}

mod diag {
    //! Diagnostic log. Off unless --log or OSPREY_LOG is given. Meant to be
    //! attached to bug reports, so it records what happened in plain words.
    use std::io::Write;
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    static LOG: OnceLock<(Mutex<std::fs::File>, Instant)> = OnceLock::new();

    pub fn init(path: &Path) {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path);
        match file {
            Ok(f) => {
                let _ = LOG.set((Mutex::new(f), Instant::now()));
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                line(&format!(
                    "osprey {} on {}/{} (unix time {secs})",
                    env!("CARGO_PKG_VERSION"),
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ));
                line(&format!(
                    "args: {:?}",
                    std::env::args().skip(1).collect::<Vec<_>>()
                ));
                let default_hook = std::panic::take_hook();
                std::panic::set_hook(Box::new(move |info| {
                    line(&format!(
                        "CRASH: {info}\n{}",
                        std::backtrace::Backtrace::force_capture()
                    ));
                    default_hook(info);
                }));
            }
            Err(e) => eprintln!("  could not open log file {}: {e}", path.display()),
        }
    }

    pub fn line(msg: &str) {
        if let Some((file, start)) = LOG.get() {
            if let Ok(mut f) = file.lock() {
                let _ = writeln!(f, "[{:>8.3}s] {msg}", start.elapsed().as_secs_f64());
            }
        }
    }
}

#[derive(Args, Clone, Default)]
struct RequestArgs {
    /// Extra request header, for example -H "Referer: https://site.example/". Repeatable.
    #[arg(short = 'H', long = "header", value_name = "NAME: VALUE")]
    headers: Vec<String>,

    /// Custom user agent.
    #[arg(long = "user-agent", value_name = "UA")]
    user_agent: Option<String>,

    /// Print results as JSON (handy for scripts and other apps).
    #[arg(long)]
    json: bool,
}

impl RequestArgs {
    fn to_options(&self) -> Result<RequestOptions> {
        let mut o = RequestOptions {
            user_agent: self.user_agent.clone(),
            ..base_options()
        };
        for h in &self.headers {
            o.push_header_line(h)?;
        }
        Ok(o)
    }
}

/// Filtering switches shared by the commands that find media.
#[derive(Args, Clone, Default)]
struct FilterArgs {
    /// Turn off ad and tracker filtering.
    #[arg(long)]
    no_filter: bool,
    /// Ignore media smaller than this, like 1MB or 500KB.
    #[arg(long, value_name = "SIZE")]
    min_size: Option<String>,
    /// Ignore media bigger than this.
    #[arg(long, value_name = "SIZE")]
    max_size: Option<String>,
    /// Only these kinds: hls, dash, direct (comma separated).
    #[arg(long = "type", value_name = "KINDS", value_delimiter = ',')]
    kinds: Vec<String>,
    /// Only these extensions: mp4,mp3 (comma separated).
    #[arg(long = "ext", value_name = "EXTS", value_delimiter = ',')]
    extensions: Vec<String>,
    /// Ignore media from this host and its subdomains. Repeatable.
    #[arg(long = "block-host", value_name = "HOST")]
    blocked_hosts: Vec<String>,
    /// Ignore URLs containing this word. Repeatable.
    #[arg(long = "block-word", value_name = "WORD")]
    blocked_words: Vec<String>,
}

impl FilterArgs {
    fn rules(&self) -> Result<MediaRules> {
        let size = |s: &Option<String>| -> Result<Option<u64>> {
            s.as_deref()
                .map(|v| {
                    parse_size(v)
                        .with_context(|| format!("'{v}' is not a size. Try 500KB, 2MB or 1GB"))
                })
                .transpose()
        };
        let kinds = if self.kinds.is_empty() {
            None
        } else {
            let mut out = Vec::new();
            for k in &self.kinds {
                out.push(match k.trim().to_ascii_lowercase().as_str() {
                    "hls" | "m3u8" => MediaKind::Hls,
                    "dash" | "mpd" => MediaKind::Dash,
                    "direct" | "file" => MediaKind::Direct,
                    other => bail!("unknown type '{other}'. Use hls, dash or direct"),
                });
            }
            Some(out)
        };
        Ok(MediaRules {
            min_size: size(&self.min_size)?,
            max_size: size(&self.max_size)?,
            kinds,
            extensions: (!self.extensions.is_empty()).then(|| {
                self.extensions
                    .iter()
                    .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
                    .collect()
            }),
            blocked_hosts: self.blocked_hosts.clone(),
            blocked_words: self.blocked_words.clone(),
        })
    }

    fn load_filters(&self) -> Option<Filters> {
        if self.no_filter {
            return None;
        }
        Some(load_filters())
    }
}

fn load_filters() -> Filters {
    match Filters::load(&filter::home_dir()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("  could not load filter lists ({e}), using the built in set");
            Filters::builtin()
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Show what a URL is (file, HLS, DASH, page), its size and available qualities.
    Probe {
        url: String,
        #[command(flatten)]
        req: RequestArgs,
    },
    /// List every format yt-dlp finds for a page. Set OSPREY_YTDLP to choose the binary.
    Formats {
        url: String,
        /// Send yt-dlp's requests through this proxy.
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Download or update yt-dlp and Deno.
    Tools,
    /// List the media links in a web page, with ads and junk filtered out.
    Sniff {
        /// Page URL or a saved .html file.
        page: String,
        /// Ask the server for each link's size and real type (slower).
        #[arg(long)]
        probe: bool,
        /// Also list what the filters removed, and why.
        #[arg(long)]
        show_blocked: bool,
        #[command(flatten)]
        filt: FilterArgs,
        #[command(flatten)]
        req: RequestArgs,
    },
    /// Download a file or stream. Given a page or short link, finds the media first.
    Get {
        url: String,
        /// Output file, or a folder to save into.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Parallel connections (files) or parallel segments (streams).
        #[arg(short, long, default_value_t = 8)]
        connections: usize,
        /// Highest video height to pick from a stream, for example 720.
        #[arg(short, long, value_name = "HEIGHT")]
        quality: Option<u32>,
        /// When a page has several media links, download this one (number from the list).
        #[arg(long, value_name = "N")]
        pick: Option<usize>,
        #[command(flatten)]
        filt: FilterArgs,
        #[command(flatten)]
        req: RequestArgs,
    },
    /// Follow a short or wrapped link to its real destination, skipping redirect pages.
    Bypass {
        url: String,
        /// Give up after this many hops.
        #[arg(long, default_value_t = 12)]
        max_hops: usize,
        /// When the local rules get stuck, send the link to bypass.vip's paid
        /// API. Needs your key in the BYPASS_VIP_KEY environment variable.
        #[arg(long)]
        bypass_vip: bool,
        #[command(flatten)]
        req: RequestArgs,
    },
    /// Text-only view of a page: no images, GIFs, video or ads, just text and links.
    Text {
        /// Page URL or a saved .html file.
        page: String,
        /// Save to a file. .html gives a clean web page, anything else plain text.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Print HTML instead of plain text.
        #[arg(long)]
        html: bool,
        /// Drop images completely instead of showing [image: description] links.
        #[arg(long)]
        no_images: bool,
        /// Keep ads (turns off filtering).
        #[arg(long)]
        no_filter: bool,
        /// Line width for plain text.
        #[arg(long, default_value_t = 100)]
        width: usize,
        #[command(flatten)]
        req: RequestArgs,
    },
    /// Manage ad and tracker filter lists (uBlock Origin syntax).
    Filters {
        #[command(subcommand)]
        action: FilterCommand,
    },
    /// Anonymity and privacy: Tor, proxy, encrypted DNS, generic identity.
    Privacy {
        #[command(subcommand)]
        action: PrivacyCommand,
    },
    /// Convert a file. The target format comes from the output extension
    /// (mp4, mkv, webm, mp3, m4a, opus, flac, wav, gif).
    Convert {
        input: PathBuf,
        output: PathBuf,
        /// Copy streams without re-encoding (fast, lossless).
        #[arg(long)]
        copy: bool,
    },
}

#[derive(Subcommand)]
enum PrivacyCommand {
    /// Show the saved settings and what this run uses.
    Show,
    /// Ask outside services which IP address they see, to confirm the setup works.
    Check {
        /// Mask the IP address, for reports you share.
        #[arg(long)]
        hide_ip: bool,
    },
    /// Anonymous mode for every run: Tor plus a generic browser identity.
    On {
        /// Tor's SOCKS address, when it is not on 9050 or 9150.
        #[arg(long, value_name = "HOST:PORT")]
        tor_address: Option<String>,
    },
    /// Turn every privacy setting off.
    Off,
    /// Always use encrypted DNS: cloudflare (1.1.1.1), quad9, google, or system to stop.
    Dns { name: String },
    /// Always use a proxy, or "off" to stop.
    Proxy { url: String },
    /// Always use Tor, "on" or "off".
    Tor {
        state: String,
        #[arg(long, value_name = "HOST:PORT")]
        address: Option<String>,
    },
    /// Always send the generic browser identity, "on" or "off".
    Identity { state: String },
}

#[derive(Subcommand)]
enum FilterCommand {
    /// Download the latest EasyList and uBlock Origin lists.
    Update,
    /// Show which lists are installed and where your own rules file is.
    List,
    /// Test whether a URL would be blocked.
    Check {
        url: String,
        /// The page the request comes from.
        #[arg(long, default_value = "https://example.com/")]
        page: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let privacy = match resolve_privacy(&cli.privacy) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    };

    // The diagnostic log records the sites you visit, which defeats the
    // point of anonymous mode. Only an explicit --log writes one then.
    let env_log = osprey_core::env_var("LOG").map(PathBuf::from);
    match (&cli.log, env_log, is_anonymous(&privacy)) {
        (Some(path), _, true) => {
            eprintln!("  note: --log in anonymous mode records the sites you visit");
            diag::init(path);
        }
        (None, Some(_), true) => eprintln!("  anonymous mode: diagnostic log switched off"),
        (Some(path), _, false) => diag::init(path),
        (None, Some(path), false) => diag::init(&path),
        (None, None, _) => {}
    }

    match run(cli, privacy).await {
        Ok(()) => diag::line("finished OK"),
        Err(e) => {
            diag::line(&format!("ERROR: {e:#}"));
            for (i, cause) in e.chain().enumerate() {
                diag::line(&format!("  cause {i}: {cause}"));
            }
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}

/// Whether a command talks to the network, and so needs the privacy
/// route checked first (Tor running, proxy valid).
fn uses_network(cmd: &Command) -> bool {
    match cmd {
        Command::Convert { .. } => false,
        Command::Filters { action } => matches!(action, FilterCommand::Update),
        Command::Privacy { action } => matches!(action, PrivacyCommand::Check { .. }),
        Command::Text { page, .. } => page.contains("://") || !std::path::Path::new(page).exists(),
        _ => true,
    }
}

async fn run(cli: Cli, mut privacy: Privacy) -> Result<()> {
    if uses_network(&cli.command) {
        privacy.prepare().await?;
        if privacy != Privacy::default() {
            eprintln!("  privacy: {}", privacy.describe());
            diag::line(&format!("privacy: {}", privacy.describe()));
        }
        if privacy.route == Route::Direct {
            if let Some(var) = privacy_mod::env_proxy() {
                eprintln!(
                    "  note: the {var} setting on this computer sends traffic through a proxy, which also does the DNS lookups"
                );
            }
        }
    }
    let _ = PRIVACY.set(privacy);

    match cli.command {
        Command::Privacy { action } => cmd_privacy(action).await,
        Command::Probe { url, req } => cmd_probe(&url, &req).await,
        Command::Formats { url, proxy } => cmd_formats(&url, proxy).await,
        Command::Tools => cmd_tools().await,
        Command::Sniff {
            page,
            probe,
            show_blocked,
            filt,
            req,
        } => cmd_sniff(&page, probe, show_blocked, &filt, &req).await,
        Command::Get {
            url,
            output,
            connections,
            quality,
            pick,
            filt,
            req,
        } => cmd_get(&url, output, connections, quality, pick, &filt, &req).await,
        Command::Bypass {
            url,
            max_hops,
            bypass_vip,
            req,
        } => cmd_bypass(&url, max_hops, bypass_vip, &req).await,
        Command::Text {
            page,
            output,
            html,
            no_images,
            no_filter,
            width,
            req,
        } => cmd_text(&page, output, html, no_images, no_filter, width, &req).await,
        Command::Filters { action } => cmd_filters(action).await,
        Command::Convert {
            input,
            output,
            copy,
        } => {
            anyhow::ensure!(
                ffmpeg::is_available().await,
                "ffmpeg was not found. Install it or set OSPREY_FFMPEG"
            );
            let spinner = ProgressBar::new_spinner()
                .with_message(format!("converting to {}", output.display()));
            spinner.enable_steady_tick(Duration::from_millis(120));
            let result = ffmpeg::convert(&input, &output, copy).await;
            spinner.finish_and_clear();
            result?;
            println!("saved {}", output.display());
            Ok(())
        }
    }
}

async fn cmd_tools() -> Result<()> {
    let client = osprey_core::build_client(&base_options())?;
    let home = osprey_core::filter::home_dir();
    for r in osprey_core::tools::refresh(&home, &client, true).await {
        match r.result {
            Ok(step) => println!("{}: {step:?}", r.tool),
            Err(why) => println!("{}: failed, {why}", r.tool),
        }
    }
    println!("Folder: {}", osprey_core::tools::tools_dir(&home).display());
    Ok(())
}

async fn cmd_formats(url: &str, proxy: Option<String>) -> Result<()> {
    use osprey_core::ytdlp::{self, ExtractOptions, Outcome};
    let home = osprey_core::filter::home_dir();
    let binary = ytdlp::binary(&home);
    let opts = ExtractOptions {
        proxy,
        deno: osprey_core::tools::deno_path(&home),
        ..Default::default()
    };
    let found = match ytdlp::extract(&binary, url, &opts).await? {
        Outcome::Found(found) => found,
        Outcome::NotKnown(why) => {
            println!("Nothing from yt-dlp: {why}");
            return Ok(());
        }
    };
    println!(
        "{} ({})",
        found.title.as_deref().unwrap_or("No title"),
        found.extractor
    );
    for f in &found.formats {
        let track = if f.audio_only {
            "audio only".to_string()
        } else {
            f.height.map_or("?".to_string(), |h| format!("{h}p"))
        };
        let size = f.size.map_or("?".to_string(), |s| {
            let mark = if f.size_is_estimate { "about " } else { "" };
            format!("{mark}{} MB", s / 1_000_000)
        });
        println!("  {:<12} {:<10} {:?}  {size}", f.id, track, f.kind);
    }
    for s in &found.subtitles {
        println!("  subtitle {}", s.language);
    }
    if found.drm_formats > 0 {
        println!("  {} DRM formats left out", found.drm_formats);
    }
    Ok(())
}

async fn cmd_probe(url: &str, req: &RequestArgs) -> Result<()> {
    let client = build_client(&req.to_options()?)?;
    let info = probe(&client, url).await.context("probe failed")?;

    let mut variants = Vec::new();
    let mut audio = Vec::new();
    let mut segments = None;
    if info.kind == MediaKind::Hls {
        match hls::load(&client, &info.final_url).await?.0 {
            Playlist::Master(m) => {
                variants = m.variants.iter().map(|v| v.describe()).collect();
                audio = m
                    .audio
                    .iter()
                    .filter(|r| r.uri.is_some())
                    .map(|r| {
                        r.name
                            .clone()
                            .or(r.language.clone())
                            .unwrap_or_else(|| r.group_id.clone())
                    })
                    .collect();
            }
            Playlist::Media(p) => segments = Some((p.segments.len(), p.duration(), p.ended)),
        }
    }

    if req.json {
        println!(
            "{}",
            serde_json::json!({
                "info": info,
                "variants": variants,
                "audio_tracks": audio,
                "segments": segments.map(|(n, _, _)| n),
                "duration_seconds": segments.map(|(_, d, _)| d),
                "live": segments.map(|(_, _, ended)| !ended),
            })
        );
        return Ok(());
    }

    println!("type      {:?}", info.kind);
    if info.final_url != info.url {
        println!("final URL {}", info.final_url);
    }
    if let Some(ct) = &info.content_type {
        println!("mime      {ct}");
    }
    if let Some(s) = info.size {
        println!("size      {}", human_bytes(s));
    }
    if info.kind == MediaKind::Direct {
        println!(
            "resume    {}",
            if info.accepts_ranges {
                "yes, multi connection works"
            } else {
                "no, single connection only"
            }
        );
    }
    if let Some(name) = &info.filename {
        println!("name      {name}");
    }
    for (i, v) in variants.iter().enumerate() {
        println!("variant   [{i}] {v}");
    }
    for a in &audio {
        println!("audio     {a}");
    }
    if let Some((n, d, ended)) = segments {
        println!(
            "segments  {n} ({:.0}s{})",
            d,
            if ended { "" } else { ", live" }
        );
    }
    if info.kind == MediaKind::Page {
        println!("hint      this is a web page. Try `osprey sniff` to list its media or `osprey bypass` if it is a short link");
    }
    Ok(())
}

struct Candidate {
    media: FoundMedia,
    size: Option<u64>,
    blocked: Option<String>,
}

/// Finds and filters the media on a page. Shared by `sniff` and `get`.
async fn find_media(
    page: &str,
    do_probe: bool,
    filt: &FilterArgs,
    req: &RequestArgs,
) -> Result<(url::Url, Vec<Candidate>)> {
    let client = build_client(&req.to_options()?)?;
    let (base, body) = textmode::load_page(&client, page)
        .await
        .context("could not load the page")?;
    let found = scan_text(&body, &base);
    let rules = filt.rules()?;
    let filters = filt.load_filters();

    let mut out: Vec<Candidate> = found
        .into_iter()
        .map(|media| Candidate {
            media,
            size: None,
            blocked: None,
        })
        .collect();

    if do_probe || rules.needs_size() {
        let opts = req.to_options()?;
        let mut tasks = Vec::new();
        for (i, c) in out.iter().enumerate() {
            let url = c.media.url.clone();
            let opts = opts.clone();
            tasks.push(tokio::spawn(async move {
                let client = build_client(&opts).ok()?;
                let info = probe(&client, &url).await.ok()?;
                Some((i, info))
            }));
        }
        for t in tasks {
            if let Ok(Some((i, info))) = t.await {
                out[i].size = info.size;
                if info.kind != MediaKind::Page {
                    out[i].media.kind = info.kind;
                }
            }
        }
    }

    for c in &mut out {
        if let Some(f) = &filters {
            if f.is_blocked(&c.media.url, base.as_str(), RequestKind::Media) {
                c.blocked = Some("ad or tracker list".into());
                continue;
            }
        }
        if let Err(reason) = rules.check(&c.media.url, c.media.kind, c.size) {
            c.blocked = Some(reason);
        }
    }
    Ok((base, out))
}

async fn cmd_sniff(
    page: &str,
    do_probe: bool,
    show_blocked: bool,
    filt: &FilterArgs,
    req: &RequestArgs,
) -> Result<()> {
    let (_, found) = find_media(page, do_probe, filt, req).await?;
    let (kept, dropped): (Vec<_>, Vec<_>) = found.into_iter().partition(|c| c.blocked.is_none());

    if req.json {
        let list = |v: &[Candidate]| {
            v.iter()
                .map(|c| serde_json::json!({ "url": c.media.url, "kind": c.media.kind, "size": c.size, "blocked": c.blocked }))
                .collect::<Vec<_>>()
        };
        println!(
            "{}",
            serde_json::json!({ "media": list(&kept), "filtered": list(&dropped) })
        );
        return Ok(());
    }

    if kept.is_empty() {
        println!("no media links in the page source. The page may build them with JavaScript, which the upcoming browser mode will catch");
    }
    for (i, c) in kept.iter().enumerate() {
        let size = c.size.map(human_bytes).unwrap_or_default();
        println!(
            "[{}] {:<6} {:>9}  {}",
            i + 1,
            format!("{:?}", c.media.kind),
            size,
            c.media.url
        );
    }
    if !dropped.is_empty() {
        if show_blocked {
            println!("\nfiltered out:");
            for c in &dropped {
                println!(
                    "  {}  ({})",
                    c.media.url,
                    c.blocked.as_deref().unwrap_or("")
                );
            }
        } else {
            println!(
                "\n{} more hidden by filters (add --show-blocked to see them)",
                dropped.len()
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_get(
    url: &str,
    output: Option<PathBuf>,
    connections: usize,
    quality: Option<u32>,
    pick: Option<usize>,
    filt: &FilterArgs,
    req: &RequestArgs,
) -> Result<()> {
    let mut request = req.to_options()?;
    let client = build_client(&request)?;
    let info = probe(&client, url)
        .await
        .context("could not reach the link")?;

    let mut target = url.to_string();
    if info.kind == MediaKind::Page {
        eprintln!("  that is a web page, following it to find the media");
        let followed = bypass(&request, url, 12).await?;
        let mut page = followed.final_url.clone();
        let landed = probe(&client, &page)
            .await
            .context("could not open where the link leads")?;

        if landed.kind != MediaKind::Page {
            eprintln!("  link leads to {page}");
            target = page;
        } else {
            let (base, found) = find_media(&page, false, filt, req).await?;
            page = base.to_string();
            let kept: Vec<_> = found.into_iter().filter(|c| c.blocked.is_none()).collect();
            let chosen = match (kept.len(), pick) {
                (0, _) => {
                    let mut msg = format!("no media found on {page}");
                    if let Some(n) = followed.note {
                        msg.push_str(&format!(". {n}"));
                    }
                    bail!("{msg}. Try `osprey text` to read it or open it in a browser");
                }
                (_, Some(n)) if n >= 1 && n <= kept.len() => &kept[n - 1],
                (_, Some(n)) => bail!(
                    "--pick {n} is out of range, the page has {} media links",
                    kept.len()
                ),
                (1, None) => &kept[0],
                (_, None) => {
                    eprintln!("  found {} media links:", kept.len());
                    for (i, c) in kept.iter().enumerate() {
                        eprintln!("  [{}] {:?}  {}", i + 1, c.media.kind, c.media.url);
                    }
                    bail!("several media links found. Run again with --pick N to choose one");
                }
            };
            eprintln!(
                "  found {:?} media: {}",
                chosen.media.kind, chosen.media.url
            );
            diag::line(&format!("page {page} -> media {}", chosen.media.url));
            target = chosen.media.url.clone();
            // Many CDNs refuse media requests that do not come from the page.
            if !request
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("referer"))
            {
                request.headers.push(("Referer".into(), page.clone()));
            }
        }
    }

    let mut opts = FetchOptions {
        request,
        output,
        ..Default::default()
    };
    opts.direct.connections = connections.max(1);
    opts.hls.concurrency = connections.max(1);
    opts.hls.max_height = quality;
    let path = osprey_core::fetch(&target, &opts, progress_printer()).await?;
    if req.json {
        println!("{}", serde_json::json!({ "saved": path, "source": target }));
    } else {
        println!("saved {}", path.display());
    }
    Ok(())
}

async fn cmd_bypass(url: &str, max_hops: usize, vip: bool, req: &RequestArgs) -> Result<()> {
    use osprey_core::bypass::{resolve, Fallback, Step, BYPASS_VIP_ENDPOINT};
    let key = std::env::var("BYPASS_VIP_KEY").unwrap_or_default();
    if vip && !osprey_core::bypass::valid_bypass_vip_key(&key) {
        anyhow::bail!("--bypass-vip needs a valid key in BYPASS_VIP_KEY");
    }
    let fallback = vip.then_some(Fallback {
        endpoint: BYPASS_VIP_ENDPOINT,
        key: &key,
    });
    let resolved = resolve(&req.to_options()?, url, max_hops, fallback).await?;
    if req.json {
        println!("{}", serde_json::to_string_pretty(&resolved)?);
        return Ok(());
    }
    let result = &resolved.link;
    println!("start   {}", result.start);
    for hop in &result.hops {
        let how = match &hop.via {
            Step::Redirect { status } => format!("{status} redirect"),
            Step::UrlParameter { name } => format!("?{name}= link"),
            Step::Base64 => "base64 link".into(),
            Step::Fragment => "link after #".into(),
            Step::PathTarget => "link in path".into(),
            Step::MetaRefresh => "meta refresh".into(),
            Step::ScriptRedirect => "script jump".into(),
            Step::TrackingRemoved => "tracking removed".into(),
        };
        println!("  {how:<16} {}", hop.url);
    }
    println!("final   {}", result.final_url);
    println!("by      {}", resolved.resolved_by.label());
    if let Some(n) = &result.note {
        println!("note    {n}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_text(
    page: &str,
    output: Option<PathBuf>,
    html: bool,
    no_images: bool,
    no_filter: bool,
    width: usize,
    req: &RequestArgs,
) -> Result<()> {
    let client = build_client(&req.to_options()?)?;
    let (url, body) = textmode::load_page(&client, page)
        .await
        .context("could not load the page")?;
    let filters = (!no_filter).then(load_filters);
    let opts = TextModeOptions {
        image_links: !no_images,
        banner: true,
    };
    let simple = textmode::simplify(&body, &url, filters.as_ref(), &opts)?;

    match output {
        Some(path) => {
            let wants_html = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"));
            let content = if wants_html {
                simple
            } else {
                textmode::to_text(&simple, width)?
            };
            std::fs::write(&path, content)
                .with_context(|| format!("could not write {}", path.display()))?;
            println!("saved {}", path.display());
        }
        None if html => println!("{simple}"),
        None => println!("{}", textmode::to_text(&simple, width)?),
    }
    Ok(())
}

async fn cmd_filters(action: FilterCommand) -> Result<()> {
    let home = filter::home_dir();
    match action {
        FilterCommand::Update => {
            let client = build_client(&base_options())?;
            println!(
                "downloading filter lists into {}",
                filter::filters_dir(&home).display()
            );
            let report = filter::update_lists(&client, &home).await?;
            let mut failed = 0;
            for r in &report {
                println!(
                    "  {} {:<40} {}",
                    if r.ok { "ok  " } else { "FAIL" },
                    r.name,
                    r.detail
                );
                failed += usize::from(!r.ok);
            }
            if failed > 0 {
                bail!("{failed} list(s) could not be downloaded. Older copies, if any, are still in use");
            }
            Ok(())
        }
        FilterCommand::List => {
            let dir = filter::filters_dir(&home);
            println!("filter folder  {}", dir.display());
            for list in filter::DEFAULT_LISTS {
                let path = dir.join(format!("{}.txt", list.id));
                let state = match std::fs::metadata(&path) {
                    Ok(m) => format!("installed, {}", human_bytes(m.len())),
                    Err(_) => "not downloaded yet".into(),
                };
                println!("  {:<40} {state}", list.name);
            }
            let f = Filters::load(&home)?;
            println!(
                "your rules     {}",
                dir.join(filter::USER_RULES_FILE).display()
            );
            println!("active sources {}", f.sources.join(", "));
            println!("\nrun `osprey filters update` to download or refresh the lists");
            Ok(())
        }
        FilterCommand::Check { url, page } => {
            let f = load_filters();
            let blocked = [
                RequestKind::Media,
                RequestKind::Script,
                RequestKind::Image,
                RequestKind::Xhr,
            ]
            .into_iter()
            .filter(|k| f.is_blocked(&url, &page, *k))
            .map(|k| format!("{k:?}").to_lowercase())
            .collect::<Vec<_>>();
            if blocked.is_empty() {
                println!("allowed  {url}");
            } else {
                println!("blocked  {url}  (as {})", blocked.join(", "));
            }
            Ok(())
        }
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Turns engine progress events into terminal progress bars.
fn progress_printer() -> ProgressFn {
    let bar: Arc<Mutex<Option<ProgressBar>>> = Arc::new(Mutex::new(None));
    Arc::new(move |event| {
        match &event {
            Progress::Start { label, total, .. } => {
                diag::line(&format!("stage {label} (total {total:?})"))
            }
            Progress::Status(msg) => diag::line(msg),
            Progress::Finish => diag::line("stage finished"),
            Progress::Advance(_) | Progress::Bytes(_) => {}
        }
        let mut slot = bar.lock().expect("progress lock");
        match event {
            Progress::Start { label, total, unit } => {
                if let Some(old) = slot.take() {
                    old.finish_and_clear();
                }
                let pb = match total {
                    Some(t) => ProgressBar::new(t),
                    None => ProgressBar::new_spinner(),
                };
                let template = match (unit, total.is_some()) {
                    (Unit::Bytes, true) => {
                        "{msg:>12} [{bar:30}] {bytes}/{total_bytes} {bytes_per_sec} eta {eta}"
                    }
                    (Unit::Bytes, false) => "{msg:>12} {spinner} {bytes} {bytes_per_sec}",
                    (Unit::Segments, _) => "{msg:>12} [{bar:30}] {pos}/{len} segments eta {eta}",
                };
                pb.set_style(
                    ProgressStyle::with_template(template)
                        .expect("valid template")
                        .progress_chars("=> "),
                );
                pb.set_message(label);
                pb.enable_steady_tick(Duration::from_millis(200));
                *slot = Some(pb);
            }
            Progress::Advance(n) => {
                if let Some(pb) = slot.as_ref() {
                    pb.inc(n);
                }
            }
            // Stream bars count pieces. Bytes are only for the browser's panel.
            Progress::Bytes(_) => {}
            Progress::Status(msg) => match slot.as_ref() {
                Some(pb) => pb.println(format!("  {msg}")),
                None => eprintln!("  {msg}"),
            },
            Progress::Finish => {
                if let Some(pb) = slot.take() {
                    pb.finish();
                }
            }
        }
    })
}

fn on_off(state: &str) -> Result<bool> {
    match state.trim().to_ascii_lowercase().as_str() {
        "on" | "yes" | "true" | "1" => Ok(true),
        "off" | "no" | "false" | "0" => Ok(false),
        other => bail!("use on or off, not '{other}'"),
    }
}

async fn cmd_privacy(action: PrivacyCommand) -> Result<()> {
    let home = filter::home_dir();
    let mut settings = Settings::load(&home)?;
    let p = &mut settings.privacy;

    match action {
        PrivacyCommand::Show => {
            println!("settings file  {}", Settings::path(&home).display());
            println!("saved          {}", p.to_privacy()?.describe());
            println!("this run       {}", active_privacy().describe());
            return Ok(());
        }
        PrivacyCommand::Check { hide_ip } => return cmd_privacy_check(hide_ip).await,
        PrivacyCommand::On { tor_address } => {
            p.anonymous = true;
            p.tor = true;
            p.generic_identity = true;
            p.proxy = None;
            if tor_address.is_some() {
                p.tor_address = tor_address;
            }
        }
        PrivacyCommand::Off => *p = Default::default(),
        PrivacyCommand::Dns { name } => {
            let mode: osprey_core::DnsMode = name.parse()?;
            p.dns =
                (mode != osprey_core::DnsMode::System).then_some(name.trim().to_ascii_lowercase());
        }
        PrivacyCommand::Proxy { url } => {
            if url.eq_ignore_ascii_case("off") {
                p.proxy = None;
            } else {
                p.proxy = Some(privacy_mod::normalise_proxy(&url)?);
                p.tor = false;
                p.anonymous = false;
            }
        }
        PrivacyCommand::Tor { state, address } => {
            p.tor = on_off(&state)?;
            if !p.tor {
                p.anonymous = false;
            } else {
                p.proxy = None;
            }
            if address.is_some() {
                p.tor_address = address;
            }
        }
        PrivacyCommand::Identity { state } => {
            p.generic_identity = on_off(&state)?;
            if !p.generic_identity {
                p.anonymous = false;
            }
        }
    }

    let now = settings.privacy.to_privacy()?;
    settings.save(&home)?;
    println!("saved: {}", now.describe());
    if now.is_tor() {
        println!(
            "Tor Browser or the Tor service must be running. Check with: osprey privacy check"
        );
    }
    Ok(())
}

/// Keeps only the first part of an IP address: 203.0.113.9 becomes 203.x.x.x
fn mask_ip(ip: &str) -> String {
    if ip.contains(':') {
        format!("{}:x:x:x (hidden)", ip.split(':').next().unwrap_or(""))
    } else {
        format!("{}.x.x.x (hidden)", ip.split('.').next().unwrap_or(""))
    }
}

async fn cmd_privacy_check(hide_ip: bool) -> Result<()> {
    let p = active_privacy();
    let report = privacy_mod::check_exposure(&base_options()).await;
    println!("setup        {}", report.setup);
    if let Some(ip) = &report.public_ip {
        let country = report
            .country
            .as_deref()
            .map(|c| format!("  (country {c})"))
            .unwrap_or_default();
        let shown = if hide_ip { mask_ip(ip) } else { ip.clone() };
        println!("sites see    {shown}{country}");
    }
    if let Some(d) = &report.dns_test {
        println!("DNS test     {d}");
    }
    if let Some(t) = report.through_tor {
        println!("through Tor  {}", if t { "yes" } else { "NO" });
    }
    if report
        .cloudflare_warp
        .as_deref()
        .is_some_and(|w| w != "off")
    {
        println!("WARP         on (Cloudflare WARP VPN is active on this computer)");
    }
    if let Some(problem) = &report.problem {
        println!("problem      {problem}");
    }

    if report
        .dns_test
        .as_deref()
        .is_some_and(|d| d.starts_with("FAILED"))
    {
        bail!(
            "encrypted DNS is switched on but did not work, so lookups would fail. \
             A firewall, antivirus or network may be blocking {}. Try another server with --dns quad9",
            p.dns.describe()
        );
    }
    let verdict = match (&p.route, report.through_tor) {
        (Route::Tor(_), Some(true)) => {
            "Sites see a Tor exit address, not yours. Your ISP only sees that you use Tor."
        }
        (Route::Tor(_), Some(false)) => {
            bail!("traffic is NOT going through Tor. Do not rely on this setup")
        }
        (Route::Tor(_), None) => "Could not confirm Tor. Treat this setup as unverified.",
        (Route::Proxy(_), _) => {
            "Sites see the proxy's address. The proxy itself can see where you go, so only use one you trust."
        }
        (Route::Direct, _) if p.dns != osprey_core::DnsMode::System => {
            "Sites see your real address. DNS is encrypted, so your ISP cannot read the names you look up, \
             but it can still see which addresses you connect to. Add --tor to hide your address."
        }
        (Route::Direct, _) => {
            "Sites see your real address and your ISP can read the names you look up. \
             Try --dns cloudflare for encrypted DNS, or --anon for Tor."
        }
    };
    println!("verdict      {verdict}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_ip_addresses() {
        assert_eq!(mask_ip("203.0.113.9"), "203.x.x.x (hidden)");
        assert_eq!(mask_ip("2001:db8::1"), "2001:x:x:x (hidden)");
    }
}
