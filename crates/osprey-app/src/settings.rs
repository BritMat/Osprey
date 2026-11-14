//! Browser settings, saved as browser.json in the BM Osprey home folder.

use std::path::{Path, PathBuf};

use osprey_core::gateway::{Upstream, WARP_PROXY};
use osprey_core::privacy::DnsMode;
use serde::{Deserialize, Serialize};

/// How the browser reaches the internet. Ordered from fastest to most
/// anonymous.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum NetMode {
    /// Your normal connection.
    #[default]
    Direct,
    /// Normal connection, but site names are looked up over encrypted
    /// DNS, so your ISP cannot read them.
    SecureDns { provider: String },
    /// Cloudflare WARP in proxy mode. Fast and free. Sites see a
    /// Cloudflare address, your ISP sees only encrypted traffic to
    /// Cloudflare. Cloudflare itself can see where you go.
    Warp,
    /// Your VPN provider's proxy, or any SOCKS5 or HTTP proxy. As fast as
    /// the VPN. The provider can see where you go.
    Vpn { proxy: String },
    /// Tor. The strongest anonymity, and the slowest.
    Tor,
}

impl NetMode {
    /// True when sites see another address instead of yours. WebRTC is
    /// then switched off in every tab, since it could reveal your real one.
    pub fn hides_ip(&self) -> bool {
        matches!(self, Self::Warp | Self::Vpn { .. } | Self::Tor)
    }

    pub fn label(&self) -> String {
        match self {
            Self::Direct => "Direct".into(),
            Self::SecureDns { provider } => format!("Secure DNS · {provider}"),
            Self::Warp => "WARP".into(),
            Self::Vpn { .. } => "VPN".into(),
            Self::Tor => "Tor".into(),
        }
    }

    pub fn key(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::SecureDns { .. } => "secure_dns",
            Self::Warp => "warp",
            Self::Vpn { .. } => "vpn",
            Self::Tor => "tor",
        }
    }

    /// The gateway route for this mode. `tor_addr` is where Tor was
    /// found, if it was. When Tor is not running the usual Tor Browser
    /// port is used anyway, so connections fail instead of going direct.
    pub fn upstream(&self, tor_addr: Option<&str>) -> Result<Upstream, String> {
        Ok(match self {
            Self::Direct => Upstream::Direct {
                dns: DnsMode::System,
            },
            Self::SecureDns { provider } => Upstream::Direct {
                dns: provider.parse::<DnsMode>().map_err(|e| e.to_string())?,
            },
            Self::Warp => Upstream::Socks5 {
                addr: WARP_PROXY.into(),
                user: None,
                pass: None,
            },
            Self::Vpn { proxy } => Upstream::from_proxy_url(proxy).map_err(|e| e.to_string())?,
            Self::Tor => Upstream::Socks5 {
                addr: tor_addr.unwrap_or("127.0.0.1:9150").to_string(),
                user: None,
                pass: None,
            },
        })
    }
}

/// How downloads fetch media.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadStyle {
    /// Like the page's own player: two connections at a time with small
    /// random pauses. Hardest for a site to tell apart from watching.
    #[default]
    Natural,
    /// Six connections, no pauses. Much faster, and easier to spot.
    Fast,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub net: NetMode,
    /// Block ad and tracker domains, and hide ads in text-only mode.
    pub block_ads: bool,
    /// Where downloads go. Default: Downloads/BM Osprey.
    pub downloads_dir: Option<PathBuf>,
    /// Minutes before a background tab is put to sleep to save memory
    /// and CPU. 0 turns it off. Private tabs never sleep, because that
    /// would end their session.
    pub sleep_after_minutes: u64,
    /// Private mode: every new tab is a private tab.
    pub always_private: bool,
    pub download_style: DownloadStyle,
    /// In Tor mode, how many Tor circuits one download may use at once.
    /// More circuits are faster, since each has its own path.
    pub tor_circuits: u8,
    /// Speed limit for all downloads together, in KB/s. 0 means none.
    pub speed_limit_kbps: u32,
    /// Open the tabs from last time when the browser starts. Private tabs
    /// are never saved.
    pub restore_tabs: bool,
    /// Send links the local rules cannot resolve to bypass.vip's paid API.
    /// Needs `bypass_vip_key`. Never in private mode.
    pub bypass_vip: bool,
    /// The user's own bypass.vip key. Kept in this file as plain text, and
    /// never sent to the interface, which only learns that one is set.
    pub bypass_vip_key: Option<String>,
    /// Colour theme: sea, ember, aurora or slate.
    pub theme: String,
    /// No blur, glow or animation in the interface.
    pub reduce_effects: bool,
    /// Downloads that run at the same time.
    pub parallel_downloads: u8,
    /// Hardware acceleration in tabs. Only Windows uses it so far, and it
    /// takes effect at the next start.
    pub hardware_acceleration: bool,
    /// Disk cache limit for tabs in MB. 0 leaves it to the web engine.
    /// Windows only so far, at the next start.
    pub cache_mb: u32,
    /// Ids of filter lists that are switched off.
    pub lists_off: Vec<String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            net: NetMode::Direct,
            block_ads: true,
            downloads_dir: None,
            sleep_after_minutes: 20,
            always_private: false,
            download_style: DownloadStyle::Natural,
            tor_circuits: 3,
            speed_limit_kbps: 0,
            restore_tabs: true,
            bypass_vip: false,
            bypass_vip_key: None,
            theme: "sea".into(),
            reduce_effects: false,
            parallel_downloads: 3,
            hardware_acceleration: true,
            cache_mb: 0,
            lists_off: Vec::new(),
        }
    }
}

impl AppSettings {
    fn path(home: &Path) -> PathBuf {
        home.join("browser.json")
    }

    pub fn load(home: &Path) -> Self {
        std::fs::read_to_string(Self::path(home))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, home: &Path) {
        let _ = std::fs::create_dir_all(home);
        let tmp = home.join("browser.json.tmp");
        if let Ok(text) = serde_json::to_string_pretty(self) {
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, Self::path(home));
            }
        }
    }

    pub fn downloads_dir(&self) -> PathBuf {
        self.downloads_dir.clone().unwrap_or_else(|| {
            dirs::download_dir()
                .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Downloads"))
                .join("BM Osprey")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_map_to_routes() {
        assert_eq!(
            NetMode::Direct.upstream(None).unwrap(),
            Upstream::Direct {
                dns: DnsMode::System
            }
        );
        assert_eq!(
            NetMode::SecureDns {
                provider: "1.1.1.1".into()
            }
            .upstream(None)
            .unwrap(),
            Upstream::Direct {
                dns: DnsMode::Cloudflare
            }
        );
        assert!(
            matches!(NetMode::Warp.upstream(None).unwrap(), Upstream::Socks5 { addr, .. } if addr == "127.0.0.1:40000")
        );
        assert!(
            matches!(NetMode::Tor.upstream(Some("127.0.0.1:9050")).unwrap(), Upstream::Socks5 { addr, .. } if addr == "127.0.0.1:9050")
        );
        // Tor not found: still a Tor port, never direct.
        assert!(matches!(
            NetMode::Tor.upstream(None).unwrap(),
            Upstream::Socks5 { .. }
        ));
        assert!(NetMode::Vpn {
            proxy: "nonsense".into()
        }
        .upstream(None)
        .is_err());
    }

    #[test]
    fn settings_round_trip_through_json() {
        let s = AppSettings {
            net: NetMode::Vpn {
                proxy: "socks5://10.64.0.1:1080".into(),
            },
            ..Default::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        assert!(text.contains(r#""mode":"vpn""#));
        let back: AppSettings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.net, s.net);
        // Unknown or missing fields fall back to defaults.
        let partial: AppSettings = serde_json::from_str(r#"{"block_ads":false}"#).unwrap();
        assert_eq!(partial.net, NetMode::Direct);
        assert_eq!(partial.sleep_after_minutes, 20);
        assert_eq!(partial.download_style, DownloadStyle::Natural);
        assert_eq!(partial.tor_circuits, 3);
        assert!(!partial.always_private);
        assert!(NetMode::Tor.hides_ip() && NetMode::Warp.hides_ip());
        assert!(!NetMode::SecureDns {
            provider: "quad9".into()
        }
        .hides_ip());
    }
}
