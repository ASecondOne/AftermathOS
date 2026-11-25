//! `amctl which <cmd>` and `amctl default ...`: which stratum runs a
//! command, every copy of it, and choosing the winner by hand.
//!
//! `which` and `default list` only read files, so they never need the
//! engine. Changes go through the engine when it runs and are done
//! directly otherwise (plan B); either way they need root.

use aftermath_ctl::defaults;
use serde_json::Value;

use crate::cli::{DefaultArgs, DefaultSub};
use crate::output::{is_true, items, need_root, text, Ctx, Style};

/// Exits with status 1 when no running stratum has `cmd`, like which(1).
pub fn which(cx: &Ctx, cmd: &str) -> Result<(), String> {
    let report = defaults::which(cmd)?;

    cx.emit(&report, |v| print_which(&cx.style, v));

    if items(&report, "copies").next().is_none() {
        std::process::exit(1);
    }
    Ok(())
}

pub fn default(cx: &Ctx, args: DefaultArgs) -> Result<(), String> {
    match (args.sub, args.cmd) {
        (Some(DefaultSub::List), _) => {
            let v = defaults::list()?;
            cx.emit(&v, |v| print_list(&cx.style, v));
        }

        (Some(DefaultSub::Order { strata, reset }), _) => {
            let names = if reset { Vec::new() } else { strata };
            if names.is_empty() && !reset {
                return Err("give the strata in order (e.g. `ubuntu arch`), or --reset".into());
            }
            let v = change(cx, "order", &names)?;
            cx.emit(&v, |v| print_order(&cx.style, v));
        }

        (None, Some(cmd)) => {
            let v = if args.reset {
                change(cx, "reset", &[cmd])?
            } else {
                let Some(stratum) = args.stratum else {
                    return Err(format!(
                        "which stratum should run `{cmd}`? e.g. `amctl default {cmd} ubuntu`, \
                         or `amctl default {cmd} --reset`"
                    ));
                };
                change(cx, "set", &[cmd, stratum])?
            };
            cx.emit(&v, |v| print_changed(&cx.style, v));
        }

        (None, None) => {
            let v = defaults::list()?;
            cx.emit(&v, |v| print_list(&cx.style, v));
        }
    }
    Ok(())
}

/// Ask the engine; without it, do the same change here (root only).
fn change(cx: &Ctx, verb: &str, args: &[String]) -> Result<Value, String> {
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Some(v) = cx.ask("default", verb, &arg_refs)? {
        return Ok(v);
    }

    need_root("default")?;
    match verb {
        "set" => defaults::set(&args[0], &args[1]),
        "reset" => defaults::reset(&args[0]),
        _ => defaults::set_order(args),
    }
}

// ----------------------------------------------------------- printing ----

fn print_which(st: &Style, v: &Value) {
    let cmd = text(v, "command");
    st.header(&format!("which {cmd}"));

    let winner = v.get("winner").filter(|w| !w.is_null());
    let winner_name = winner.map(|w| text(w, "stratum")).unwrap_or("");

    if items(v, "copies").next().is_none() {
        println!("  {}", st.red(&format!("no stratum has `{cmd}`")));
    }

    for c in items(v, "copies") {
        print_copy(st, c, winner_name);
    }
    println!();

    match winner {
        Some(w) => st.row(
            "winner",
            &format!("{}  {}", st.bold(winner_name), st.dim(text(w, "why"))),
        ),
        None => st.row("winner", "none"),
    }
    st.row("runs via", &runs_via(v));

    for p in items(v, "passed_over") {
        let line = format!(
            "{} points to {}, which has no `{cmd}`",
            text(p, "why"),
            text(p, "stratum")
        );
        st.row("skipped", &st.dim(&line));
    }
    for s in items(v, "not_checked") {
        st.row(
            "unchecked",
            &st.dim(&format!("{} ({})", text(s, "stratum"), text(s, "why"))),
        );
    }

    if is_true(v, "stale") {
        let hint = "launchers are out of date: /usr/lib/aftermath/aftermath-strata refresh";
        println!("  {}", st.red(hint));
    }
}

/// `* ubuntu   /usr/bin/vi  -> /usr/bin/vim.basic   (file on the host)`
fn print_copy(st: &Style, c: &Value, winner: &str) {
    let stratum = text(c, "stratum");
    let wins = stratum == winner && is_true(c, "first_in_stratum");

    let marker = if wins { st.red("*") } else { " ".into() };
    let name = format!("{stratum:<9}"); // pad before colouring
    let name = if wins { st.purple(&name) } else { name };

    let mut line = format!("{marker} {name} {}", text(c, "path"));
    if text(c, "real") != text(c, "path") {
        line.push_str(&format!(" -> {}", text(c, "real")));
    }
    if text(c, "file") != text(c, "real") {
        line.push_str(&format!("  {}", st.dim(&format!("({})", text(c, "file")))));
    }
    if !is_true(c, "first_in_stratum") {
        line.push_str(&format!("  {}", st.dim("(shadowed in its stratum)")));
    }
    println!("  {line}");
}

fn runs_via(v: &Value) -> String {
    let cmd = text(v, "command");
    match v.get("launcher").and_then(Value::as_str) {
        Some(stratum) => format!("launcher -> strat -> {stratum}"),
        None if v.get("runs_in").is_some_and(|r| !r.is_null()) => {
            format!("the host's PATH (no launcher for `{cmd}`)")
        }
        None => "nothing".into(),
    }
}

fn print_changed(st: &Style, v: &Value) {
    print_which(st, v);
    println!();

    if let Some(note) = v.get("note").and_then(Value::as_str) {
        println!("  {}", st.red(note));
    }
    st.row("saved in", text(v, "file"));
    st.row("launchers", text(v, "launchers"));
    println!(
        "  {}",
        st.dim("open Aftermath shells pick this up at their next prompt; elsewhere run `hash -r`")
    );
}

fn print_list(st: &Style, v: &Value) {
    st.header("command defaults");

    let mine = v.get("defaults").and_then(Value::as_object);
    match mine {
        Some(map) if !map.is_empty() => {
            for (cmd, stratum) in map {
                println!(
                    "  {:<20} {}",
                    cmd,
                    st.purple(stratum.as_str().unwrap_or(""))
                );
            }
        }
        _ => println!(
            "  {}",
            st.dim("none of yours yet (amctl default <cmd> <stratum>)")
        ),
    }
    println!();

    st.row("order", &join(v, "order"));
    if items(v, "user_order").next().is_some() {
        println!(
            "  {}",
            st.dim("(your order: amctl default order --reset to undo)")
        );
    }
    st.row("saved in", text(v, "file"));

    if let Some(pins) = v.get("shipped_pins").and_then(Value::as_object) {
        let list: Vec<String> = pins
            .iter()
            .map(|(p, s)| format!("{p}={}", s.as_str().unwrap_or("")))
            .collect();
        st.row("shipped", &st.dim(&list.join("  ")));
    }
}

fn print_order(st: &Style, v: &Value) {
    st.header("lookup order");
    st.row("order", &join(v, "order"));
    st.row("launchers", text(v, "launchers"));
}

/// An array of strings as `a, b, c`.
fn join(v: &Value, key: &str) -> String {
    let names: Vec<&str> = items(v, key).filter_map(Value::as_str).collect();
    names.join(", ")
}
