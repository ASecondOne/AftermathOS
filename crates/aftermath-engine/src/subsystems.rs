//! The built-in amctl subsystems besides services: system, strata, boot,
//! overlay, locale. Each is a small adapter around aftermath-ctl /
//! aftermath-strata so `amctl` can do the same things offline (as root)
//! when the engine is down.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use aftermath_ctl::{facts, grubenv, locale, overlay, STRATA_SERVICE};
use aftermath_strata::state::{State as Switches, STATE};
use aftermath_strata::CONFIG;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json::{json, Value};

use crate::control::Subsystem;

fn need_arg(args: &[String], what: &str) -> Result<String, String> {
    args.first().cloned().ok_or_else(|| format!("needs {what}"))
}

// ------------------------------------------------------------ system ----

pub struct System;

impl Subsystem for System {
    fn name(&self) -> &'static str {
        "system"
    }
    fn about(&self) -> &'static str {
        "status, poweroff, reboot"
    }
    fn queries(&self) -> &'static [&'static str] {
        &["status", "subsystems"]
    }
    fn call(&mut self, verb: &str, _args: &[String]) -> Result<Value, String> {
        // aftermath-init (PID 1) shuts down cleanly on these signals.
        let sig = match verb {
            "poweroff" => Signal::SIGUSR2,
            "reboot" => Signal::SIGINT,
            "halt" => Signal::SIGUSR1,
            other => return Err(format!("unknown verb `system {other}`")),
        };
        crate::log(&format!("{verb} requested via amctl"));
        // Answer first, then signal PID 1.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let _ = kill(Pid::from_raw(1), sig);
        });
        Ok(json!({"action": verb}))
    }
}

// ------------------------------------------------------------ strata ----

pub struct Strata;

use aftermath_ctl::facts::{load_cfg, stratum_json};

fn run_strata(args: &[&str]) -> Result<(), String> {
    let out = Command::new(STRATA_SERVICE)
        .args(args)
        .env("AFTERMATH_KMSG", "1")
        .output()
        .map_err(|e| format!("{STRATA_SERVICE}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err
            .lines()
            .last()
            .unwrap_or("aftermath-strata failed")
            .trim_start_matches("aftermath-strata: ")
            .trim_start_matches("FAILED: ")
            .to_owned())
    }
}

fn mode_is_full() -> bool {
    facts::mode() == "full"
}

impl Subsystem for Strata {
    fn name(&self) -> &'static str {
        "strata"
    }
    fn about(&self) -> &'static str {
        "strata from /etc/aftermath/strata.toml (list, status, start, stop, restart, enable, disable)"
    }
    fn queries(&self) -> &'static [&'static str] {
        &["list", "status"]
    }
    fn call(&mut self, verb: &str, args: &[String]) -> Result<Value, String> {
        let cfg = load_cfg()?;
        match verb {
            "list" => {
                let list: Result<Vec<Value>, String> = cfg
                    .stratum
                    .iter()
                    .map(|s| stratum_json(&cfg, &s.name))
                    .collect();
                Ok(json!({"strata": list?, "mode": facts::mode()}))
            }
            "status" => stratum_json(&cfg, &need_arg(args, "a stratum name")?),
            "start" | "stop" | "restart" | "enable" | "disable" => {
                let name = need_arg(args, "a stratum name")?;
                let s = cfg
                    .get(&name)
                    .ok_or_else(|| format!("no stratum `{name}` in {CONFIG}"))?;
                if s.kind == "host" {
                    return Err(format!("`{name}` is the host stratum: always up"));
                }
                if matches!(verb, "enable" | "disable") {
                    let mut sw = Switches::load(Path::new(STATE))?;
                    sw.strata.insert(name.clone(), verb == "enable");
                    sw.save(Path::new(STATE))?;
                }
                let starting = matches!(verb, "start" | "restart" | "enable");
                if starting && !mode_is_full() {
                    return Err(
                        "strata are off in Arch host mode (plan B); boot normally first".into(),
                    );
                }
                if matches!(verb, "stop" | "restart" | "disable") {
                    let mut a = vec!["stop", name.as_str()];
                    if verb == "disable" {
                        a.push("--disabled");
                    }
                    run_strata(&a)?;
                }
                if starting {
                    run_strata(&["start", &name])?;
                }
                stratum_json(&load_cfg()?, &name)
            }
            other => Err(format!("unknown verb `strata {other}`")),
        }
    }
}

// -------------------------------------------------------------- boot ----

pub struct Boot;

impl Subsystem for Boot {
    fn name(&self) -> &'static str {
        "boot"
    }
    fn about(&self) -> &'static str {
        "GRUB one-shot next boot (normal, host, busybox, clean, serial, debug, none)"
    }
    fn queries(&self) -> &'static [&'static str] {
        &["status"]
    }
    fn call(&mut self, verb: &str, args: &[String]) -> Result<Value, String> {
        match verb {
            "next" => {
                let target = need_arg(args, "a target (normal, host, busybox, clean, none)")?;
                let dev = grubenv::set_next(&target)?;
                crate::log(&format!("next boot: {target} (grubenv on {dev})"));
                Ok(json!({"next": target, "device": dev}))
            }
            "status" => match grubenv::read_all() {
                Ok(Some((dev, vars))) => {
                    let get = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
                    let next = get("aftermath_next").filter(|v| !v.is_empty());
                    let target = next.as_ref().and_then(|e| {
                        grubenv::NEXT_TARGETS
                            .iter()
                            .find(|(_, id)| id == e)
                            .map(|(t, _)| *t)
                    });
                    Ok(json!({
                        "device": dev,
                        "next": target.map(str::to_owned).or(next),
                        "boot_fails": get("boot_fails"),
                        "boot_success": get("boot_success"),
                    }))
                }
                Ok(None) => Ok(
                    json!({"device": null, "note": "no GRUB environment (ISO or --direct boot)"}),
                ),
                Err(e) => Err(format!("grubenv: {e}")),
            },
            other => Err(format!("unknown verb `boot {other}`")),
        }
    }
}

// ----------------------------------------------------------- overlay ----

pub struct Overlay;

impl Subsystem for Overlay {
    fn name(&self) -> &'static str {
        "overlay"
    }
    fn about(&self) -> &'static str {
        "persistent host root: status, reset (on the next boot), reset --cancel"
    }
    fn queries(&self) -> &'static [&'static str] {
        &["status"]
    }
    fn call(&mut self, verb: &str, args: &[String]) -> Result<Value, String> {
        match verb {
            "status" => Ok(overlay::status()),
            "reset" => {
                let cancel = args.iter().any(|a| a == "--cancel");
                let at = overlay::request_reset(cancel)?;
                crate::log(&format!(
                    "overlay reset {} ({at})",
                    if cancel {
                        "cancelled"
                    } else {
                        "requested for the next boot"
                    }
                ));
                Ok(json!({"reset_pending": !cancel, "disk": at}))
            }
            other => Err(format!("unknown verb `overlay {other}`")),
        }
    }
    fn summary(&self) -> Option<Value> {
        None // already part of the facts
    }
}

// ------------------------------------------------------------ locale ----

pub struct Locale {
    pub loaded: Option<String>,
}

/// At boot: derive vconsole.conf / xorg keyboard config and load the keymap.
pub fn keyboard_boot() -> Option<String> {
    let root = Path::new("/");
    let (l, err) = locale::load(root);
    if let Some(e) = err {
        crate::warn(&format!("{e} (using the default German layout)"));
    }
    let changed = locale::write_derived(&l, root);
    if !changed.is_empty() {
        crate::log(&format!("keyboard config written: {}", changed.join(", ")));
    }
    match locale::load_keymap(root, &l.keymap) {
        Ok(how) => {
            crate::log(&format!("console keymap {} loaded ({how})", l.keymap));
            let _ = fs::write("/run/aftermath/keymap", format!("{}\n", l.keymap));
            Some(l.keymap)
        }
        Err(e) => {
            crate::warn(&format!(
                "console keymap {} NOT loaded: {e} (the kernel default US layout stays)",
                l.keymap
            ));
            None
        }
    }
}

impl Subsystem for Locale {
    fn name(&self) -> &'static str {
        "locale"
    }
    fn about(&self) -> &'static str {
        "keyboard layout from /etc/aftermath/aftermath.toml [locale]"
    }
    fn queries(&self) -> &'static [&'static str] {
        &["keymap"]
    }
    fn call(&mut self, verb: &str, args: &[String]) -> Result<Value, String> {
        match verb {
            "keymap" => {
                let (l, _) = locale::load(Path::new("/"));
                let probe = locale::probe_y_key();
                Ok(json!({
                    "configured": l,
                    "loaded": self.loaded,
                    "probe_key_y": probe.map(String::from),
                }))
            }
            "set" => {
                let name = need_arg(args, "a keymap name (e.g. de-latin1-nodeadkeys, us)")?;
                if !locale::valid_keymap(&name) {
                    return Err(format!("invalid keymap name `{name}`"));
                }
                let how = locale::apply_keymap(Path::new("/"), &name)?;
                self.loaded = Some(name.clone());
                crate::log(&format!("console keymap {name} loaded ({how}) via amctl"));
                Ok(json!({"keymap": name, "how": how}))
            }
            other => Err(format!("unknown verb `locale {other}`")),
        }
    }
    fn summary(&self) -> Option<Value> {
        None
    }
}
