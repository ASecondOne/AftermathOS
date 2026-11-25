//! amctl - Aftermath control: the single front door to the running system
//! (the systemctl of Aftermath).
//!
//! It talks to aftermath-engine over /run/aftermath/control.sock. Anyone may
//! ask (status, lists, logs); changing things needs root. If the engine or
//! its socket is not there (BusyBox rescue, engine crashed), amctl says so
//! and does what it can by itself: status from /proc and /run, logs, boot
//! next, overlay reset, keymap, and poweroff/reboot via PID 1 or the reboot
//! syscall (plan B).
//!
//! Layout: `cli` (the commands), `output` (engine calls, colours, JSON),
//! then one module per area.

mod cli;
mod logs;
mod output;
mod services;
mod status;
mod strata;
mod system;

use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use serde_json::json;

use cli::{BootCmd, Cli, Cmd, KeymapCmd, OverlayCmd};
use output::Ctx;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cx = Ctx::new(cli.json, cli.no_color);

    match run(&cx, cli.cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            report_error(&cx, &e);
            ExitCode::FAILURE
        }
    }
}

fn report_error(cx: &Ctx, err: &str) {
    if cx.json {
        println!("{}", json!({ "ok": false, "error": err }));
    } else {
        eprintln!("{} {err}", cx.style.red("amctl:"));
    }
}

fn run(cx: &Ctx, cmd: Cmd) -> Result<(), String> {
    match cmd {
        Cmd::Status => status::run(cx),
        Cmd::Strata(c) => strata::run(cx, c),
        Cmd::Services(c) => services::run(cx, c),
        Cmd::Logs(args) => logs::run(cx, args),

        Cmd::Poweroff => system::power(cx, "poweroff"),
        Cmd::Reboot => system::power(cx, "reboot"),

        Cmd::Boot(BootCmd::Next { target }) => system::boot_next(cx, &target),
        Cmd::Boot(BootCmd::Status) => system::boot_status(cx),

        Cmd::Overlay(OverlayCmd::Status) => system::overlay_status(cx),
        Cmd::Overlay(OverlayCmd::Reset { cancel }) => system::overlay_reset(cx, cancel),

        Cmd::Keymap { cmd: None } => system::keymap_show(cx),
        Cmd::Keymap {
            cmd: Some(KeymapCmd::Set { name }),
        } => system::keymap_set(cx, &name),

        Cmd::Subsystems => system::subsystems(cx),
        Cmd::Completions { shell } => {
            let mut out = std::io::stdout();
            clap_complete::generate(shell, &mut Cli::command(), "amctl", &mut out);
            Ok(())
        }
    }
}
