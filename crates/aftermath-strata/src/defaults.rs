//! /etc/aftermath/strata.d/user.toml: which stratum runs a command, as
//! chosen by hand with `amctl default`.
//!
//! The shipped /etc/aftermath/strata.toml stays untouched (and keeps its
//! comments); this small file only records the user's choices, and they
//! beat the shipped `[cross.pin]` entries:
//!
//! ```toml
//! # optional: replaces `[cross] order`
//! order = ["ubuntu", "arch"]
//!
//! [pin]
//! vim = "ubuntu"
//! dpkg = "arch"
//! ```
//!
//! Only exact command names, no globs: a default is about one command.
//! It lives in /etc, so it survives reboots on a persistent root (overlay
//! disk) and is lost on a pure RAM root, like state.toml.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::cross::valid_cmd_name;

pub const USER_DEFAULTS: &str = "/etc/aftermath/strata.d/user.toml";

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct UserDefaults {
    /// Lookup order for commands without a pin; empty keeps the shipped one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,

    /// Command name -> stratum.
    #[serde(default)]
    pub pin: BTreeMap<String, String>,
}

impl UserDefaults {
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// A missing file means no user defaults; a broken one is reported.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn render(&self) -> String {
        let body = toml::to_string(self).unwrap_or_default();

        format!(
            "# Written by `amctl default`. Your choices beat the shipped [cross.pin]\n\
             # entries in /etc/aftermath/strata.toml. Delete a line (or run\n\
             # `amctl default <cmd> --reset`) to go back to the shipped rules.\n\n{body}"
        )
    }

    /// Atomic write (tmp + rename); creates /etc/aftermath/strata.d.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }

        let tmp = path.with_extension("toml.new");
        std::fs::write(&tmp, self.render()).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// `amctl default <cmd> <stratum>`. The stratum must be in strata.toml.
    pub fn set_pin(&mut self, cfg: &Config, cmd: &str, stratum: &str) -> Result<(), String> {
        check_cmd(cmd)?;
        check_stratum(cfg, stratum)?;

        self.pin.insert(cmd.to_owned(), stratum.to_owned());
        Ok(())
    }

    /// `amctl default <cmd> --reset`. Returns the stratum it was set to.
    pub fn reset_pin(&mut self, cmd: &str) -> Result<String, String> {
        check_cmd(cmd)?;

        self.pin
            .remove(cmd)
            .ok_or_else(|| format!("`{cmd}` has no default of yours to reset"))
    }

    /// `amctl default order <stratum>...`: every name once, all known.
    pub fn set_order(&mut self, cfg: &Config, names: &[String]) -> Result<(), String> {
        if names.is_empty() {
            return Err("give the strata in the order you want, e.g. `ubuntu arch`".into());
        }

        for (i, name) in names.iter().enumerate() {
            check_stratum(cfg, name)?;
            if names[..i].contains(name) {
                return Err(format!("`{name}` is listed twice"));
            }
        }

        self.order = names.to_vec();
        Ok(())
    }
}

fn check_cmd(cmd: &str) -> Result<(), String> {
    if valid_cmd_name(cmd) {
        Ok(())
    } else {
        Err(format!("invalid command name `{cmd}`"))
    }
}

fn check_stratum(cfg: &Config, name: &str) -> Result<(), String> {
    if cfg.get(name).is_some() {
        Ok(())
    } else {
        Err(format!("no stratum `{name}` in /etc/aftermath/strata.toml"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::parse(
            r#"
[[stratum]]
name = "arch"
path = "/"
kind = "host"
enabled = true

[[stratum]]
name = "ubuntu"
path = "/aftermath/strata/ubuntu"
enabled = true
"#,
        )
        .unwrap()
    }

    #[test]
    fn roundtrip_keeps_order_and_pins() {
        let mut d = UserDefaults::default();
        d.set_pin(&cfg(), "vim", "ubuntu").unwrap();
        d.set_order(&cfg(), &["ubuntu".into(), "arch".into()])
            .unwrap();

        let back = UserDefaults::parse(&d.render()).unwrap();
        assert_eq!(back, d);
        assert_eq!(UserDefaults::parse("").unwrap(), UserDefaults::default());
    }

    #[test]
    fn set_and_reset() {
        let mut d = UserDefaults::default();
        assert!(
            d.set_pin(&cfg(), "vim", "debian").is_err(),
            "unknown stratum"
        );
        assert!(d.set_pin(&cfg(), "../x", "ubuntu").is_err(), "bad name");

        d.set_pin(&cfg(), "vim", "ubuntu").unwrap();
        assert_eq!(d.reset_pin("vim").unwrap(), "ubuntu");
        assert!(d.reset_pin("vim").is_err(), "nothing left to reset");
    }

    #[test]
    fn order_rejects_duplicates_and_unknowns() {
        let mut d = UserDefaults::default();
        assert!(d.set_order(&cfg(), &[]).is_err());
        assert!(d
            .set_order(&cfg(), &["arch".into(), "arch".into()])
            .is_err());
        assert!(d.set_order(&cfg(), &["fedora".into()]).is_err());
    }
}
