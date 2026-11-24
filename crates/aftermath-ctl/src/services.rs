//! Service files: /etc/aftermath/services/<name>.toml, supervised by
//! aftermath-engine and controlled with `amctl services ...`.
//!
//! ```toml
//! description = "Prints a heartbeat every 5 s"
//! exec = ["/usr/bin/bash", "-c", "while true; do date; sleep 5; done"]
//! stratum = "arch"          # optional: run inside another stratum (via strat)
//! user = "root"             # optional: run as this user
//! restart = "on-failure"    # "always" | "on-failure" (default) | "no"
//! restart_delay = 1         # seconds before a restart (default 1)
//! after = ["network"]       # start after these services (ordering only)
//! needs = ["other"]         # start these first; stop this one if they stop
//! enabled = true            # start at boot (amctl services enable/disable
//!                           # overrides it in /etc/aftermath/state.toml)
//! workdir = "/"             # optional
//! [env]                     # optional extra environment
//! FOO = "bar"
//! ```
//!
//! The file name (without .toml) is the service name. Output (stdout and
//! stderr) goes to `<session log dir>/services/<name>.log`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    Always,
    #[default]
    OnFailure,
    No,
}

fn one() -> u64 {
    1
}
fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServiceDef {
    #[serde(skip_deserializing, default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub exec: Vec<String>,
    #[serde(default)]
    pub stratum: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub restart: Restart,
    #[serde(default = "one")]
    pub restart_delay: u64,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c))
        && !n.starts_with('.')
}

pub fn parse(name: &str, text: &str) -> Result<ServiceDef, String> {
    if !valid_name(name) {
        return Err(format!("invalid service name `{name}`"));
    }
    let mut d: ServiceDef = toml::from_str(text).map_err(|e| e.to_string())?;
    if d.exec.is_empty() || d.exec[0].is_empty() {
        return Err("`exec` must name a program".into());
    }
    if !d.exec[0].starts_with('/') {
        return Err(format!(
            "`exec` needs an absolute path, got `{}`",
            d.exec[0]
        ));
    }
    d.name = name.to_owned();
    Ok(d)
}

/// All `*.toml` in `dir`: the good ones, and (file, error) for the rest.
pub fn load_dir(dir: &Path) -> (Vec<ServiceDef>, Vec<(String, String)>) {
    let (mut ok, mut bad) = (Vec::new(), Vec::new());
    let Ok(rd) = fs::read_dir(dir) else {
        return (ok, bad);
    };
    let mut files: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    for f in files {
        let name = f
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match fs::read_to_string(&f)
            .map_err(|e| e.to_string())
            .and_then(|t| parse(&name, &t))
        {
            Ok(d) => ok.push(d),
            Err(e) => bad.push((f.display().to_string(), e)),
        }
    }
    (ok, bad)
}

/// Start order for `wanted` plus everything they `need` (transitively),
/// respecting `after` and `needs` among the chosen set. Unknown `after`
/// names are ignored (pure ordering); unknown `needs` and cycles are errors.
pub fn start_order(defs: &[ServiceDef], wanted: &[String]) -> Result<Vec<String>, String> {
    let by_name: BTreeMap<&str, &ServiceDef> = defs.iter().map(|d| (d.name.as_str(), d)).collect();
    // Close over `needs`.
    let mut chosen = BTreeSet::new();
    let mut todo: Vec<String> = wanted.to_vec();
    while let Some(n) = todo.pop() {
        let d = by_name
            .get(n.as_str())
            .ok_or_else(|| format!("unknown service `{n}`"))?;
        if chosen.insert(n.clone()) {
            todo.extend(d.needs.iter().cloned());
        }
    }
    // Depth-first topological sort (deterministic: names sorted).
    let mut out = Vec::new();
    let mut done = BTreeSet::new();
    fn visit(
        n: &str,
        by_name: &BTreeMap<&str, &ServiceDef>,
        chosen: &BTreeSet<String>,
        done: &mut BTreeSet<String>,
        stack: &mut Vec<String>,
        out: &mut Vec<String>,
    ) -> Result<(), String> {
        if done.contains(n) {
            return Ok(());
        }
        if stack.iter().any(|s| s == n) {
            stack.push(n.to_owned());
            return Err(format!("dependency cycle: {}", stack.join(" -> ")));
        }
        stack.push(n.to_owned());
        let d = by_name[n];
        let mut deps: Vec<&String> = d.needs.iter().chain(d.after.iter()).collect();
        deps.sort();
        deps.dedup();
        for dep in deps {
            if chosen.contains(dep) {
                visit(dep, by_name, chosen, done, stack, out)?;
            }
        }
        stack.pop();
        done.insert(n.to_owned());
        out.push(n.to_owned());
        Ok(())
    }
    for n in &chosen {
        visit(n, &by_name, &chosen, &mut done, &mut Vec::new(), &mut out)?;
    }
    Ok(out)
}

/// Services that (transitively) `need` `name`: stopped along with it.
pub fn dependents(defs: &[ServiceDef], name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut todo = vec![name.to_owned()];
    while let Some(n) = todo.pop() {
        for d in defs {
            if d.needs.contains(&n) && !out.contains(&d.name) && d.name != name {
                out.push(d.name.clone());
                todo.push(d.name.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, text: &str) -> ServiceDef {
        parse(name, text).unwrap()
    }

    #[test]
    fn parses_defaults() {
        let d = def("hb", r#"exec = ["/usr/bin/sleep", "100"]"#);
        assert_eq!(d.restart, Restart::OnFailure);
        assert_eq!(d.restart_delay, 1);
        assert!(d.enabled);
        let d = def(
            "x",
            r#"exec = ["/bin/true"]
restart = "always"
stratum = "ubuntu"
[env]
A = "b""#,
        );
        assert_eq!(d.restart, Restart::Always);
        assert_eq!(d.env.get("A").map(String::as_str), Some("b"));
    }

    #[test]
    fn rejects_bad_files() {
        assert!(parse("x", r#"exec = []"#).is_err());
        assert!(parse("x", r#"exec = ["sleep"]"#).is_err());
        assert!(parse("x", "exec = [\"/bin/true\"]\nbogus = 1").is_err());
        assert!(parse("../x", r#"exec = ["/bin/true"]"#).is_err());
    }

    #[test]
    fn order_and_needs() {
        let defs = vec![
            def("web", "exec=[\"/a\"]\nneeds=[\"db\"]\nafter=[\"log\"]"),
            def("db", "exec=[\"/a\"]\nafter=[\"log\"]"),
            def("log", "exec=[\"/a\"]"),
            def("other", "exec=[\"/a\"]\nafter=[\"nonexistent\"]"),
        ];
        // web pulls in db; log is only ordering (not wanted -> not started)
        assert_eq!(
            start_order(&defs, &["web".into()]).unwrap(),
            vec!["db", "web"]
        );
        let all: Vec<String> = defs.iter().map(|d| d.name.clone()).collect();
        let o = start_order(&defs, &all).unwrap();
        let pos = |n: &str| o.iter().position(|x| x == n).unwrap();
        assert!(pos("log") < pos("db") && pos("db") < pos("web"));
        assert_eq!(dependents(&defs, "db"), vec!["web"]);
        assert!(start_order(&defs, &["nope".into()]).is_err());
    }

    #[test]
    fn cycles() {
        let defs = vec![
            def("a", "exec=[\"/a\"]\nneeds=[\"b\"]"),
            def("b", "exec=[\"/a\"]\nafter=[\"a\"]"),
        ];
        assert!(start_order(&defs, &["a".into()])
            .unwrap_err()
            .contains("cycle"));
    }
}
