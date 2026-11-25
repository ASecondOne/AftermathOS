//! /etc/aftermath/strata.toml, plus the runtime switches (state.toml) and
//! the user's command defaults (strata.d/user.toml) layered on top.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Deserialize, Default, Debug)]
pub struct Config {
    #[serde(default)]
    pub stratum: Vec<Stratum>,
    #[serde(default)]
    pub cross: Cross,
    /// The user's defaults from strata.d/user.toml (not part of strata.toml).
    #[serde(skip)]
    pub user: crate::defaults::UserDefaults,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Stratum {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Deserialize, Default, Debug)]
pub struct Cross {
    /// Lookup order for commands without a pin. Default: host first, then
    /// the other enabled strata in file order.
    #[serde(default)]
    pub order: Vec<String>,
    /// command (glob: `*`, `?`) -> stratum, e.g. `"apt-*" = "ubuntu"`.
    #[serde(default)]
    pub pin: BTreeMap<String, String>,
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// `load` plus the runtime on/off switches from state.toml
    /// (`amctl strata enable|disable`). A broken state file is ignored with
    /// a warning on stderr: the shipped config always wins over nothing.
    pub fn load_with_state(
        path: &std::path::Path,
        state: &std::path::Path,
    ) -> Result<Self, String> {
        let mut cfg = Self::load(path)?;
        match crate::state::State::load(state) {
            Ok(st) => cfg.apply_state(&st),
            Err(e) => eprintln!("aftermath: ignoring {e}"),
        }
        Ok(cfg)
    }

    /// Everything that decides who runs what, as the running system sees
    /// it: strata.toml, state.toml and strata.d/user.toml. A broken
    /// user.toml is ignored with a warning (plan B: the shipped rules).
    pub fn load_system() -> Result<Self, String> {
        use crate::defaults::{UserDefaults, USER_DEFAULTS};

        let mut cfg =
            Self::load_with_state(&crate::at(crate::CONFIG), &crate::at(crate::state::STATE))?;

        match UserDefaults::load(&crate::at(USER_DEFAULTS)) {
            Ok(user) => cfg.user = user,
            Err(e) => eprintln!("aftermath: ignoring {e}"),
        }
        Ok(cfg)
    }

    pub fn apply_state(&mut self, st: &crate::state::State) {
        for s in &mut self.stratum {
            if s.kind == "host" {
                continue; // the host can't be switched off
            }
            if let Some(on) = st.strata.get(&s.name) {
                s.enabled = *on;
            }
        }
    }

    /// The host stratum (the booted Arch root, `kind = "host"`).
    pub fn host(&self) -> Option<&Stratum> {
        self.stratum.iter().find(|s| s.kind == "host")
    }

    pub fn host_name(&self) -> &str {
        self.host().map(|s| s.name.as_str()).unwrap_or("arch")
    }

    pub fn get(&self, name: &str) -> Option<&Stratum> {
        self.stratum.iter().find(|s| s.name == name)
    }

    /// Enabled strata that are not the host.
    pub fn guests(&self) -> impl Iterator<Item = &Stratum> {
        self.stratum
            .iter()
            .filter(|s| s.enabled && s.kind != "host")
    }

    /// Effective lookup order (only enabled strata). The user's order
    /// (`amctl default order`) comes first; enabled strata it leaves out
    /// keep their usual place after it, so none is lost by accident.
    pub fn order(&self) -> Vec<String> {
        let enabled = |n: &String| self.stratum.iter().any(|s| &s.name == n && s.enabled);

        let shipped: Vec<String> = if self.cross.order.is_empty() {
            let mut v = vec![self.host_name().to_owned()];
            v.extend(self.guests().map(|s| s.name.clone()));
            v
        } else {
            self.cross
                .order
                .iter()
                .filter(|n| enabled(n))
                .cloned()
                .collect()
        };

        let mut order: Vec<String> = self
            .user
            .order
            .iter()
            .filter(|n| enabled(n))
            .cloned()
            .collect();
        for name in shipped {
            if !order.contains(&name) {
                order.push(name);
            }
        }
        order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = r#"
[[stratum]]
name = "arch"
path = "/"
kind = "host"
enabled = true

[[stratum]]
name = "ubuntu"
path = "/aftermath/strata/ubuntu"
kind = "debian"
enabled = true

[[stratum]]
name = "windows"
path = "/aftermath/strata/windows"
enabled = false

[cross.pin]
"apt*" = "ubuntu"
pacman = "arch"
"#;

    #[test]
    fn parses_and_orders() {
        let c = Config::parse(CFG).unwrap();
        assert_eq!(c.host_name(), "arch");
        assert_eq!(c.order(), vec!["arch", "ubuntu"]);
        assert_eq!(c.guests().count(), 1);
        assert_eq!(c.cross.pin.get("apt*").map(String::as_str), Some("ubuntu"));
    }

    #[test]
    fn state_overrides_enabled_but_not_host() {
        let mut c = Config::parse(CFG).unwrap();
        let mut st = crate::state::State::default();
        st.strata.insert("ubuntu".into(), false);
        st.strata.insert("windows".into(), true);
        st.strata.insert("arch".into(), false);
        c.apply_state(&st);
        assert!(!c.get("ubuntu").unwrap().enabled);
        assert!(c.get("windows").unwrap().enabled);
        assert!(c.get("arch").unwrap().enabled);
    }

    #[test]
    fn user_order_comes_first_and_keeps_the_rest() {
        let mut c = Config::parse(CFG).unwrap();
        c.user.order = vec!["ubuntu".into(), "windows".into()];
        assert_eq!(c.order(), vec!["ubuntu", "arch"]);
    }

    #[test]
    fn explicit_order_skips_disabled() {
        let mut c = Config::parse(CFG).unwrap();
        c.cross.order = vec!["ubuntu".into(), "windows".into(), "arch".into()];
        assert_eq!(c.order(), vec!["ubuntu", "arch"]);
    }
}
