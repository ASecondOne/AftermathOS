//! /etc/aftermath/state.toml: on/off switches changed at runtime by
//! `amctl strata enable|disable` and `amctl services enable|disable`.
//!
//! The shipped config files (strata.toml, services/*.toml) stay untouched
//! and keep their comments; this small file only records overrides:
//!
//! ```toml
//! [strata]
//! ubuntu = false
//!
//! [services]
//! heartbeat = true
//! ```
//!
//! It lives in /etc, so it survives reboots when the host overlay disk is
//! in use (persistent root) and is lost on a pure RAM root.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const STATE: &str = "/etc/aftermath/state.toml";

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct State {
    #[serde(default)]
    pub strata: BTreeMap<String, bool>,
    #[serde(default)]
    pub services: BTreeMap<String, bool>,
}

impl State {
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// A missing file is an empty state; a broken one is reported.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(t) => Self::parse(&t).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn render(&self) -> String {
        let body = toml::to_string(self).unwrap_or_default();
        format!(
            "# Written by amctl (enable/disable). Overrides `enabled` in\n\
             # /etc/aftermath/strata.toml and /etc/aftermath/services/*.toml.\n\
             # Delete a line to go back to the shipped default.\n\n{body}"
        )
    }

    /// Atomic write (tmp + rename).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let tmp = path.with_extension("toml.new");
        std::fs::write(&tmp, self.render()).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut s = State::default();
        s.strata.insert("ubuntu".into(), false);
        s.services.insert("heartbeat".into(), true);
        let back = State::parse(&s.render()).unwrap();
        assert_eq!(back, s);
        assert_eq!(State::parse("").unwrap(), State::default());
    }
}
