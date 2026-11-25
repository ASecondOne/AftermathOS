//! `amctl default` and `amctl which`: the work itself, shared by the engine
//! (online) and amctl (offline plan B). Changing a default writes
//! /etc/aftermath/strata.d/user.toml and regenerates the cross launchers
//! right away, so no reboot is needed.

use std::process::Command;

use aftermath_strata::config::Config;
use aftermath_strata::cross::Reason;
use aftermath_strata::defaults::{UserDefaults, USER_DEFAULTS};
use aftermath_strata::which::{self, Report};
use aftermath_strata::{at, status, CROSS_DIR};
use serde_json::{json, Value};

use crate::STRATA_SERVICE;

/// `amctl default list`: the user's defaults and order, plus the shipped pins.
pub fn list() -> Result<Value, String> {
    let cfg = Config::load_system()?;

    Ok(json!({
        "file": USER_DEFAULTS,
        "defaults": cfg.user.pin,
        "order": cfg.order(),
        "user_order": cfg.user.order,
        "shipped_pins": cfg.cross.pin,
    }))
}

/// `amctl default <cmd> <stratum>`.
pub fn set(cmd: &str, stratum: &str) -> Result<Value, String> {
    let cfg = Config::load_system()?;
    let mut user = load_user()?;

    user.set_pin(&cfg, cmd, stratum)?;
    save_user(&user)?;

    let mut reply = after_change(cmd)?;
    reply["default"] = json!(stratum);
    if let Some(note) = availability_note(&cfg, cmd, stratum) {
        reply["note"] = json!(note);
    }
    Ok(reply)
}

/// `amctl default <cmd> --reset`.
pub fn reset(cmd: &str) -> Result<Value, String> {
    let mut user = load_user()?;

    let was = user.reset_pin(cmd)?;
    save_user(&user)?;

    let mut reply = after_change(cmd)?;
    reply["was"] = json!(was);
    Ok(reply)
}

/// `amctl default order <stratum>...`; an empty list resets the order.
pub fn set_order(names: &[String]) -> Result<Value, String> {
    let cfg = Config::load_system()?;
    let mut user = load_user()?;

    if names.is_empty() {
        user.order.clear();
    } else {
        user.set_order(&cfg, names)?;
    }
    save_user(&user)?;

    let launchers = refresh_launchers();
    let cfg = Config::load_system()?;
    Ok(json!({ "order": cfg.order(), "launchers": launchers }))
}

/// `amctl which <cmd>`: every copy and the winner. Read-only.
pub fn which(cmd: &str) -> Result<Value, String> {
    let cfg = Config::load_system()?;
    let index = std::fs::read_to_string(at(&format!("{CROSS_DIR}/index"))).unwrap_or_default();

    let report = which::report(&cfg, &status::up(), &index, cmd);
    Ok(report_json(&cfg, &report))
}

pub fn report_json(cfg: &Config, r: &Report) -> Value {
    let copies: Vec<Value> = r
        .copies
        .iter()
        .map(|c| {
            json!({
                "stratum": c.stratum,
                "path": c.path,
                "real": c.real,
                "file": c.file,
                "first_in_stratum": c.first,
            })
        })
        .collect();

    let winner = r.resolution.choice.as_ref().map(|c| {
        json!({
            "stratum": c.stratum,
            "reason": c.reason.key(),
            "why": c.reason.describe(&r.cmd),
        })
    });

    let passed_over: Vec<Value> = r
        .resolution
        .passed_over
        .iter()
        .map(|p| {
            json!({
                "stratum": p.stratum,
                "reason": p.reason.key(),
                "why": rule_name(&p.reason),
            })
        })
        .collect();

    let not_checked: Vec<Value> = r
        .not_checked
        .iter()
        .map(|s| json!({ "stratum": s.stratum, "why": s.why }))
        .collect();

    json!({
        "command": r.cmd,
        "copies": copies,
        "winner": winner,
        "runs_in": r.runs_in(cfg),
        "launcher": r.launcher,
        "stale": r.stale(cfg),
        "passed_over": passed_over,
        "not_checked": not_checked,
    })
}

/// "your default" / "shipped pin `apt*`" for a rule that was passed over.
fn rule_name(reason: &Reason) -> String {
    match reason {
        Reason::UserDefault => "your default".into(),
        Reason::ShippedPin(p) => format!("shipped pin `{p}`"),
        Reason::Order | Reason::Host => "the lookup order".into(),
    }
}

// ------------------------------------------------------------ helpers ----

fn load_user() -> Result<UserDefaults, String> {
    UserDefaults::load(&at(USER_DEFAULTS))
}

fn save_user(user: &UserDefaults) -> Result<(), String> {
    user.save(&at(USER_DEFAULTS))
}

/// Regenerate the launchers, then report what `cmd` resolves to now.
fn after_change(cmd: &str) -> Result<Value, String> {
    let launchers = refresh_launchers();

    let mut reply = which(cmd)?;
    reply["launchers"] = json!(launchers);
    reply["file"] = json!(USER_DEFAULTS);
    Ok(reply)
}

/// `aftermath-strata refresh`. Never fatal: the choice is saved either
/// way and applies at the latest when the strata come up at the next boot.
fn refresh_launchers() -> String {
    let service = at(STRATA_SERVICE);
    if !service.exists() {
        return format!("not regenerated: {STRATA_SERVICE} is missing");
    }

    match Command::new(&service).arg("refresh").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            format!("not regenerated ({}); saved for the next boot", err.trim())
        }
        Err(e) => format!("not regenerated ({e}); saved for the next boot"),
    }
}

/// A hint when the chosen stratum can't run `cmd` right now.
fn availability_note(cfg: &Config, cmd: &str, stratum: &str) -> Option<String> {
    let s = cfg.get(stratum)?;
    let host = s.kind == "host";

    if !host && !s.enabled {
        return Some(format!(
            "`{stratum}` is disabled: until it is enabled, the usual rules apply"
        ));
    }
    if !host && !status::up().contains(&s.name) {
        return Some(format!(
            "`{stratum}` is not running: the default applies once it is up"
        ));
    }

    let copies = which::copies_in(stratum, &at(&s.path), cmd);
    if copies.is_empty() {
        let hint = format!(
            "`{stratum}` has no `{cmd}`: until it does, the usual rules apply \
             (install it, e.g. `strat {stratum} apt install ...`)"
        );
        return Some(hint);
    }
    None
}
