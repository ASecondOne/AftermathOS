//! `amctl logs`: this boot's session logs (kernel, consoles, services),
//! read straight from the files, so it works without the engine.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use aftermath_ctl::facts;

use crate::cli::LogsArgs;
use crate::output::Ctx;

pub fn run(cx: &Ctx, args: LogsArgs) -> Result<(), String> {
    let dir = log_dir()?;

    if args.list {
        return list(cx, &dir);
    }

    let (file, filter) = pick_file(&dir, &args)?;
    let mut f = File::open(&file).map_err(|e| format!("{}: {e}", file.display()))?;

    print_tail(&mut f, filter, args.lines)?;

    if args.follow {
        follow(&mut f, &file, filter);
    }
    Ok(())
}

/// This boot's log directory: from the engine's note, else the `latest` link.
fn log_dir() -> Result<PathBuf, String> {
    if let Some(dir) = facts::log_dir() {
        return Ok(PathBuf::from(dir));
    }

    for latest in [
        "/aftermath/store/logs/latest",
        "/var/log/aftermath/latest",
        "/run/aftermath/logs/latest",
    ] {
        if Path::new(latest).is_dir() {
            return fs::canonicalize(latest).map_err(|e| e.to_string());
        }
    }
    Err("no session logs found for this boot".into())
}

/// Every file of this boot (one level of subdirectories, e.g. services/).
fn list(cx: &Ctx, dir: &Path) -> Result<(), String> {
    cx.style.header(&format!("logs in {}", dir.display()));

    let mut files = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let inner = fs::read_dir(&path).into_iter().flatten().flatten();
            files.extend(inner.map(|e| e.path()));
        } else {
            files.push(path);
        }
    }
    files.sort();

    for f in files {
        let size = fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
        let name = f.strip_prefix(dir).unwrap_or(&f).display().to_string();
        println!("  {name:<28} {}", cx.style.dim(&format!("{size} bytes")));
    }
    Ok(())
}

/// Which file to show, and an optional substring every line must contain.
/// Default: only Aftermath's own lines from the kernel log.
fn pick_file(dir: &Path, args: &LogsArgs) -> Result<(PathBuf, Option<&'static str>), String> {
    if let Some(topic) = &args.topic {
        let path = if topic == "desktop" {
            dir.join("desktop.log")
        } else {
            dir.join("services").join(format!("{topic}.log"))
        };
        return Ok((path, None));
    }
    if let Some(service) = &args.service {
        if !aftermath_ctl::services::valid_name(service) {
            return Err(format!("invalid service name `{service}`"));
        }
        let file = dir.join("services").join(format!("{service}.log"));
        return Ok((file, None));
    }

    if let Some(console) = &args.console {
        // Plain names only (tty1, ttyS0): no paths.
        if !console.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(format!("invalid console `{console}`"));
        }
        return Ok((dir.join(format!("{console}.log")), None));
    }

    let kmsg = dir.join("kmsg.log");
    if args.kernel {
        Ok((kmsg, None))
    } else {
        Ok((kmsg, Some("aftermath")))
    }
}

fn wanted(line: &str, filter: Option<&str>) -> bool {
    filter.is_none_or(|word| line.contains(word))
}

/// The last `count` matching lines.
fn print_tail(f: &mut File, filter: Option<&str>, count: usize) -> Result<(), String> {
    let mut text = String::new();
    f.read_to_string(&mut text).map_err(|e| e.to_string())?;

    let lines: Vec<&str> = text.lines().filter(|l| wanted(l, filter)).collect();
    let start = lines.len().saturating_sub(count);

    let mut out = std::io::stdout().lock();
    for line in &lines[start..] {
        let _ = writeln!(out, "{line}");
    }
    let _ = out.flush();
    Ok(())
}

/// `tail -f`: poll the file for growth and print new complete lines.
/// Runs until interrupted (Ctrl-C).
fn follow(f: &mut File, path: &Path, filter: Option<&str>) -> ! {
    let mut pos = f.stream_position().unwrap_or(0);
    let mut partial = String::new();

    loop {
        std::thread::sleep(Duration::from_millis(300));

        let len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if len < pos {
            pos = 0; // the file was truncated or replaced: start over
        }
        if len == pos {
            continue;
        }

        let mut chunk = Vec::new();
        let _ = f.seek(SeekFrom::Start(pos));
        let _ = f.take(len - pos).read_to_end(&mut chunk);
        pos = len;

        partial.push_str(&String::from_utf8_lossy(&chunk));
        print_complete_lines(&mut partial, filter);
    }
}

/// Print and remove every finished line from `buf`; keep the unfinished rest.
fn print_complete_lines(buf: &mut String, filter: Option<&str>) {
    while let Some(end) = buf.find('\n') {
        let line: String = buf.drain(..=end).collect();
        if wanted(&line, filter) {
            print!("{line}");
        }
    }
    let _ = std::io::stdout().flush();
}
