use std::io::IsTerminal;
use std::path::Path;
use std::process::Command;

use aftermath_ctl::users;
use serde_json::{json, Value};

use crate::cli::UserCmd;
use crate::output::{items, need_engine, text, Ctx};

fn change(cx: &Ctx, verb: &str, args: &[&str]) -> Result<Value, String> {
    users::require_aftermath()?;
    cx.ask("users", verb, args)?
        .ok_or_else(|| need_engine("users"))
}

fn show(cx: &Ctx, report: &Value) {
    cx.emit(report, |v| {
        cx.style.header("user account");
        let u = &v["user"];
        for (key, value) in [("name", text(u,"name").to_owned()), ("uid", u["uid"].to_string()),
            ("gid", u["gid"].to_string()), ("home", text(u,"home").to_owned()),
            ("shell", text(u,"shell").to_owned()), ("password", text(u,"password_status").to_owned()),
            ("groups", items(u,"groups").filter_map(Value::as_str).collect::<Vec<_>>().join(", "))] {
            cx.style.row(key, &value);
        }
        if let Some(error) = v["sync_error"].as_str() {
            eprintln!("Account changed on the host; strata sync failed: {error}. Retry `amctl users sync`.");
        }
        if v["persistent"] == false {
            println!("  Temporary root: account changes will be lost at reboot.");
        }
    });
}

fn interactive(cx: &Ctx) -> Result<(), String> {
    if cx.json || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        Err("password setup needs an interactive terminal without --json; use `users add NAME --locked` to create without prompting".into())
    } else {
        Ok(())
    }
}

fn passwd(cx: &Ctx, name: Option<&str>) -> Result<(), String> {
    users::require_aftermath()?;
    interactive(cx)?;
    let uid = nix::unistd::getuid().as_raw();
    let accounts = users::read(Path::new("/"))?;
    let user = accounts
        .iter()
        .find(|u| name.map_or(u.uid == uid, |name| u.name == name))
        .ok_or("no matching local account")?;
    if !users::valid_existing_name(&user.name) || (uid != 0 && user.uid != uid) {
        return Err(
            "you can only change your own password; resetting another user's password needs root"
                .into(),
        );
    }
    if user.uid != 0 && !users::regular(user.uid) {
        return Err("password management here is limited to root and regular users".into());
    }
    // PAM handles secret input with echo disabled. Nothing secret goes through
    // the engine socket, environment, arguments or amctl's output handling.
    let status = Command::new("/usr/bin/passwd")
        .args(["--", &user.name])
        .env_clear()
        .env("PATH", "/usr/bin")
        .env(
            "TERM",
            std::env::var("TERM").unwrap_or_else(|_| "linux".into()),
        )
        .status()
        .map_err(|e| format!("start password prompt: {e}"))?;
    if !status.success() {
        return Err(format!(
            "password setup did not finish; retry `amctl users passwd {}`",
            user.name
        ));
    }
    if uid == 0 {
        match change(cx, "sync", &[]) {
            Ok(_) => {}
            Err(e) => eprintln!(
                "Password updated on the host; strata sync failed: {e}. Retry `amctl users sync`."
            ),
        }
    }
    Ok(())
}

pub fn run(cx: &Ctx, cmd: UserCmd) -> Result<(), String> {
    match cmd {
        UserCmd::List => {
            let report = match cx.ask("users", "list", &[])? {
                Some(v) => v,
                None => {
                    json!({"users": users::read(Path::new("/"))?.into_iter().filter(|u| u.uid == 0 || users::regular(u.uid)).collect::<Vec<_>>() })
                }
            };
            cx.emit(&report, |v| {
                cx.style.header("local users");
                for u in items(v, "users") {
                    println!(
                        "  {:<20} uid {:<6} {:<8} {}",
                        text(u, "name"),
                        u["uid"],
                        text(u, "password_status"),
                        text(u, "home")
                    );
                }
                println!("  Password locks do not terminate existing sessions.");
            });
        }
        UserCmd::Show { name } => {
            let report = match cx.ask("users", "show", &[&name])? {
                Some(v) => v,
                None => {
                    json!({"user": users::read(Path::new("/"))?.into_iter().find(|u| u.name == name).ok_or("no matching local account")?})
                }
            };
            show(cx, &report);
        }
        UserCmd::Add { name, locked } => {
            if !locked {
                interactive(cx)?;
            }
            let report = change(cx, "add", &[&name])?;
            show(cx, &report);
            if !locked {
                println!("Set a password for {name}. If cancelled, the new account stays password-locked.");
                passwd(cx, Some(&name))?;
            }
        }
        UserCmd::Passwd { name } => passwd(cx, name.as_deref())?,
        UserCmd::Lock { name } => show(cx, &change(cx, "lock", &[&name])?),
        UserCmd::Unlock { name } => show(cx, &change(cx, "unlock", &[&name])?),
        UserCmd::Sync => {
            let report = change(cx, "sync", &[])?;
            cx.emit(&report, |_| {
                println!("Host accounts shared with running Linux strata.")
            });
        }
    }
    Ok(())
}
