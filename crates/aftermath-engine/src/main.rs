//! aftermath-engine: the part of the Aftermath init that is allowed to fail.
//!
//! PID 1 (aftermath-init) starts this as `aftermath-engine --mode full|host`
//! and restarts it / falls back to the next layer when it dies. The engine:
//!
//! * starts the strata (full mode). If that fails it logs it and keeps going
//!   in plain Arch host mode: plan B is always a working Arch system.
//! * loads the console keymap (/etc/aftermath/aftermath.toml [locale])
//! * keeps a root shell on every console
//! * runs the control socket for `amctl` (control.rs) and supervises the
//!   services from /etc/aftermath/services (services.rs)
//! * marks the boot as successful in GRUB's grubenv, which resets GRUB's
//!   boot counter (see grubenv.rs)
//!
//! Test hooks on the kernel command line (`aftermath.test=<name>`):
//! `engine-crash` (crash in full mode), `engine-crash-always` (crash in every
//! mode), `strata-fail`, `no-success` (never mark the boot good), `panic`
//! (trigger a kernel panic, to test `panic=` reboots + GRUB boot counting).

mod control;
mod logs;
mod network;
mod services;
mod shells;
mod strata;
mod subsystems;

use aftermath_ctl::grubenv;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::thread::sleep;
use std::time::Duration;

pub const RED: &str = "\x1b[1;31m";
pub const PURPLE: &str = "\x1b[1;35m";
pub const RESET: &str = "\x1b[0m";

pub fn log(msg: &str) {
    log_at(6, msg);
}

/// Failures, fallbacks and test hooks: level 3 so they show with `loglevel=4`.
pub fn warn(msg: &str) {
    log_at(3, msg);
}

fn log_at(level: u8, msg: &str) {
    let line = format!("<{level}>aftermath-engine: {msg}\n");
    match OpenOptions::new().write(true).open("/dev/kmsg") {
        Ok(mut k) => {
            let _ = k.write_all(line.as_bytes());
        }
        Err(_) => eprint!("{line}"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Full,
    Host,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut mode = Mode::Full;
    while let Some(a) = args.next() {
        if a == "--mode" {
            mode = match args.next().as_deref() {
                Some("host") => Mode::Host,
                _ => Mode::Full,
            };
        }
    }

    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let param = |key: &str| {
        cmdline
            .split_whitespace()
            .find_map(|a| a.strip_prefix(key).map(str::to_owned))
    };
    let test = param("aftermath.test=").unwrap_or_default();
    if let Some(n) = param("aftermath.bootfails=").filter(|n| n != "0") {
        warn(&format!(
            "GRUB reports {n} unconfirmed boot(s) before this one"
        ));
    }

    // ---- test hooks -----------------------------------------------------
    if test == "panic" {
        warn("TEST: triggering a kernel panic in 2s (expect reboot via panic=)");
        sleep(Duration::from_secs(2));
        let _ = fs::write("/proc/sys/kernel/sysrq", "1");
        let _ = fs::write("/proc/sysrq-trigger", "c");
    }
    if test == "engine-crash-always" || (test == "engine-crash" && mode == Mode::Full) {
        warn("TEST: simulating an engine crash");
        std::process::abort();
    }

    // ---- keyboard (first, so every console starts with the right layout)
    let _ = fs::create_dir_all("/run/aftermath");
    let keymap = subsystems::keyboard_boot();

    // ---- network (before strata: apt/pacman want it) ------------------
    network::start();

    // ---- strata ---------------------------------------------------------
    let mut effective = mode;
    if mode == Mode::Full {
        match strata::start(test == "strata-fail") {
            Ok(()) => log("strata up"),
            Err(e) => {
                warn(&format!(
                    "strata FAILED: {e} -> continuing in plain Arch host mode"
                ));
                effective = Mode::Host;
            }
        }
    } else {
        log("Arch host mode: strata disabled");
    }
    let mode_name = if effective == Mode::Full {
        "full"
    } else {
        "host"
    };
    let _ = fs::write("/run/aftermath/mode", format!("{mode_name}\n"));

    // ---- consoles -------------------------------------------------------
    let log_dir = logs::session_dir();
    if let Some(d) = &log_dir {
        let _ = fs::write(aftermath_ctl::LOGDIR_FILE, format!("{}\n", d.display()));
        logs::start_kmsg_tap(d);
        log(&format!(
            "session logs (consoles + kernel log) in {}",
            d.display()
        ));
    }
    network::wait_online();
    let mut shells = shells::Shells::default();
    if !shells.start_all(mode_name, log_dir.clone()) {
        warn("no usable shell (is /usr/bin/bash broken?) - exiting so PID 1 can fall back");
        std::process::exit(3);
    }

    // ---- boot is good ---------------------------------------------------
    if test == "no-success" {
        warn("TEST: not marking the boot as successful");
    } else {
        match grubenv::mark_boot_success() {
            Ok(Some(dev)) => log(&format!("boot marked successful in grubenv on {dev}")),
            Ok(None) => log("no GRUB environment found (ISO / direct boot): nothing to mark"),
            Err(e) => warn(&format!("could not mark boot successful: {e}")),
        }
    }
    // ---- control socket + services ---------------------------------------
    let jobs = control::start();
    let host =
        aftermath_strata::config::Config::load(std::path::Path::new(aftermath_strata::CONFIG))
            .map(|c| c.host_name().to_owned())
            .unwrap_or_else(|_| "arch".into());
    let mut svc = services::Services::new(log_dir.clone(), host);
    svc.boot();
    let mut registry = control::Registry {
        subsystems: vec![
            Box::new(subsystems::System),
            Box::new(subsystems::Strata),
            Box::new(svc),
            Box::new(subsystems::Boot),
            Box::new(subsystems::Overlay),
            Box::new(subsystems::Locale { loaded: keymap }),
        ],
    };
    log(&format!("up ({mode_name} mode)"));

    event_loop(&mut shells, &mut registry, jobs)
}

/// The one place children are reaped: shells, services, everything. Between
/// reaps it answers amctl requests and runs subsystem timers.
fn event_loop(
    shells: &mut shells::Shells,
    registry: &mut control::Registry,
    jobs: Option<std::sync::mpsc::Receiver<control::Job>>,
) -> ! {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;
    loop {
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) | Err(_) => break,
                Ok(st) => {
                    let Some(pid) = st.pid() else { break };
                    if !shells.child_exited(pid.as_raw()) {
                        registry.child_exited(pid.as_raw(), &st);
                    }
                }
            }
        }
        registry.tick();
        match &jobs {
            Some(rx) => match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(job) => {
                    let resp = registry.handle(&job.req, job.uid);
                    let _ = job.reply.send(resp);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    sleep(Duration::from_millis(250))
                }
            },
            None => sleep(Duration::from_millis(250)),
        }
    }
}
