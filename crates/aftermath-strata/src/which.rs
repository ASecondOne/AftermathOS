//! Every copy of a command across the running strata, and which one wins:
//! `strat --which -a <cmd>` and `amctl which <cmd>`.
//!
//! Only files are read (strata.toml, the status file, the launcher index
//! and the strata's bin directories), so this works without the engine.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::cross::{explain, lookup_index, Providers, Resolution};
use crate::{at, BIN_DIRS};

/// One copy of a command inside one stratum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub stratum: String,
    /// Where it sits in the stratum's PATH, e.g. `/usr/bin/vi`.
    pub path: String,
    /// Symlinks resolved inside the stratum, e.g. `/usr/bin/vim.basic`.
    pub real: String,
    /// The same file as the host sees it, e.g.
    /// `/aftermath/strata/ubuntu/usr/bin/vim.basic`.
    pub file: PathBuf,
    /// The copy this stratum's PATH finds first (others are shadowed).
    pub first: bool,
}

/// A stratum whose files were not looked at, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub stratum: String,
    pub why: String,
}

/// Everything `which` shows for one command.
#[derive(Debug, Clone)]
pub struct Report {
    pub cmd: String,
    pub copies: Vec<Found>,
    pub resolution: Resolution,
    /// What the launcher index says right now (None: no launcher, the
    /// host's PATH finds it directly).
    pub launcher: Option<String>,
    pub not_checked: Vec<Skipped>,
}

impl Report {
    /// The stratum that actually runs `cmd` now.
    pub fn runs_in(&self, cfg: &Config) -> Option<String> {
        if let Some(stratum) = &self.launcher {
            return Some(stratum.clone());
        }

        let host = cfg.host_name();
        let host_has_it = self.copies.iter().any(|c| c.stratum == host);
        host_has_it.then(|| host.to_owned())
    }

    /// True when the launchers don't match the rules yet (run
    /// `aftermath-strata refresh`, which `amctl default` does itself).
    pub fn stale(&self, cfg: &Config) -> bool {
        let wanted = self.resolution.choice.as_ref().map(|c| c.stratum.clone());
        wanted != self.runs_in(cfg)
    }
}

/// Build the report. `up` are the guest strata that are running.
///
/// What counts is what runs *now*: a stratum started at runtime but not
/// (yet) enabled at boot is searched too, exactly like the launchers do
/// (`Config::with_running`).
pub fn report(cfg: &Config, up: &[String], index: &str, cmd: &str) -> Report {
    let live = cfg.with_running(up);
    let mut copies = Vec::new();
    let mut not_checked = Vec::new();

    for s in &cfg.stratum {
        let running = s.kind == "host" || up.contains(&s.name);

        if !running {
            let why = if s.enabled { "not running" } else { "disabled" };
            not_checked.push(skipped(&s.name, why));
            continue;
        }

        copies.extend(copies_in(&s.name, &at(&s.path), cmd));
    }

    // explain() only needs to know who has this one command.
    let mut providers = Providers::new();
    for c in &copies {
        providers
            .entry(c.stratum.clone())
            .or_default()
            .insert(cmd.to_owned());
    }

    Report {
        cmd: cmd.to_owned(),
        resolution: explain(&live, &providers, cmd),
        launcher: lookup_index(index, cmd),
        copies,
        not_checked,
    }
}

fn skipped(stratum: &str, why: &str) -> Skipped {
    Skipped {
        stratum: stratum.to_owned(),
        why: why.to_owned(),
    }
}

/// The copies of `cmd` in the stratum rooted at `root`, in PATH order.
/// Directories that are symlinks to another one (merged /usr: /bin ->
/// usr/bin) are shown once, under their real name.
pub fn copies_in(stratum: &str, root: &Path, cmd: &str) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();

    for dir in BIN_DIRS {
        let Some(real_dir) = resolve_in_root(root, &format!("/{dir}")) else {
            continue; // no such directory in this stratum
        };

        let path = format!("{}/{cmd}", real_dir.trim_end_matches('/'));
        if found.iter().any(|f| f.path == path) {
            continue; // same directory, reached through a symlink
        }

        let Some(real) = resolve_in_root(root, &path) else {
            continue;
        };
        let file = root.join(real.trim_start_matches('/'));
        if file.is_dir() {
            continue;
        }

        found.push(Found {
            stratum: stratum.to_owned(),
            first: found.is_empty(),
            path,
            real,
            file,
        });
    }
    found
}

/// Resolve `path` (absolute, as seen inside the stratum) with every symlink
/// followed *inside* `root`, like the kernel does after chroot: an absolute
/// link target such as `/etc/alternatives/vi` means `<root>/etc/...`.
/// None if it doesn't exist or loops.
pub fn resolve_in_root(root: &Path, path: &str) -> Option<String> {
    let mut done: Vec<String> = Vec::new();
    // Components still to walk, the next one last (so pop() takes it).
    let mut todo: Vec<String> = split_rev(path);
    let mut links_followed = 0;

    while let Some(part) = todo.pop() {
        if part == "." {
            continue;
        }
        if part == ".." {
            done.pop();
            continue;
        }

        let here = root.join(done.join("/")).join(&part);
        let meta = fs::symlink_metadata(&here).ok()?;

        if !meta.file_type().is_symlink() {
            done.push(part);
            continue;
        }

        links_followed += 1;
        if links_followed > 40 {
            return None; // a symlink loop
        }

        let target = fs::read_link(&here).ok()?;
        let target = target.to_string_lossy();
        if target.starts_with('/') {
            done.clear(); // absolute: back to the stratum's root
        }
        todo.extend(split_rev(&target));
    }

    Some(format!("/{}", done.join("/")))
}

/// Path components, non-empty, in reverse order.
fn split_rev(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|p| !p.is_empty())
        .rev()
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A fresh scratch directory for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("am-which-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    #[test]
    fn absolute_links_stay_inside_the_root() {
        let root = scratch("abs");
        touch(&root.join("usr/bin/vim.basic"));
        fs::create_dir_all(root.join("etc/alternatives")).unwrap();
        symlink("/usr/bin/vim.basic", root.join("etc/alternatives/vi")).unwrap();
        symlink("/etc/alternatives/vi", root.join("usr/bin/vi")).unwrap();

        let real = resolve_in_root(&root, "/usr/bin/vi");
        assert_eq!(real.as_deref(), Some("/usr/bin/vim.basic"));
        assert_eq!(resolve_in_root(&root, "/usr/bin/nope"), None);
    }

    #[test]
    fn loops_are_caught() {
        let root = scratch("loop");
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        symlink("b", root.join("usr/bin/a")).unwrap();
        symlink("a", root.join("usr/bin/b")).unwrap();

        assert_eq!(resolve_in_root(&root, "/usr/bin/a"), None);
    }

    #[test]
    fn a_running_stratum_counts_even_if_not_enabled_at_boot() {
        let dir = scratch("live");
        touch(&dir.join("arch/usr/bin/bash"));
        touch(&dir.join("fedora/usr/bin/dnf"));

        let toml = format!(
            "[[stratum]]\nname = \"arch\"\npath = \"{arch}\"\nkind = \"host\"\nenabled = true\n\n\
             [[stratum]]\nname = \"fedora\"\npath = \"{fedora}\"\nkind = \"fedora\"\nenabled = false\n",
            arch = dir.join("arch").display(),
            fedora = dir.join("fedora").display(),
        );
        let cfg = Config::parse(&toml).unwrap();
        let index = "dnf fedora\n";

        // Started with `amctl strata start fedora`, not enabled at boot.
        let running = report(&cfg, &["fedora".into()], index, "dnf");
        assert_eq!(running.copies.len(), 1);
        let winner = running.resolution.choice.as_ref().unwrap();
        assert_eq!(winner.stratum, "fedora");
        assert!(!running.stale(&cfg), "matches the launcher");
        assert!(running.not_checked.is_empty());

        // Not running: skipped, and called disabled.
        let stopped = report(&cfg, &[], "", "dnf");
        assert!(stopped.copies.is_empty());
        assert_eq!(stopped.not_checked[0].why, "disabled");
    }

    #[test]
    fn merged_usr_shows_each_copy_once() {
        let root = scratch("merged");
        touch(&root.join("usr/bin/tac"));
        touch(&root.join("usr/local/bin/tac"));
        symlink("usr/bin", root.join("bin")).unwrap();
        symlink("bin", root.join("usr/sbin")).unwrap();

        let found = copies_in("arch", &root, "tac");
        let paths: Vec<&str> = found.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["/usr/local/bin/tac", "/usr/bin/tac"]);
        assert!(found[0].first && !found[1].first);
        assert_eq!(found[1].file, root.join("usr/bin/tac"));
    }
}
