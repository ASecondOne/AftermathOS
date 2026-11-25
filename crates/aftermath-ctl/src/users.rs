//! Local account inventory and validation. Password hashes never leave this
//! module; mutation uses the distribution's account tools in the engine.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::Serialize;

pub const MIN_UID: u32 = 1000;
pub const MAX_UID: u32 = 59999;

#[derive(Clone, Debug, Serialize)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub full_name: String,
    pub home: String,
    pub shell: String,
    pub groups: Vec<String>,
    pub password_status: &'static str,
    pub can_unlock: bool,
}

pub fn regular(uid: u32) -> bool {
    (MIN_UID..=MAX_UID).contains(&uid)
}

pub fn valid_new_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'_'))
        && name.len() <= 32
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

pub fn valid_existing_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.$".contains(&b))
}

fn password_state(field: Option<&str>) -> (&'static str, bool) {
    match field {
        None => ("unknown", false),
        Some("") => ("empty", false),
        Some(s) if s.starts_with('!') || s.starts_with('*') => {
            let unlock = s
                .strip_prefix('!')
                .is_some_and(|rest| !rest.is_empty() && !rest.starts_with(['!', '*']));
            ("locked", unlock)
        }
        Some(_) => ("set", false),
    }
}

pub fn parse(passwd: &str, group: &str, shadow: Option<&str>) -> Result<Vec<Account>, String> {
    let mut groups = Vec::new();
    for (line, text) in group
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.is_empty() && !l.starts_with('#'))
    {
        let f: Vec<_> = text.split(':').collect();
        if f.len() != 4 {
            return Err(format!("invalid group database entry on line {}", line + 1));
        }
        let gid = f[2]
            .parse::<u32>()
            .map_err(|_| format!("invalid group ID on line {}", line + 1))?;
        groups.push((f[0], gid, f[3].split(',').collect::<Vec<_>>()));
    }
    let shadow: BTreeMap<_, _> = shadow
        .into_iter()
        .flat_map(str::lines)
        .filter_map(|l| {
            let mut f = l.split(':');
            Some((f.next()?, f.next()?))
        })
        .collect();
    let mut out = Vec::new();
    let mut names = BTreeSet::new();
    for (line, text) in passwd
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.is_empty() && !l.starts_with('#'))
    {
        let f: Vec<_> = text.split(':').collect();
        if f.len() != 7 || !names.insert(f[0]) {
            return Err(format!(
                "invalid or duplicate passwd entry on line {}",
                line + 1
            ));
        }
        let uid = f[2]
            .parse()
            .map_err(|_| format!("invalid uid on line {}", line + 1))?;
        let gid = f[3]
            .parse()
            .map_err(|_| format!("invalid gid on line {}", line + 1))?;
        let (password_status, can_unlock) = password_state(shadow.get(f[0]).copied());
        let memberships = groups
            .iter()
            .filter(|(_, id, members)| *id == gid || members.contains(&f[0]))
            .map(|(name, _, _)| (*name).to_owned())
            .collect();
        out.push(Account {
            name: f[0].into(),
            uid,
            gid,
            full_name: f[4].into(),
            home: f[5].into(),
            shell: f[6].into(),
            groups: memberships,
            password_status,
            can_unlock,
        });
    }
    out.sort_by(|a, b| a.uid.cmp(&b.uid).then(a.name.cmp(&b.name)));
    Ok(out)
}

pub fn read(root: &Path) -> Result<Vec<Account>, String> {
    let passwd = fs::read_to_string(root.join("etc/passwd"))
        .map_err(|e| format!("read local passwd database: {e}"))?;
    let group = fs::read_to_string(root.join("etc/group"))
        .map_err(|e| format!("read local group database: {e}"))?;
    let shadow = match fs::read_to_string(root.join("etc/shadow")) {
        Ok(s) => Some(s),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) =>
        {
            None
        }
        Err(e) => return Err(format!("read local password status: {e}")),
    };
    parse(&passwd, &group, shadow.as_deref())
}

pub fn require_aftermath() -> Result<(), String> {
    let os = fs::read_to_string("/usr/lib/os-release").unwrap_or_default();
    if os.lines().any(|l| {
        l.strip_prefix("ID=")
            .is_some_and(|id| id.trim_matches('"') == "aftermath")
    }) {
        Ok(())
    } else {
        Err("account changes must run inside Aftermath, not on the build host".into())
    }
}

/// Pick a globally unused local ID among every available configured userland.
pub fn next_uid(passwd_dbs: &[String], group_dbs: &[String]) -> Result<u32, String> {
    let used: BTreeSet<u32> = passwd_dbs
        .iter()
        .chain(group_dbs)
        .flat_map(|db| db.lines())
        .filter_map(|line| line.split(':').nth(2)?.parse().ok())
        .collect();
    (MIN_UID..=MAX_UID)
        .find(|id| !used.contains(id))
        .ok_or_else(|| "no unused regular-user UID/GID remains".into())
}

pub fn create_args(name: &str, uid: u32) -> Result<Vec<String>, String> {
    if !valid_new_name(name) {
        return Err("use a name of 1-32 lowercase letters, digits, '_' or '-', starting with a letter or '_'".into());
    }
    if !regular(uid) {
        return Err("new accounts need a regular-user UID".into());
    }
    Ok([
        "--create-home",
        "--user-group",
        "--shell",
        "/usr/bin/bash",
        "--uid",
        &uid.to_string(),
        "--key",
        "HOME_MODE=0700",
        "--key",
        "UID_MIN=1000",
        "--key",
        "UID_MAX=59999",
        "--",
        name,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inventory_has_groups_and_status_but_never_hashes() {
        let users = parse(
            "root:x:0:0:Root:/root:/bin/bash\nalice:x:1000:1000:Alice:/home/alice:/usr/bin/bash\n",
            "root:x:0:\nalice:x:1000:\nwheel:x:998:alice\n",
            Some("root:!:::::::\nalice:!$y$must-not-leak:::::::\n"),
        )
        .unwrap();
        assert_eq!(users[1].groups, ["alice", "wheel"]);
        assert_eq!(users[1].password_status, "locked");
        assert!(users[1].can_unlock);
        let json = serde_json::to_string(&users).unwrap();
        assert!(!json.contains("must-not-leak") && !json.contains("$y$"));
        assert!(!users[0].can_unlock);
        assert_eq!(password_state(None).0, "unknown");
        assert_eq!(password_state(Some("")).0, "empty");
        assert!(!password_state(Some("!!")).1);
    }
    #[test]
    fn validates_names_and_picks_ids_across_userlands() {
        for n in ["tim", "alice-2", "_user"] {
            assert!(valid_new_name(n));
        }
        for n in ["", "../root", "-p", "Alice", "a:b", "a\nb", "a b"] {
            assert!(!valid_new_name(n));
        }
        assert_eq!(
            next_uid(
                &["alice:x:1000:1000::/:/bin/bash".into()],
                &["other:x:1001:".into()]
            )
            .unwrap(),
            1002
        );
        let args = create_args("tim", 1002).unwrap();
        assert_eq!(&args[args.len() - 2..], ["--", "tim"]);
        assert!(!args.iter().any(|a| a == "--password"));
        assert!(create_args("root", 0).is_err());
        assert!(parse("broken", "", None).is_err());
    }
}
