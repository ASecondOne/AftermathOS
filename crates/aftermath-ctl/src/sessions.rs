//! Shared console/session identifiers and declarative console configuration.

use std::fs;
use std::path::Path;

use serde::Deserialize;

pub const PAM_MODULE: &str = "/usr/lib/security/pam_aftermath.so";

pub fn normalize_tty(tty: &str) -> Option<String> {
    let tty = tty.strip_prefix("/dev/").unwrap_or(tty);
    let valid = if let Some(n) = tty.strip_prefix("pts/") {
        !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
    } else {
        (tty.starts_with("tty") || tty == "console")
            && tty.bytes().all(|b| b.is_ascii_alphanumeric())
    };
    (valid && tty.len() <= 32).then(|| tty.to_owned())
}

pub fn vt_number(tty: &str) -> Option<u32> {
    let n: u32 = tty.strip_prefix("tty")?.parse().ok()?;
    (1..=63).contains(&n).then_some(n)
}

#[derive(Deserialize)]
#[serde(default)]
struct Console {
    login_ttys: Vec<String>,
}
impl Default for Console {
    fn default() -> Self {
        Self {
            login_ttys: vec!["tty2".into()],
        }
    }
}
#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    console: Console,
}

pub fn parse_login_ttys(text: &str) -> Result<Vec<String>, String> {
    let c: Config = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for tty in c.console.login_ttys {
        if vt_number(&tty).is_none() {
            return Err(format!(
                "login_ttys needs a VT from tty1 to tty63, got `{tty}`"
            ));
        }
        if !out.contains(&tty) {
            out.push(tty);
        }
    }
    Ok(out)
}

pub fn login_ttys(root: &Path) -> Result<Vec<String>, String> {
    match fs::read_to_string(root.join("etc/aftermath/aftermath.toml")) {
        Ok(t) => parse_login_ttys(&t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Console::default().login_ttys),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identifiers_and_console_configuration() {
        assert_eq!(normalize_tty("/dev/pts/3").as_deref(), Some("pts/3"));
        assert_eq!(normalize_tty("tty2").as_deref(), Some("tty2"));
        for bad in [
            "../../etc/passwd",
            "/dev/pts/../tty1",
            "pts/",
            "tty2/extra",
            "",
        ] {
            assert!(normalize_tty(bad).is_none());
        }
        assert_eq!(vt_number("tty2"), Some(2));
        assert_eq!(vt_number("ttyS0"), None);
        assert_eq!(parse_login_ttys("").unwrap(), ["tty2"]);
        assert!(parse_login_ttys("[console]\nlogin_ttys=[]")
            .unwrap()
            .is_empty());
        assert!(parse_login_ttys("[console]\nlogin_ttys=['ttyS0']").is_err());
    }
}
