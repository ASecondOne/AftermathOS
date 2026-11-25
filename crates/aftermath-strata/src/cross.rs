//! Cross-stratum command resolution, the launcher index, and `strat`'s
//! argv dispatch.
//!
//! Layout (generated at boot by `aftermath-strata setup`):
//! * `/aftermath/cross/pin/<cmd>`  launchers that must win over the host's own
//!   command (a pin says e.g. `apt` belongs to ubuntu); first in PATH.
//! * `/aftermath/cross/bin/<cmd>`  launchers for commands the host doesn't
//!   have; last in PATH, so the host always wins otherwise.
//! * `/aftermath/cross/index`      `<cmd> <stratum>` per line.
//!
//! Every launcher is a symlink to the `strat` multicall binary, which looks
//! its own name up in the index.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;

/// Shell-style glob with `*` and `?` (no character classes).
pub fn glob(pattern: &str, name: &str) -> bool {
    fn rec(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => rec(&p[1..], n) || (!n.is_empty() && rec(p, &n[1..])),
            (Some(b'?'), Some(_)) => rec(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => rec(&p[1..], &n[1..]),
            _ => false,
        }
    }
    rec(pattern.as_bytes(), name.as_bytes())
}

/// Which commands each stratum provides: stratum -> command names.
pub type Providers = BTreeMap<String, BTreeSet<String>>;

/// Why a stratum runs a command (shown by `amctl which`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The user chose it: `amctl default <cmd> <stratum>`.
    UserDefault,
    /// A `[cross.pin]` entry in the shipped strata.toml (the pattern).
    ShippedPin(String),
    /// Nothing pinned: the first stratum in the lookup order that has it.
    Order,
    /// Nothing pinned and the host has it (the host comes first).
    Host,
}

impl Reason {
    /// Short machine-readable key for JSON: user, pin, order, host.
    pub fn key(&self) -> &'static str {
        match self {
            Reason::UserDefault => "user",
            Reason::ShippedPin(_) => "pin",
            Reason::Order => "order",
            Reason::Host => "host",
        }
    }

    /// One line for humans.
    pub fn describe(&self, cmd: &str) -> String {
        match self {
            Reason::UserDefault => format!("your default (amctl default {cmd} ...)"),
            Reason::ShippedPin(pattern) => format!("shipped pin `{pattern}` in strata.toml"),
            Reason::Order => "first in the lookup order that has it".into(),
            Reason::Host => "the host wins when nothing is pinned".into(),
        }
    }
}

/// The winning stratum and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub stratum: String,
    pub reason: Reason,
}

/// A rule that matched `cmd` but was passed over: its stratum lacks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passed {
    pub stratum: String,
    pub reason: Reason,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    pub choice: Option<Choice>,
    pub passed_over: Vec<Passed>,
}

/// The stratum that should run `cmd`, and why. First match wins:
/// 1. the user's default (`/etc/aftermath/strata.d/user.toml`),
/// 2. the most specific shipped pin (exact names beat globs, longer globs
///    beat shorter ones),
/// 3. the first stratum in the lookup order.
///
/// A default or pin whose stratum doesn't have the command is passed over
/// (and reported), so a stale choice never makes a command disappear.
pub fn explain(cfg: &Config, providers: &Providers, cmd: &str) -> Resolution {
    let has = |s: &str| providers.get(s).is_some_and(|set| set.contains(cmd));
    let mut passed_over = Vec::new();

    for (stratum, reason) in pinned(cfg, cmd) {
        if has(&stratum) {
            let choice = Some(Choice { stratum, reason });
            return Resolution {
                choice,
                passed_over,
            };
        }
        passed_over.push(Passed { stratum, reason });
    }

    let choice = cfg.order().into_iter().find(|s| has(s)).map(|stratum| {
        let reason = if stratum == cfg.host_name() {
            Reason::Host
        } else {
            Reason::Order
        };
        Choice { stratum, reason }
    });

    Resolution {
        choice,
        passed_over,
    }
}

/// The pins that apply to `cmd`, strongest first: the user's, then the
/// shipped ones by specificity.
fn pinned(cfg: &Config, cmd: &str) -> Vec<(String, Reason)> {
    let mut out = Vec::new();

    if let Some(stratum) = cfg.user.pin.get(cmd) {
        out.push((stratum.clone(), Reason::UserDefault));
    }

    let mut shipped: Vec<(&String, &String)> = cfg
        .cross
        .pin
        .iter()
        .filter(|(pattern, _)| glob(pattern, cmd))
        .collect();

    shipped.sort_by_key(|(pattern, _)| {
        let exact = !pattern.contains(['*', '?']);
        (std::cmp::Reverse(exact), std::cmp::Reverse(pattern.len()))
    });

    for (pattern, stratum) in shipped {
        out.push((stratum.clone(), Reason::ShippedPin(pattern.clone())));
    }
    out
}

/// The stratum that should run `cmd` (see [`explain`]).
pub fn resolve(cfg: &Config, providers: &Providers, cmd: &str) -> Option<String> {
    explain(cfg, providers, cmd).choice.map(|c| c.stratum)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// Shadows a host command: `/aftermath/cross/pin` (front of PATH).
    Pin,
    /// Command the host doesn't have: `/aftermath/cross/bin` (end of PATH).
    Bin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launcher {
    pub cmd: String,
    pub stratum: String,
    pub slot: Slot,
}

/// Every launcher to generate. Commands owned by the host need none.
pub fn plan(cfg: &Config, providers: &Providers) -> Vec<Launcher> {
    let host = cfg.host_name();
    let host_cmds = providers.get(host);
    let all: BTreeSet<&String> = providers.values().flatten().collect();
    all.into_iter()
        .filter_map(|cmd| {
            let owner = resolve(cfg, providers, cmd)?;
            if owner == host {
                return None;
            }
            let slot = if host_cmds.is_some_and(|h| h.contains(cmd)) {
                Slot::Pin
            } else {
                Slot::Bin
            };
            Some(Launcher {
                cmd: cmd.clone(),
                stratum: owner,
                slot,
            })
        })
        .collect()
}

/// Index file: one `<cmd> <stratum>` per line.
pub fn write_index(launchers: &[Launcher]) -> String {
    launchers
        .iter()
        .map(|l| format!("{} {}\n", l.cmd, l.stratum))
        .collect()
}

pub fn lookup_index(index: &str, cmd: &str) -> Option<String> {
    index.lines().find_map(|l| {
        let (c, s) = l.split_once(' ')?;
        (c == cmd).then(|| s.trim().to_owned())
    })
}

/// A command name is safe to use as a launcher file name.
pub fn valid_cmd_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && name.bytes().all(|b| b.is_ascii_graphic())
}

/// What `strat` should do, decided from argv alone.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// `strat` / `strat --help`
    Usage,
    /// `strat --list`
    List,
    /// `strat --which <cmd>`
    Which(String),
    /// `strat --which -a <cmd>`: every copy, and which one wins
    WhichAll(String),
    /// `strat <stratum> <cmd> [args...]`
    Run {
        stratum: String,
        cmd: String,
        args: Vec<String>,
    },
    /// Called through a launcher symlink: run `argv0` in its owning stratum.
    Launcher { cmd: String, args: Vec<String> },
}

pub fn decide(argv: &[String]) -> Action {
    let argv0 = argv.first().map(String::as_str).unwrap_or("strat");
    let base = argv0.rsplit('/').next().unwrap_or(argv0);
    let rest = argv.get(1..).unwrap_or(&[]);
    if base != "strat" {
        return Action::Launcher {
            cmd: base.to_owned(),
            args: rest.to_vec(),
        };
    }
    match rest {
        [] => Action::Usage,
        [h, ..] if h == "-h" || h == "--help" => Action::Usage,
        [l] if l == "--list" || l == "-l" => Action::List,
        [w, cmd] if is_which(w) => Action::Which(cmd.clone()),
        [w, a, cmd] if is_which(w) && is_all(a) => Action::WhichAll(cmd.clone()),
        [a, w, cmd] if is_all(a) && is_which(w) => Action::WhichAll(cmd.clone()),
        [stratum, cmd, args @ ..] => Action::Run {
            stratum: stratum.clone(),
            cmd: cmd.clone(),
            args: args.to_vec(),
        },
        [_] => Action::Usage,
    }
}

fn is_which(arg: &str) -> bool {
    arg == "--which" || arg == "-w"
}

fn is_all(arg: &str) -> bool {
    arg == "-a" || arg == "--all"
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
[cross.pin]
"apt*" = "ubuntu"
"apt-key" = "arch"
dpkg = "ubuntu"
pacman = "ubuntu"
"#,
        )
        .unwrap()
    }

    fn providers() -> Providers {
        let mut p = Providers::new();
        p.insert(
            "arch".into(),
            ["ls", "bash", "pacman", "dpkg", "makepkg"]
                .map(String::from)
                .into(),
        );
        p.insert(
            "ubuntu".into(),
            [
                "ls",
                "bash",
                "apt",
                "apt-get",
                "apt-key",
                "dpkg",
                "add-apt-repository",
            ]
            .map(String::from)
            .into(),
        );
        p
    }

    #[test]
    fn globs() {
        assert!(glob("apt*", "apt"));
        assert!(glob("apt*", "apt-get"));
        assert!(glob("dpkg-?", "dpkg-x"));
        assert!(!glob("dpkg-?", "dpkg-xy"));
        assert!(!glob("apt*", "xapt"));
        assert!(glob("*", ""));
    }

    #[test]
    fn host_first_then_pins() {
        let (c, p) = (cfg(), providers());
        assert_eq!(resolve(&c, &p, "ls").as_deref(), Some("arch"));
        assert_eq!(resolve(&c, &p, "apt-get").as_deref(), Some("ubuntu"));
        assert_eq!(
            resolve(&c, &p, "add-apt-repository").as_deref(),
            Some("ubuntu")
        );
        // Pinned to ubuntu and both have it -> ubuntu, even over the host.
        assert_eq!(resolve(&c, &p, "dpkg").as_deref(), Some("ubuntu"));
        // Pin to a stratum that lacks the command falls back to order.
        assert_eq!(resolve(&c, &p, "pacman").as_deref(), Some("arch"));
        // Exact pin beats the glob: apt-key -> arch, but arch lacks it -> ubuntu via order.
        assert_eq!(resolve(&c, &p, "apt-key").as_deref(), Some("ubuntu"));
        assert_eq!(resolve(&c, &p, "nope"), None);
    }

    #[test]
    fn reasons() {
        let (c, p) = (cfg(), providers());
        let why = |cmd: &str| {
            explain(&c, &p, cmd)
                .choice
                .map(|ch| (ch.stratum, ch.reason))
        };

        assert_eq!(why("ls"), Some(("arch".into(), Reason::Host)));
        assert_eq!(
            why("dpkg"),
            Some(("ubuntu".into(), Reason::ShippedPin("dpkg".into())))
        );
        assert_eq!(
            why("add-apt-repository"),
            Some(("ubuntu".into(), Reason::Order))
        );

        // The exact `apt-key = "arch"` pin is passed over (arch lacks it).
        let r = explain(&c, &p, "apt-key");
        assert_eq!(r.passed_over[0].stratum, "arch");
        assert_eq!(r.choice.unwrap().reason, Reason::ShippedPin("apt*".into()));
    }

    #[test]
    fn user_default_beats_shipped_pin_and_order() {
        let (mut c, p) = (cfg(), providers());
        c.user.pin.insert("ls".into(), "ubuntu".into());
        c.user.pin.insert("dpkg".into(), "arch".into());

        let ls = explain(&c, &p, "ls").choice.unwrap();
        assert_eq!(
            (ls.stratum.as_str(), ls.reason),
            ("ubuntu", Reason::UserDefault)
        );

        // Back to the host, against the shipped `dpkg = "ubuntu"` pin.
        let dpkg = explain(&c, &p, "dpkg").choice.unwrap();
        assert_eq!(
            (dpkg.stratum.as_str(), dpkg.reason),
            ("arch", Reason::UserDefault)
        );

        let launchers = plan(&c, &p);
        let ls_launcher = launchers.iter().find(|l| l.cmd == "ls").unwrap();
        assert_eq!(ls_launcher.slot, Slot::Pin, "shadows the host's ls");
        assert!(
            launchers.iter().all(|l| l.cmd != "dpkg"),
            "host runs dpkg itself"
        );
    }

    #[test]
    fn a_default_to_a_stratum_without_the_command_is_passed_over() {
        let (mut c, p) = (cfg(), providers());
        c.user.pin.insert("makepkg".into(), "ubuntu".into());

        let r = explain(&c, &p, "makepkg");
        assert_eq!(r.choice.unwrap().stratum, "arch");
        assert_eq!(r.passed_over.len(), 1);
        assert_eq!(r.passed_over[0].reason, Reason::UserDefault);
    }

    #[test]
    fn plan_slots() {
        let plan = plan(&cfg(), &providers());
        let find = |n: &str| plan.iter().find(|l| l.cmd == n).cloned();
        assert_eq!(find("ls"), None, "host commands need no launcher");
        assert_eq!(find("makepkg"), None);
        assert_eq!(find("apt").unwrap().slot, Slot::Bin);
        assert_eq!(
            find("dpkg").unwrap().slot,
            Slot::Pin,
            "shadows the host's dpkg"
        );
        let idx = write_index(&plan);
        assert_eq!(lookup_index(&idx, "apt-get").as_deref(), Some("ubuntu"));
        assert_eq!(lookup_index(&idx, "ls"), None);
    }

    #[test]
    fn dispatch() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(decide(&v(&["strat"])), Action::Usage);
        assert_eq!(decide(&v(&["/usr/bin/strat", "--list"])), Action::List);
        assert_eq!(
            decide(&v(&["strat", "--which", "apt"])),
            Action::Which("apt".into())
        );
        assert_eq!(
            decide(&v(&["strat", "ubuntu", "apt", "install", "foo"])),
            Action::Run {
                stratum: "ubuntu".into(),
                cmd: "apt".into(),
                args: v(&["install", "foo"])
            }
        );
        assert_eq!(
            decide(&v(&["/aftermath/cross/bin/apt-get", "update"])),
            Action::Launcher {
                cmd: "apt-get".into(),
                args: v(&["update"])
            }
        );
        assert_eq!(
            decide(&v(&["strat", "--which", "-a", "vi"])),
            Action::WhichAll("vi".into())
        );
        assert_eq!(
            decide(&v(&["strat", "-a", "--which", "vi"])),
            Action::WhichAll("vi".into())
        );
        assert_eq!(decide(&v(&["strat", "ubuntu"])), Action::Usage);
    }

    #[test]
    fn launcher_names() {
        assert!(valid_cmd_name("apt-get"));
        assert!(valid_cmd_name("["));
        assert!(!valid_cmd_name(".."));
        assert!(!valid_cmd_name("a b"));
    }
}
