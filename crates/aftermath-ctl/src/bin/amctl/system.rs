//! The smaller commands: boot next/status, overlay status/reset, keymap,
//! poweroff/reboot and the subsystem list. Each one works offline as far as
//! possible (as root), so they still help when the engine is down.

use std::fs;
use std::path::Path;

use aftermath_ctl::{grubenv, locale, overlay};
use nix::sys::reboot::{reboot, RebootMode};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json::{json, Value};

use crate::output::{is_true, need_engine, need_root, text, Ctx};
use crate::status::root_line;

// --------------------------------------------------------------- boot ----

pub fn boot_next(cx: &Ctx, target: &str) -> Result<(), String> {
    let reply = match cx.ask("boot", "next", &[target])? {
        Some(v) => v,
        None => {
            need_root("boot next")?;
            let device = grubenv::set_next(target)?;
            json!({ "next": target, "device": device })
        }
    };

    cx.emit(&reply, |v| {
        let st = &cx.style;
        if text(v, "next") == "none" {
            println!("next boot: {}", st.purple("the normal menu default"));
        } else {
            let device = format!("grubenv on {}", text(v, "device"));
            println!(
                "next boot (once): {}  {}",
                st.purple(text(v, "next")),
                st.dim(&device)
            );
        }
    });
    Ok(())
}

pub fn boot_status(cx: &Ctx) -> Result<(), String> {
    let reply = cx
        .ask("boot", "status", &[])?
        .ok_or_else(|| need_engine("boot status"))?;

    cx.emit(&reply, |v| {
        let st = &cx.style;
        st.header("boot");

        // ISO / --direct boots have no grubenv at all.
        if v["device"].is_null() {
            st.row("grubenv", &st.dim(text(v, "note")));
            return;
        }

        st.row("grubenv", text(v, "device"));
        let next = match text(v, "next") {
            "" => "menu default".to_string(),
            next => st.purple(&format!("{next} (once)")),
        };
        st.row("next", &next);
        st.row("boot_fails", text(v, "boot_fails"));
    });
    Ok(())
}

// ------------------------------------------------------------ overlay ----

pub fn overlay_status(cx: &Ctx) -> Result<(), String> {
    let reply = cx
        .ask("overlay", "status", &[])?
        .unwrap_or_else(overlay::status);

    cx.emit(&reply, |v| {
        cx.style.header("overlay");
        cx.style.row("root", &root_line(&cx.style, v));
    });
    Ok(())
}

pub fn overlay_reset(cx: &Ctx, cancel: bool) -> Result<(), String> {
    let args: &[&str] = if cancel { &["--cancel"] } else { &[] };

    let reply = match cx.ask("overlay", "reset", args)? {
        Some(v) => v,
        None => {
            need_root("overlay reset")?;
            let disk = overlay::request_reset(cancel)?;
            json!({ "reset_pending": !cancel, "disk": disk })
        }
    };

    cx.emit(&reply, |v| {
        let st = &cx.style;
        if is_true(v, "reset_pending") {
            println!(
                "{} all saved system changes are wiped at the next boot ({}).\n\
                 Cancel with `amctl overlay reset --cancel`.",
                st.red("overlay reset requested:"),
                text(v, "disk")
            );
        } else {
            println!("overlay reset {}", st.purple("cancelled"));
        }
    });
    Ok(())
}

// ------------------------------------------------------------- keymap ----

pub fn keymap_show(cx: &Ctx) -> Result<(), String> {
    let reply = match cx.ask("locale", "keymap", &[])? {
        Some(v) => v,
        None => offline_keymap(),
    };
    cx.emit(&reply, |v| print_keymap(cx, v));
    Ok(())
}

/// Plan B: configured layout, what the engine last loaded, and a live probe.
fn offline_keymap() -> Value {
    let (configured, _) = locale::load(Path::new("/"));
    let loaded = fs::read_to_string("/run/aftermath/keymap")
        .ok()
        .map(|s| s.trim().to_owned());

    json!({
        "configured": configured,
        "loaded": loaded,
        "probe_key_y": locale::probe_y_key().map(String::from),
    })
}

fn print_keymap(cx: &Ctx, v: &Value) {
    let st = &cx.style;
    let configured = &v["configured"];
    st.header("keyboard");

    st.row("console", text(configured, "keymap"));

    let loaded = match v["loaded"].as_str() {
        Some(k) => st.purple(k),
        None => st.red("no (kernel default US layout)"),
    };
    st.row("loaded", &loaded);

    let xkb = format!(
        "{} {}",
        text(configured, "xkb_layout"),
        text(configured, "xkb_variant")
    );
    st.row("X11/Wayland", &xkb);

    // Keycode 21 is Y on US keyboards and Z on German ones.
    if let Some(key) = v["probe_key_y"].as_str() {
        st.row("check", &format!("the key right of T types `{key}`"));
    }

    let hint = "configured in /etc/aftermath/aftermath.toml [locale]";
    println!("  {}", st.dim(hint));
}

pub fn keymap_set(cx: &Ctx, name: &str) -> Result<(), String> {
    let reply = match cx.ask("locale", "set", &[name])? {
        Some(v) => v,
        None => {
            need_root("keymap set")?;
            let how = locale::apply_keymap(Path::new("/"), name)?;
            json!({ "keymap": name, "how": how })
        }
    };

    cx.emit(&reply, |v| {
        let keymap = cx.style.purple(text(v, "keymap"));
        println!("keymap {keymap} loaded ({}) and saved", text(v, "how"));
    });
    Ok(())
}

// -------------------------------------------------------------- power ----

/// `poweroff` / `reboot`: via the engine, else signal PID 1 ourselves, and
/// as a last resort the reboot syscall.
pub fn power(cx: &Ctx, verb: &str) -> Result<(), String> {
    if cx.ask("system", verb, &[])?.is_some() {
        if !cx.json {
            println!("{}", cx.style.purple(&format!("{verb}...")));
        }
        return Ok(());
    }

    need_root(verb)?;
    let poweroff = verb == "poweroff";

    // aftermath-init (PID 1) shuts down cleanly on these signals.
    let signal = if poweroff {
        Signal::SIGUSR2
    } else {
        Signal::SIGINT
    };
    if kill(Pid::from_raw(1), signal).is_ok() {
        println!("{verb}: asked PID 1");
        return Ok(());
    }

    nix::unistd::sync();
    let mode = if poweroff {
        RebootMode::RB_POWER_OFF
    } else {
        RebootMode::RB_AUTOBOOT
    };
    // Only returns if it failed.
    let err = reboot(mode).unwrap_err();
    Err(format!("reboot syscall: {err}"))
}

// --------------------------------------------------------- subsystems ----

pub fn subsystems(cx: &Ctx) -> Result<(), String> {
    let reply = cx
        .ask("system", "subsystems", &[])?
        .ok_or_else(|| need_engine("subsystems"))?;

    cx.emit(&reply, |list| {
        cx.style.header("subsystems");
        for sub in list.as_array().into_iter().flatten() {
            cx.style.row(text(sub, "name"), text(sub, "about"));
        }
    });
    Ok(())
}
