//! Mirror early desktop logs into this boot's persistent log directory.
//! Providers retain their original stdout descriptors, including across startup.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SOURCES: &[(&str, &str)] = &[
    ("udev", "udev.log"),
    ("dbus", "dbus.log"),
    ("elogind", "elogind.log"),
    ("sddm", "sddm.log"),
    ("engine", "desktop-events.log"),
    ("syslog", "syslog.log"),
];

/// Record a desktop lifecycle event without logging any authentication input.
pub fn desktop_event(message: &str) {
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/run/aftermath/desktop-events.log")
    {
        let _ = writeln!(file, "{message}");
    }
}

/// Capture startup output already written, then follow appends and restarts.
pub fn start_desktop_tap(directory: &Path) {
    let directory = directory.to_owned();
    let result = std::thread::Builder::new()
        .name("desktop-logs".into())
        .spawn(move || {
            let Ok(mut logs) = DesktopLogs::new(Path::new("/run/aftermath"), &directory) else {
                return;
            };
            loop {
                let _ = logs.poll();
                std::thread::sleep(Duration::from_millis(100));
            }
        });
    if let Err(error) = result {
        crate::warn(&format!("desktop log capture: {error}"));
    }
}

/// Track each input independently; source files are opened only for reading.
struct Source {
    name: &'static str,
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    output: File,
}

struct DesktopLogs {
    sources: Vec<Source>,
    combined: File,
}
impl DesktopLogs {
    fn new(runtime: &Path, directory: &Path) -> io::Result<Self> {
        fs::create_dir_all(directory.join("services"))?;
        let mut sources = Vec::new();
        for &(name, filename) in SOURCES {
            sources.push(Source {
                name,
                path: runtime.join(filename),
                offset: 0,
                partial: Vec::new(),
                output: append(&directory.join("services").join(format!("{name}.log")))?,
            });
        }
        Ok(Self {
            sources,
            combined: append(&directory.join("desktop.log"))?,
        })
    }

    /// Bound each read so a noisy daemon cannot hold the engine or exhaust RAM.
    fn poll(&mut self) -> io::Result<()> {
        for source in &mut self.sources {
            let Ok(mut input) = File::open(&source.path) else {
                continue;
            };
            if input.metadata()?.len() < source.offset {
                source.offset = 0;
                source.partial.clear();
            }
            input.seek(SeekFrom::Start(source.offset))?;
            let mut chunk = Vec::new();
            input.take(64 * 1024).read_to_end(&mut chunk)?;
            source.output.write_all(&chunk)?;
            source.offset += chunk.len() as u64;
            source.partial.extend(chunk);
            while let Some(end) = source.partial.iter().position(|byte| *byte == b'\n') {
                write!(self.combined, "[{}] ", source.name)?;
                self.combined
                    .write_all(&source.partial.drain(..=end).collect::<Vec<_>>())?;
            }
            // Keep malformed, newline-free logging bounded too.
            if source.partial.len() > 64 * 1024 {
                self.combined.write_all(&source.partial)?;
                self.combined.write_all(b"\n")?;
                source.partial.clear();
            }
        }
        Ok(())
    }
}

fn append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_early_output_and_appends_without_replaying_old_lines() {
        let root =
            std::env::temp_dir().join(format!("aftermath-desktop-logs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("runtime")).unwrap();
        fs::write(root.join("runtime/udev.log"), "early discovery\n").unwrap();
        let mut logs = DesktopLogs::new(&root.join("runtime"), &root.join("saved")).unwrap();
        logs.poll().unwrap();
        append(&root.join("runtime/udev.log"))
            .unwrap()
            .write_all(b"later\n")
            .unwrap();
        fs::write(root.join("runtime/sddm.log"), "greeter ready\n").unwrap();
        logs.poll().unwrap();
        logs.poll().unwrap();
        assert_eq!(
            fs::read_to_string(root.join("saved/desktop.log")).unwrap(),
            "[udev] early discovery\n[udev] later\n[sddm] greeter ready\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("saved/services/udev.log")).unwrap(),
            "early discovery\nlater\n"
        );
        drop(logs);
        fs::remove_dir_all(root).unwrap();
    }
}
