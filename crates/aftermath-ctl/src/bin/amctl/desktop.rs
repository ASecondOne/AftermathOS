//! `amctl desktop check`: show the report even when prerequisites are missing.

use std::path::Path;
use std::process::ExitCode;

use aftermath_ctl::desktop;
use serde_json::Value;

use crate::cli::DesktopCmd;
use crate::output::{items, need_engine, text, Ctx};

pub fn run(cx: &Ctx, cmd: DesktopCmd) -> Result<ExitCode, String> {
    let root = match cmd {
        DesktopCmd::Status => {
            let v = cx
                .ask("desktop", "status", &[])?
                .ok_or_else(|| need_engine("desktop status"))?;
            cx.emit(&v, |v| {
                cx.style.header("graphical desktop");
                cx.style.row("state", text(v, "state"));
                cx.style.row("session", text(v, "session"));
                cx.style.row(
                    "login1",
                    if v["login1"]["responding"] == true {
                        "elogind responding"
                    } else {
                        "waiting for elogind"
                    },
                );
                cx.style
                    .row("greeter", text(&v["display_manager"], "provider"));
                cx.style.row("user bus", text(v, "user_bus"));
                cx.style.row("log", text(v, "log"));
                if let Some(e) = v["error"].as_str() {
                    cx.style.row("error", e);
                }
            });
            return Ok(if v["state"] == "running" {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            });
        }
        DesktopCmd::Check { root } => root,
        DesktopCmd::Bus => {
            let v = cx
                .ask("desktop", "bus", &[])?
                .ok_or_else(|| need_engine("desktop bus"))?;
            cx.emit(&v, |v| print_bus(cx, v));
            return Ok(if v["state"] == "running" && v["responding"] == true {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            });
        }
        DesktopCmd::Devices => {
            let v = cx
                .ask("desktop", "devices", &[])?
                .ok_or_else(|| need_engine("desktop devices"))?;
            cx.emit(&v, |v| print_devices(cx, v));
            return Ok(if v["state"] == "running" {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            });
        }
    };
    let report = match root {
        Some(root) => desktop::check(&root)?,
        None => match cx.ask("desktop", "check", &[])? {
            Some(v) => v,
            None => desktop::check(Path::new("/"))?,
        },
    };
    cx.emit(&report, |v| print(cx, v));
    Ok(if report["ready"].as_bool() == Some(true) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    })
}

fn print(cx: &Ctx, v: &Value) {
    let st = &cx.style;
    st.header("desktop check: Plasma Wayland");
    st.row("root", text(v, "root"));
    st.row("ready", if v["ready"] == true { "yes" } else { "no" });
    println!("  {}", text(v, "scope"));
    if v["devices"].is_object() {
        print_devices(cx, &v["devices"]);
    }
    if v["bus"].is_object() {
        print_bus(cx, &v["bus"]);
    }
    println!("\n  Installed programs (present means executable file found):");
    for c in items(v, "checks") {
        println!("  {:<8} {}", text(c, "state"), text(c, "id"));
        if text(c, "state") == "present" {
            let paths: Vec<_> = items(c, "found").filter_map(Value::as_str).collect();
            println!("           {}", paths.join(", "));
        } else {
            println!("           {}", text(c, "action"));
        }
        for e in items(c, "errors").filter_map(Value::as_str) {
            println!("           {e}");
        }
    }
    println!("\n  Compatibility limits and runtime checks:");
    for b in items(v, "blockers") {
        println!("  - {}: {}", text(b, "id"), text(b, "detail"));
    }
    println!("\n  Next step: {}", text(v, "next_step"));
    println!("\n  {}", text(v, "runtime_validation"));
}

fn print_bus(cx: &Ctx, v: &Value) {
    let st = &cx.style;
    st.header("system D-Bus");
    st.row("state", text(v, "state"));
    if let Some(pid) = v["pid"].as_i64() {
        st.row("pid", &pid.to_string());
    }
    st.row(
        "responding",
        if v["responding"] == true { "yes" } else { "no" },
    );
    st.row("provider", text(v, "provider"));
    st.row("address", text(v, "address"));
    st.row("config", text(v, "config"));
    st.row("machine ID", text(v, "machine_id"));
    st.row("restarts", &v["restarts"].to_string());
    if let Some(error) = v["error"].as_str() {
        st.row("error", error);
    }
    st.row("log", text(v, "log"));
    st.row("user bus", text(v, "session_bus"));
}

fn print_devices(cx: &Ctx, v: &Value) {
    let st = &cx.style;
    st.header("device discovery");
    st.row("state", text(v, "state"));
    if let Some(pid) = v["pid"].as_i64() {
        st.row("pid", &pid.to_string());
    }
    st.row("provider", text(v, "provider"));
    st.row(
        "responding",
        if v["responding"] == true { "yes" } else { "no" },
    );
    st.row("coldplug", text(v, "coldplug"));
    st.row("phase", text(v, "phase"));
    st.row("restarts", &v["restarts"].to_string());
    if let Some(error) = v["error"].as_str() {
        st.row("error", error);
    }
    st.row("log", text(v, "log"));
    st.row("seat access", text(v, "seat_access"));
}
