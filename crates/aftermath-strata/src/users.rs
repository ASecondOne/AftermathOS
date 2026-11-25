//! Sharing the host's users with a stratum.
//!
//! We don't bind-mount /etc/passwd & co into strata: package managers
//! (adduser, systemd-sysusers, ...) replace those files with rename(), which
//! fails on a bind-mounted file. Instead each stratum keeps its own system
//! users (e.g. Ubuntu's `_apt`) and gets the host's *human* accounts (root
//! and uid/gid 1000-59999) copied in at boot and when the host accounts change.

use std::collections::BTreeSet;

/// Range of "real" accounts shared from the host.
fn shared_id(id: &str) -> bool {
    id.parse::<u32>().is_ok_and(|n| (1000..60000).contains(&n))
}

fn name(line: &str) -> Option<&str> {
    line.split(':')
        .next()
        .filter(|n| !n.is_empty() && !n.starts_with('#'))
}

/// Names of host entries to share from a passwd- or group-style file
/// (`id_field` = 2 for both: name:x:ID:...).
pub fn shared_names(host_db: &str) -> BTreeSet<String> {
    host_db
        .lines()
        .filter(|l| {
            let f: Vec<&str> = l.split(':').collect();
            f.len() > 2 && (f[0] == "root" || shared_id(f[2]))
        })
        .filter_map(|l| name(l).map(str::to_owned))
        .collect()
}

/// Merge: keep the stratum's entries, replace/add the host's shared ones.
pub fn merge(host_db: &str, stratum_db: &str, shared: &BTreeSet<String>) -> String {
    let host_line = |n: &str| host_db.lines().find(|l| name(l) == Some(n));
    let mut out = String::new();
    let mut done = BTreeSet::new();
    for l in stratum_db.lines() {
        match name(l) {
            Some(n) if shared.contains(n) => {
                if let Some(h) = host_line(n) {
                    out.push_str(h);
                    out.push('\n');
                    done.insert(n.to_owned());
                }
            }
            _ => {
                out.push_str(l);
                out.push('\n');
            }
        }
    }
    for n in shared.iter().filter(|n| !done.contains(*n)) {
        if let Some(h) = host_line(n) {
            out.push_str(h);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_users() {
        let host = "root:x:0:0::/root:/bin/bash\nbin:x:1:1::/:/usr/bin/nologin\ntim:x:1000:1000::/home/tim:/bin/bash\n";
        let ubuntu =
            "root:x:0:0:root:/root:/bin/sh\n_apt:x:42:65534::/nonexistent:/usr/sbin/nologin\n";
        let shared = shared_names(host);
        assert_eq!(shared, ["root", "tim"].map(String::from).into());
        let m = merge(host, ubuntu, &shared);
        assert_eq!(
            m,
            "root:x:0:0::/root:/bin/bash\n_apt:x:42:65534::/nonexistent:/usr/sbin/nologin\ntim:x:1000:1000::/home/tim:/bin/bash\n"
        );
        // shadow has no ids: reuse the passwd names.
        let m = merge(
            "root:!:1::::::\ntim:$6$x:1::::::\n",
            "root:*:1::::::\n_apt:*:1::::::\n",
            &shared,
        );
        assert!(m.starts_with("root:!:") && m.contains("_apt:*") && m.contains("tim:$6$x"));
    }
}
