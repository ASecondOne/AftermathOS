//! Talking to the engine and printing results: the shared context, the
//! red/purple style, JSON-or-human output and the common error messages.

use std::io::IsTerminal;

use aftermath_ctl::proto::{self, Request};
use aftermath_ctl::SOCKET;
use serde_json::Value;

/// What every command needs: output format and colours.
pub struct Ctx {
    /// Print raw JSON instead of the human view.
    pub json: bool,
    pub style: Style,
}

impl Ctx {
    /// Colours only for a terminal, and never with --json, --no-color or NO_COLOR.
    pub fn new(json: bool, no_color: bool) -> Self {
        let color = !no_color
            && !json
            && std::env::var_os("NO_COLOR").is_none()
            && std::io::stdout().is_terminal();

        Self {
            json,
            style: Style { on: color },
        }
    }

    /// Ask the engine. `Ok(None)` means it is not reachable: the caller
    /// then falls back to what it can do offline (plan B).
    pub fn ask(&self, subsystem: &str, verb: &str, args: &[&str]) -> Result<Option<Value>, String> {
        let req = Request {
            subsystem: subsystem.into(),
            verb: verb.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
        };

        match proto::call(SOCKET, &req) {
            Ok(reply) if reply.ok => Ok(Some(reply.data)),
            Ok(reply) => Err(reply.error.unwrap_or_else(|| "the engine refused".into())),
            Err(e) => {
                self.note_offline(&e);
                Ok(None)
            }
        }
    }

    fn note_offline(&self, err: &std::io::Error) {
        if !self.json {
            eprintln!(
                "{} the engine is not reachable ({SOCKET}: {err}); working offline",
                self.style.red("amctl:")
            );
        }
    }

    /// Print `v` as pretty JSON (--json) or with the given human printer.
    pub fn emit(&self, v: &Value, human: impl FnOnce(&Value)) {
        if self.json {
            println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
        } else {
            human(v);
        }
    }
}

/// The Aftermath look: red and purple on the (black) terminal.
pub struct Style {
    pub on: bool,
}

impl Style {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    }

    pub fn red(&self, s: &str) -> String {
        self.paint("1;31", s)
    }

    pub fn purple(&self, s: &str) -> String {
        self.paint("1;35", s)
    }

    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }

    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }

    /// `AFTERMATH <title>` heading.
    pub fn header(&self, title: &str) {
        println!("{} {}", self.red("AFTERMATH"), self.purple(title));
    }

    /// An indented `key   value` line.
    pub fn row(&self, key: &str, value: &str) {
        println!("  {}  {value}", self.purple(&format!("{key:<10}")));
    }

    /// A state word: running/up in purple, trouble in red, the rest dim.
    pub fn state(&self, state: &str) -> String {
        let word = state.trim();
        let good = word == "running" || word == "up";
        let bad = word.starts_with("failed") || word == "down" || word == "restarting";

        if good {
            self.purple(state)
        } else if bad {
            self.red(state)
        } else {
            self.dim(state)
        }
    }
}

// ------------------------------------------------------- JSON helpers ----

/// A string field, or "" if missing.
pub fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// A boolean field that is explicitly true.
pub fn is_true(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool) == Some(true)
}

/// The items of an array field (empty if missing).
pub fn items<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    v.get(key).and_then(Value::as_array).into_iter().flatten()
}

/// "on" / "off" / "?" for an `enabled` field.
pub fn on_off(v: &Value) -> &'static str {
    match v.get("enabled").and_then(Value::as_bool) {
        Some(true) => "on",
        Some(false) => "off",
        None => "?",
    }
}

// ------------------------------------------------------------- errors ----

/// Error for commands that only the engine can do.
pub fn need_engine(what: &str) -> String {
    format!(
        "`amctl {what}` needs the engine, which is not running \
         (BusyBox rescue or the engine crashed; see `amctl logs`)"
    )
}

/// Offline changes need root (the engine checks this itself when online).
pub fn need_root(what: &str) -> Result<(), String> {
    if nix::unistd::geteuid().is_root() {
        Ok(())
    } else {
        Err(format!("`amctl {what}` changes the system and needs root"))
    }
}
