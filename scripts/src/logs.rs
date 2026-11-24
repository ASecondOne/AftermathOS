//! `cargo am logs`: read the session logs the VM wrote to the strata disk
//! (out/strata.img: /logs/<boot time>/{tty1,ttyS0,kmsg}.log), without root
//! and without mounting: `debugfs` (e2fsprogs) reads the ext4 image directly.
//! Works while the VM is running (the newest few seconds may be missing).

use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Args;

use crate::ctx::{which, Ctx};

#[derive(Args)]
pub struct LogsArgs {
    /// List the boots that have logs.
    #[arg(long, short)]
    pub list: bool,
    /// Which boot: 0 = newest, 1 = the one before, ... or a boot name.
    #[arg(long, short, default_value = "0")]
    pub boot: String,
    /// Only this log: tty1, ttyS0, kmsg, ...
    #[arg(long, short)]
    pub file: Option<String>,
    /// Read another strata disk image instead of out/strata.img.
    #[arg(long, value_name = "PATH")]
    pub image: Option<std::path::PathBuf>,
    /// Keep terminal escape sequences (colours, cursor movement).
    #[arg(long)]
    pub raw: bool,
}

pub fn run(ctx: &Ctx, a: &LogsArgs) -> Result<()> {
    let img = a.image.clone().unwrap_or_else(|| ctx.strata_image());
    if !img.exists() {
        bail!(
            "no strata disk ({}): without it the VM keeps its logs in RAM only.\n\
             The serial console is always in out/vm/serial.log.",
            img.display()
        );
    }
    if which("debugfs").is_none() {
        bail!("debugfs not found: sudo pacman -S --needed e2fsprogs");
    }
    let debugfs = |req: &str| -> Result<String> {
        let out = Command::new("debugfs")
            .arg("-R")
            .arg(req)
            .arg(&img)
            .output()
            .context("run debugfs")?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };

    let mut boots = names(&debugfs("ls -p /logs")?);
    boots.retain(|n| n != "latest");
    boots.sort();
    if boots.is_empty() {
        bail!(
            "no session logs on {} yet (boot Aftermath with the strata disk first)",
            img.display()
        );
    }
    if a.list {
        for (i, b) in boots.iter().rev().enumerate() {
            let files = names(&debugfs(&format!("ls -p /logs/{b}"))?).join(" ");
            println!("{i:>3}  {b}  {files}");
        }
        return Ok(());
    }
    let boot = match a.boot.parse::<usize>() {
        Ok(i) => boots.iter().rev().nth(i).cloned(),
        Err(_) => boots.iter().find(|b| **b == a.boot).cloned(),
    }
    .with_context(|| format!("no boot `{}` (see `cargo am logs --list`)", a.boot))?;

    let mut files = names(&debugfs(&format!("ls -p /logs/{boot}"))?);
    files.sort_by_key(|f| (f != "kmsg.log", f.clone()));
    if let Some(want) = &a.file {
        let want = if want.ends_with(".log") {
            want.clone()
        } else {
            format!("{want}.log")
        };
        files.retain(|f| *f == want);
        if files.is_empty() {
            bail!("no {want} in boot {boot}");
        }
    }
    for f in files {
        println!("\x1b[1;35m===== {boot}/{f} =====\x1b[0m");
        let text = debugfs(&format!("cat /logs/{boot}/{f}"))?;
        print!("{}", if a.raw { text } else { strip_escapes(&text) });
        println!();
    }
    Ok(())
}

/// File names from `debugfs ls -p` (`/inode/mode/uid/gid/name/size/`).
fn names(ls: &str) -> Vec<String> {
    ls.lines()
        .filter_map(|l| l.split('/').nth(5))
        .filter(|n| !n.is_empty() && *n != "." && *n != "..")
        .map(str::to_owned)
        .collect()
}

/// Drop CSI (`ESC [ ... letter`), OSC (`ESC ] ... BEL|ESC \`) sequences and
/// carriage returns, so the logs read like plain text.
pub fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.next() {
                Some('[') => {
                    for c in it.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = it.next() {
                        if c == '\x07' || (c == '\x1b' && it.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_debugfs_ls() {
        let ls = "/2/040755/0/0/./\n/2/040755/0/0/../\n/12/040755/0/0/2026-11-24_20-36-05//\n/13/120777/0/0/latest/19/\n";
        assert_eq!(names(ls), vec!["2026-11-24_20-36-05", "latest"]);
    }

    #[test]
    fn strips_escapes() {
        let s = "\x1b[1;31mred\x1b[0m\r\n\x1b]3008;start=x;user=root\x1b\\# ls\x07\n";
        assert_eq!(strip_escapes(s), "red\n# ls\x07\n");
        assert_eq!(strip_escapes("a\x1b]0;title\x07b"), "ab");
    }
}
