//! System facts that need no engine: what `amctl status` shows, online
//! (the engine adds services) or offline (plan B, read straight from
//! /proc, /sys and /run).

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::{disks, overlay, LOGDIR_FILE, RUN_DIR};

fn read_trim(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// full / host (engine) or busybox (PID 1 rescue layer, no engine).
pub fn mode() -> String {
    if let Some(m) = read_trim(format!("{RUN_DIR}/mode")) {
        return m;
    }
    if Path::new("/run/aftermath/rescue/bin").exists() {
        return "busybox".into();
    }
    "unknown".into()
}

pub fn uptime_secs() -> f64 {
    read_trim("/proc/uptime")
        .and_then(|u| u.split_whitespace().next().and_then(|t| t.parse().ok()))
        .unwrap_or(0.0)
}

pub fn human_duration(secs: f64) -> String {
    let s = secs as u64;
    let (d, h, m, s) = (s / 86400, s / 3600 % 24, s / 60 % 60, s % 60);
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Default route (iface, gateway) from /proc/net/route.
pub fn default_route(table: &str) -> Option<(String, String)> {
    table.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() > 2 && f[1] == "00000000" {
            let gw = u32::from_str_radix(f[2], 16).ok()?;
            let b = gw.to_le_bytes();
            Some((
                f[0].to_owned(),
                format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]),
            ))
        } else {
            None
        }
    })
}

pub fn nameservers(resolv: &str) -> Vec<String> {
    resolv
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            (w.next() == Some("nameserver"))
                .then(|| w.next().map(str::to_owned))
                .flatten()
        })
        .collect()
}

pub fn network() -> Value {
    let mut ifaces = Vec::new();
    if let Ok(addrs) = nix::ifaddrs::getifaddrs() {
        for a in addrs {
            if a.interface_name == "lo" {
                continue;
            }
            let Some(sin) = a.address.as_ref().and_then(|s| s.as_sockaddr_in()) else {
                continue;
            };
            let prefix = a
                .netmask
                .as_ref()
                .and_then(|m| m.as_sockaddr_in())
                .map(|m| u32::from(m.ip()).count_ones())
                .unwrap_or(0);
            ifaces
                .push(json!({"name": a.interface_name, "ipv4": format!("{}/{prefix}", sin.ip())}));
        }
    }
    let route = default_route(&fs::read_to_string("/proc/net/route").unwrap_or_default());
    let dns = nameservers(&fs::read_to_string("/etc/resolv.conf").unwrap_or_default());
    json!({
        "online": route.is_some() && !dns.is_empty(),
        "interfaces": ifaces,
        "gateway": route.as_ref().map(|r| r.1.clone()),
        "gateway_iface": route.map(|r| r.0),
        "dns": dns,
    })
}

/// `/run/aftermath/strata` lines: `<name> <state>[: detail]`.
pub fn strata_status() -> Vec<(String, String)> {
    fs::read_to_string(format!("{RUN_DIR}/strata"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(n, s)| (n.to_owned(), s.to_owned()))
        .collect()
}

pub fn log_dir() -> Option<String> {
    read_trim(LOGDIR_FILE)
}

/// Everything except services (those need the engine).
pub fn collect() -> Value {
    let strata: Vec<Value> = strata_status()
        .into_iter()
        .map(|(n, s)| json!({"name": n, "state": s}))
        .collect();
    let up = uptime_secs();
    json!({
        "mode": mode(),
        "hostname": read_trim("/proc/sys/kernel/hostname"),
        "kernel": read_trim("/proc/sys/kernel/osrelease"),
        "boot_id": read_trim("/proc/sys/kernel/random/boot_id"),
        "uptime_secs": up as u64,
        "uptime": human_duration(up),
        "overlay": overlay::status(),
        "network": network(),
        "strata": strata,
        "keymap": read_trim(format!("{RUN_DIR}/keymap")),
        "log_dir": log_dir(),
        "root_fs": disks::mount_of(Path::new("/")).map(|(_, t)| t),
    })
}

pub fn os_name(root: &Path) -> Option<String> {
    ["etc/os-release", "usr/lib/os-release"]
        .iter()
        .find_map(|f| {
            fs::read_to_string(root.join(f))
                .ok()?
                .lines()
                .find_map(|l| {
                    l.strip_prefix("PRETTY_NAME=")
                        .map(|v| v.trim_matches('"').to_owned())
                })
        })
}

/// strata.toml with the state.toml switches applied.
pub fn load_cfg() -> Result<aftermath_strata::config::Config, String> {
    aftermath_strata::config::Config::load_with_state(
        Path::new(aftermath_strata::CONFIG),
        Path::new(aftermath_strata::state::STATE),
    )
}

/// One stratum as amctl shows it (config + live state).
pub fn stratum_json(cfg: &aftermath_strata::config::Config, name: &str) -> Result<Value, String> {
    let s = cfg
        .get(name)
        .ok_or_else(|| format!("no stratum `{name}` in {}", aftermath_strata::CONFIG))?;
    let host = s.kind == "host";
    let state = if host {
        "up".to_owned()
    } else {
        strata_status()
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, st)| st)
            .unwrap_or_else(|| if s.enabled { "down" } else { "disabled" }.into())
    };
    let root = Path::new(&s.path);
    let mounted = host || disks::mount_of(root).is_some();
    Ok(json!({
        "name": s.name,
        "kind": s.kind,
        "path": s.path,
        "enabled": s.enabled,
        "host": host,
        "state": state,
        "mounted": mounted,
        "os": if mounted { os_name(root) } else { None },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_and_dns() {
        let t = "Iface\tDestination\tGateway\nlo\t0000007F\t00000000\neth0\t00000000\t0202000A\n";
        assert_eq!(default_route(t), Some(("eth0".into(), "10.0.2.2".into())));
        assert_eq!(default_route("Iface\n"), None);
        assert_eq!(
            nameservers("# x\nnameserver 10.0.2.3\nsearch y\n"),
            vec!["10.0.2.3"]
        );
    }

    #[test]
    fn durations() {
        assert_eq!(human_duration(5.2), "5s");
        assert_eq!(human_duration(125.0), "2m 5s");
        assert_eq!(human_duration(3_700.0), "1h 1m");
        assert_eq!(human_duration(90_000.0), "1d 1h 0m");
    }
}
