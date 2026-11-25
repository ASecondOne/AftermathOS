//! amctl - Aftermath control: the single front door to the running system
//! (the systemctl of Aftermath).
//!
//! It talks to aftermath-engine over /run/aftermath/control.sock. Anyone may
//! ask (status, lists, logs); changing things needs root. If the engine or
//! its socket is not there (BusyBox rescue, engine crashed), amctl says so
//! and does what it can by itself: status from /proc and /run, logs, boot
//! next, overlay reset, keymap, which/default, and poweroff/reboot via PID 1 or the reboot
//! syscall (plan B).
//!
//! Layout: `cli` (the commands), `output` (engine calls, colours, JSON),
//! then one module per area.

mod cli;
mod desktop;
mod logs;
mod output;
mod services;
mod sessions;
mod status;
mod strata;
mod system;
mod users;
mod which;

use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use serde_json::json;

use cli::{BootCmd, Cli, Cmd, KeymapCmd, OverlayCmd};
use output::Ctx;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cx = Ctx::new(cli.json, cli.no_color);

    match run(&cx, cli.cmd) {
        Ok(code) => code,
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

fn run(cx: &Ctx, cmd: Cmd) -> Result<ExitCode, String> {
    match cmd {
        Cmd::List => strata::run(cx, cli::StrataCmd::List),
        Cmd::Enable { name } => strata::run(cx, cli::StrataCmd::Enable { name }),
        Cmd::Disable { name } => strata::run(cx, cli::StrataCmd::Disable { name }),
        Cmd::Status => status::run(cx),
        Cmd::Strata(c) => strata::run(cx, c),
        Cmd::Services(c) => services::run(cx, c),
        Cmd::Desktop(c) => return desktop::run(cx, c),
        Cmd::Sessions(c) => sessions::run(cx, c),
        Cmd::Users(c) => users::run(cx, c),
        Cmd::Logs(args) => logs::run(cx, args),
        Cmd::Which { cmd } => which::which(cx, &cmd),
        Cmd::Default(args) => which::default(cx, args),

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
    .map(|()| ExitCode::SUCCESS)
}
