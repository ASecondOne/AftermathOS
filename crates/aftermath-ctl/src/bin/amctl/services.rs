//! `amctl services ...`: the services the engine supervises.

use std::path::Path;

use aftermath_ctl::services::load_dir;
use aftermath_ctl::SERVICES_DIR;
use serde_json::{json, Value};

use crate::cli::SvcCmd;
use crate::output::{items, need_engine, on_off, text, Ctx, Style};

pub fn run(cx: &Ctx, cmd: SvcCmd) -> Result<(), String> {
    let (verb, name) = match cmd {
        SvcCmd::List => ("list", None),
        SvcCmd::Reload => ("reload", None),
        SvcCmd::Status { name } => ("status", Some(name)),
        SvcCmd::Start { name } => ("start", Some(name)),
        SvcCmd::Stop { name } => ("stop", Some(name)),
        SvcCmd::Restart { name } => ("restart", Some(name)),
        SvcCmd::Enable { name } => ("enable", Some(name)),
        SvcCmd::Disable { name } => ("disable", Some(name)),
    };

    let args: Vec<&str> = name.as_deref().into_iter().collect();
    let reply = match cx.ask("services", verb, &args)? {
        Some(v) => v,
        None if verb == "list" => offline_list(),
        None => return Err(need_engine(&format!("services {verb}"))),
    };

    cx.emit(&reply, |v| match verb {
        "list" => print_list(&cx.style, v),
        "reload" => print_reload(v),
        _ => print_one(&cx.style, v),
    });
    Ok(())
}

/// Plan B: the service files, without any runtime state.
fn offline_list() -> Value {
    let (defs, broken) = load_dir(Path::new(SERVICES_DIR));

    let services: Vec<Value> = defs
        .iter()
        .map(|d| {
            json!({
                "name": d.name,
                "state": "unknown",
                "description": d.description,
                "enabled": d.enabled,
            })
        })
        .collect();

    let broken: Vec<Value> = broken
        .iter()
        .map(|(file, error)| json!({ "file": file, "error": error }))
        .collect();

    json!({ "services": services, "broken": broken })
}

fn print_list(st: &Style, v: &Value) {
    st.header("services");

    let services: Vec<&Value> = items(v, "services").collect();
    if services.is_empty() {
        let hint = format!("no services (put *.toml files in {SERVICES_DIR})");
        println!("  {}", st.dim(&hint));
    } else {
        let heading = format!(
            "{:<16} {:<11} {:<8} {:<7} {}",
            "NAME", "STATE", "PID", "BOOT", "DESCRIPTION"
        );
        println!("  {}", st.dim(&heading));
    }

    for s in services {
        let pid = s["pid"].as_i64().map_or("-".to_string(), |p| p.to_string());
        println!(
            "  {} {} {:<8} {:<7} {}",
            st.bold(&format!("{:<16}", text(s, "name"))), // pad before colouring
            st.state(&format!("{:<11}", text(s, "state"))),
            pid,
            on_off(s),
            text(s, "description")
        );
    }

    for b in items(v, "broken") {
        println!(
            "  {} {}: {}",
            st.red("broken"),
            text(b, "file"),
            text(b, "error")
        );
    }
}

fn print_reload(v: &Value) {
    println!(
        "loaded {} service file(s), {} broken",
        v["loaded"], v["broken"]
    );
}

fn print_one(st: &Style, s: &Value) {
    st.header(&format!("service {}", text(s, "name")));

    // Rows that only appear when the field has a value.
    let optional_row = |key: &str, field: &str| {
        if !text(s, field).is_empty() {
            st.row(key, text(s, field));
        }
    };

    optional_row("about", "description");
    st.row("state", &state_line(st, s));
    if !text(s, "why").is_empty() {
        st.row("why", &st.red(text(s, "why")));
    }

    if let Some(exec) = s["exec"].as_array() {
        let words: Vec<&str> = exec.iter().filter_map(Value::as_str).collect();
        st.row("exec", &words.join(" "));
    }
    optional_row("stratum", "stratum");

    let restart = format!("{} ({} restarts so far)", text(s, "restart"), s["restarts"]);
    st.row("restart", &restart);
    st.row("at boot", on_off(s));

    optional_row("last exit", "last_exit");
    optional_row("note", "note");
    optional_row("log", "log");
}

/// "running (pid 212) for 3m 2s"
fn state_line(st: &Style, s: &Value) -> String {
    let mut line = st.state(text(s, "state"));

    if let Some(pid) = s["pid"].as_i64() {
        line.push_str(&format!(" (pid {pid})"));
    }
    if !text(s, "for").is_empty() {
        line.push_str(&format!(" for {}", text(s, "for")));
    }
    line
}
