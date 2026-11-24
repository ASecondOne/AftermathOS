//! The control socket (/run/aftermath/control.sock) and the subsystem
//! registry behind `amctl`.
//!
//! A listener thread accepts connections; every request is handed to the
//! engine's main loop over a channel (so all state lives in one thread and
//! child processes are reaped in one place) and the answer goes back the
//! same way. Each connection gets its own small thread, so a stuck client
//! never blocks the others.
//!
//! Authorization: SO_PEERCRED gives the caller's uid. Verbs a subsystem
//! lists in `queries()` are open to everyone; everything else needs root.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use aftermath_ctl::proto::{Request, Response, MAX_LINE, TIMEOUT};
use aftermath_ctl::SOCKET;
use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use nix::sys::wait::WaitStatus;
use serde_json::{json, Value};

/// A part of the system amctl can talk to. New subsystems (Windows
/// stratum, D-Bus/logind shim, ...) implement this and get registered in
/// main.rs; nothing else in the engine needs to change.
pub trait Subsystem {
    fn name(&self) -> &'static str;
    /// One line for `amctl help` / `system subsystems`.
    fn about(&self) -> &'static str;
    /// Read-only verbs: allowed for every user.
    fn queries(&self) -> &'static [&'static str];
    fn call(&mut self, verb: &str, args: &[String]) -> Result<Value, String>;
    /// Added to `amctl status` under the subsystem's name.
    fn summary(&self) -> Option<Value> {
        None
    }
    /// A child exited; return true if it was one of ours.
    fn child_exited(&mut self, _pid: i32, _status: &WaitStatus) -> bool {
        false
    }
    /// Called about 4x a second (timers, delayed restarts).
    fn tick(&mut self) {}
}

pub struct Job {
    pub req: Request,
    pub uid: u32,
    pub reply: Sender<Response>,
}

pub fn start() -> Option<Receiver<Job>> {
    let _ = std::fs::create_dir_all(aftermath_ctl::RUN_DIR);
    let _ = std::fs::remove_file(SOCKET);
    let listener = match UnixListener::bind(SOCKET) {
        Ok(l) => l,
        Err(e) => {
            crate::warn(&format!(
                "control socket {SOCKET}: {e} (amctl works offline only)"
            ));
            return None;
        }
    };
    let _ = std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o666));
    let (tx, rx) = mpsc::channel::<Job>();
    let spawned = std::thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            for conn in listener.incoming().flatten() {
                let tx = tx.clone();
                let _ = std::thread::Builder::new()
                    .name("control-conn".into())
                    .spawn(move || serve(conn, tx));
            }
        });
    match spawned {
        Ok(_) => {
            crate::log(&format!("control socket on {SOCKET}"));
            Some(rx)
        }
        Err(e) => {
            crate::warn(&format!("control thread: {e}"));
            None
        }
    }
}

fn serve(conn: UnixStream, tx: Sender<Job>) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let uid = match getsockopt(&conn, PeerCredentials) {
        Ok(c) => c.uid(),
        Err(_) => u32::MAX, // unknown caller: queries only
    };
    let mut line = String::new();
    let mut reader = BufReader::new((&conn).take(MAX_LINE as u64));
    let resp = match reader.read_line(&mut line) {
        Ok(0) | Err(_) => return,
        Ok(_) => match serde_json::from_str::<Request>(&line) {
            Err(e) => Response::err(format!("bad request: {e}")),
            Ok(req) => {
                let (rtx, rrx) = mpsc::channel();
                if tx
                    .send(Job {
                        req,
                        uid,
                        reply: rtx,
                    })
                    .is_err()
                {
                    Response::err("the engine is shutting down")
                } else {
                    rrx.recv_timeout(TIMEOUT)
                        .unwrap_or_else(|_| Response::err("the engine did not answer in time"))
                }
            }
        },
    };
    let mut out = serde_json::to_string(&resp).unwrap_or_else(|_| "{\"ok\":false}".into());
    out.push('\n');
    let _ = (&conn).write_all(out.as_bytes());
}

pub struct Registry {
    pub subsystems: Vec<Box<dyn Subsystem>>,
}

impl Registry {
    pub fn handle(&mut self, req: &Request, uid: u32) -> Response {
        // The engine-wide status: facts + every subsystem's summary.
        if req.subsystem == "system" && req.verb == "status" {
            let mut v = aftermath_ctl::facts::collect();
            v["engine"] = json!({"running": true, "pid": std::process::id()});
            for s in &self.subsystems {
                if let Some(sum) = s.summary() {
                    v[s.name()] = sum;
                }
            }
            return Response::ok(v);
        }
        if req.subsystem == "system" && req.verb == "subsystems" {
            let list: Vec<Value> = self
                .subsystems
                .iter()
                .map(|s| json!({"name": s.name(), "about": s.about(), "queries": s.queries()}))
                .collect();
            return Response::ok(json!(list));
        }
        let Some(sub) = self
            .subsystems
            .iter_mut()
            .find(|s| s.name() == req.subsystem)
        else {
            return Response::err(format!("unknown subsystem `{}`", req.subsystem));
        };
        if uid != 0 && !sub.queries().contains(&req.verb.as_str()) {
            return Response::err(format!(
                "permission denied: `amctl {} {}` changes the system and needs root",
                req.subsystem, req.verb
            ));
        }
        match sub.call(&req.verb, &req.args) {
            Ok(v) => Response::ok(v),
            Err(e) => Response::err(e),
        }
    }

    pub fn child_exited(&mut self, pid: i32, st: &WaitStatus) -> bool {
        self.subsystems.iter_mut().any(|s| s.child_exited(pid, st))
    }

    pub fn tick(&mut self) {
        for s in &mut self.subsystems {
            s.tick();
        }
    }
}
