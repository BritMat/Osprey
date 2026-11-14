//! Saved settings, kept in `settings.json` in the BM Osprey home folder
//! (see [`crate::filter::home_dir`]). Command line flags override them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::privacy::{DnsMode, Privacy, Route};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub privacy: PrivacySettings,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrivacySettings {
    /// Everything on: Tor plus the generic identity.
    pub anonymous: bool,
    pub tor: bool,
    /// Tor's SOCKS address when it is not on 9050 or 9150.
    pub tor_address: Option<String>,
    pub proxy: Option<String>,
    /// cloudflare, quad9, google or system.
    pub dns: Option<String>,
    pub generic_identity: bool,
    /// Tor circuits per download (1 to 8). Unset means 3.
    pub tor_circuits: Option<u8>,
}

impl PrivacySettings {
    pub fn to_privacy(&self) -> Result<Privacy> {
        let dns = match &self.dns {
            Some(d) => d.parse::<DnsMode>()?,
            None => DnsMode::System,
        };
        if self.anonymous {
            let mut p = Privacy::anonymous();
            p.route = Route::Tor(self.tor_address.clone());
            p.circuits = self.tor_circuits.unwrap_or(3);
            return Ok(p);
        }
        let route = if self.tor {
            Route::Tor(self.tor_address.clone())
        } else if let Some(proxy) = &self.proxy {
            Route::Proxy(proxy.clone())
        } else {
            Route::Direct
        };
        let circuits = if matches!(route, Route::Tor(_)) {
            self.tor_circuits.unwrap_or(3)
        } else {
            0
        };
        Ok(Privacy {
            route,
            dns,
            generic_identity: self.generic_identity,
            circuits,
        })
    }
}

impl Settings {
    pub fn path(home: &Path) -> PathBuf {
        home.join("settings.json")
    }

    /// Loads settings. A missing file means defaults. A damaged file is an
    /// error, so a broken privacy setting is never silently ignored.
    pub fn load(home: &Path) -> Result<Self> {
        let path = Self::path(home);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                Error::Unsupported(format!(
                    "{} is damaged ({e}). Fix or delete it",
                    path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        std::fs::create_dir_all(home)?;
        let path = Self::path(home);
        let tmp = home.join("settings.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_conversion() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Settings::load(dir.path()).unwrap(), Settings::default());

        let mut s = Settings::default();
        s.privacy.dns = Some("1.1.1.1".into());
        s.save(dir.path()).unwrap();
        let loaded = Settings::load(dir.path()).unwrap();
        assert_eq!(loaded, s);
        let p = loaded.privacy.to_privacy().unwrap();
        assert_eq!(
            (p.route, p.dns, p.generic_identity),
            (Route::Direct, DnsMode::Cloudflare, false)
        );

        s.privacy.anonymous = true;
        s.privacy.tor_address = Some("127.0.0.1:9150".into());
        let p = s.privacy.to_privacy().unwrap();
        assert_eq!(p.route, Route::Tor(Some("127.0.0.1:9150".into())));
        assert!(p.generic_identity);
    }

    #[test]
    fn damaged_file_is_an_error_not_a_silent_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(Settings::path(dir.path()), "{ not json").unwrap();
        assert!(Settings::load(dir.path()).is_err());
    }
}
