//! `amctl status`: the one-screen overview.

use aftermath_ctl::facts;
use serde_json::{json, Value};

use crate::output::{is_true, items, text, Ctx, Style};

pub fn run(cx: &Ctx) -> Result<(), String> {
    let status = match cx.ask("system", "status", &[])? {
        Some(v) => v,
        None => offline_status(),
    };

    cx.emit(&status, |v| print(&cx.style, v));
    Ok(())
}

/// Plan B: the same facts, read directly; services are unknown.
fn offline_status() -> Value {
    let mut v = facts::collect();
    v["engine"] = json!({ "running": false });
    v
}

fn print(st: &Style, v: &Value) {
    st.header("status");

    st.row("mode", &mode_line(st, text(v, "mode")));
    st.row("engine", &engine_line(st, &v["engine"]));
    st.row("root", &root_line(st, &v["overlay"]));
    st.row("network", &network_line(st, &v["network"]));
    st.row("strata", &strata_line(st, v));
    st.row("services", &services_line(st, &v["services"]));
    if let Some(state) = v["desktop"]["devices"]["state"].as_str() {
        st.row(
            "devices",
            &format!(
                "{state}; coldplug {}",
                text(&v["desktop"]["devices"], "coldplug")
            ),
        );
    }
    if let Some(state) = v["desktop"]["bus"]["state"].as_str() {
        st.row("system bus", state);
    }
    if let Some(active) = v["sessions"]["active"].as_u64() {
        st.row(
            "sessions",
            &format!("{active} authenticated; {} users", v["sessions"]["users"]),
        );
    }

    let keymap = v["keymap"].as_str().unwrap_or("kernel default (US)");
    st.row("keymap", keymap);

    st.row("uptime", &uptime_line(st, v));
    if let Some(dir) = v["log_dir"].as_str() {
        st.row("logs", dir);
    }
}

fn mode_line(st: &Style, mode: &str) -> String {
    match mode {
        "full" => st.purple("full (Arch host + strata)"),
        "host" => st.red("host (strata off: plan B)"),
        "busybox" => st.red("busybox rescue"),
        other => other.to_owned(),
    }
}

fn engine_line(st: &Style, engine: &Value) -> String {
    if is_true(engine, "running") {
        format!("{} (pid {})", st.purple("running"), engine["pid"])
    } else {
        st.red("NOT reachable (offline view)")
    }
}

/// Persistent overlay (with disk usage) or RAM root, plus a pending reset.
pub fn root_line(st: &Style, overlay: &Value) -> String {
    let mut line = if is_true(overlay, "persistent") {
        let device = overlay["device"].as_str().unwrap_or("?");
        let usage = usage_note(overlay);
        format!("{} (overlay disk {device}{usage})", st.purple("persistent"))
    } else {
        st.red("RAM only (changes are lost at poweroff)")
    };

    if is_true(overlay, "reset_pending") {
        line.push_str(&format!("  {}", st.red("[reset at next boot]")));
    }
    line
}

/// ", 12% of 1945 MiB used" or nothing.
fn usage_note(overlay: &Value) -> String {
    let used = overlay["used_bytes"].as_u64();
    let size = overlay["size_bytes"].as_u64();

    match (used, size) {
        (Some(used), Some(size)) if size > 0 => {
            let percent = used * 100 / size;
            format!(", {percent}% of {} MiB used", size >> 20)
        }
        _ => String::new(),
    }
}

fn network_line(st: &Style, net: &Value) -> String {
    let online = if is_true(net, "online") {
        st.purple("online")
    } else {
        st.red("offline")
    };

    let interfaces: Vec<String> = items(net, "interfaces")
        .map(|i| format!("{} {}", text(i, "name"), text(i, "ipv4")))
        .collect();

    let mut line = format!("{online}  {}", interfaces.join(", "));

    if let Some(gw) = net["gateway"].as_str() {
        line.push_str(&format!("  gw {gw}"));
    }

    let dns: Vec<&str> = items(net, "dns").filter_map(Value::as_str).collect();
    if !dns.is_empty() {
        line.push_str(&format!("  dns {}", dns.join(" ")));
    }
    line
}

fn strata_line(st: &Style, v: &Value) -> String {
    let guests: Vec<String> = items(v, "strata")
        .map(|s| format!("{} {}", text(s, "name"), st.state(text(s, "state"))))
        .collect();

    if guests.is_empty() {
        st.dim("host only")
    } else {
        format!("arch (host) {}, {}", st.purple("up"), guests.join(", "))
    }
}

fn services_line(st: &Style, services: &Value) -> String {
    // Only the engine knows about running services.
    if !services.is_object() {
        return st.dim("unknown (needs the engine)");
    }

    let failed = services["failed"].as_u64().unwrap_or(0);
    let failed = if failed > 0 {
        st.red(&failed.to_string())
    } else {
        "0".into()
    };

    format!(
        "{} running, {failed} failed, {} total",
        services["running"], services["total"]
    )
}

fn uptime_line(st: &Style, v: &Value) -> String {
    let details = format!("kernel {}  boot {}", text(v, "kernel"), text(v, "boot_id"));
    format!("{}  {}", text(v, "uptime"), st.dim(&details))
}
