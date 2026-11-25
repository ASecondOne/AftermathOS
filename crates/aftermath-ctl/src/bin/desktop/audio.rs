//! Per-session audio processes. The authenticated user owns every child and log.
use std::fs::{self, OpenOptions};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Audio {
    services: Vec<Service>,
}
struct Service {
    program: &'static str,
    child: Option<Child>,
    log: PathBuf,
    attempts: u8,
    retry: Instant,
}

impl Audio {
    /// Start PipeWire before its policy manager and PulseAudio compatibility.
    pub fn start() -> std::io::Result<Self> {
        let runtime = PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR")
                .ok_or_else(|| std::io::Error::other("missing XDG_RUNTIME_DIR"))?,
        );
        let state = std::env::var_os("XDG_STATE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
            })
            .join("aftermath/audio");
        fs::create_dir_all(&state)?;
        let mut services = Vec::new();
        for name in ["pipewire", "wireplumber", "pipewire-pulse"] {
            let mut service = Service {
                program: name,
                child: None,
                log: state.join(format!("{name}.log")),
                attempts: 0,
                retry: Instant::now(),
            };
            service.advance();
            services.push(service);
            if name == "pipewire" {
                wait_for_socket(&runtime.join("pipewire-0"));
            }
        }
        Ok(Self { services })
    }

    /// Retry a crashed audio process at most three times during this login.
    pub fn tick(&mut self) {
        for service in &mut self.services {
            service.advance();
        }
    }
}

impl Service {
    fn advance(&mut self) {
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(None) => return,
                Ok(Some(_)) => self.child = None,
                Err(_) => return,
            }
            self.retry = Instant::now() + Duration::from_secs(1);
        }
        if self.attempts >= 3 || Instant::now() < self.retry {
            return;
        }
        self.attempts += 1;
        self.retry = Instant::now() + Duration::from_secs(1);
        let result = self.spawn();
        match result {
            Ok(child) => self.child = Some(child),
            Err(error) => eprintln!("Aftermath audio: {}: {error}", self.program),
        }
    }

    /// Each child has a separate process group so session cleanup is bounded.
    fn spawn(&self) -> std::io::Result<Child> {
        let output = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)?;
        Command::new(format!("/usr/bin/{}", self.program))
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output))
            .spawn()
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        for service in &self.services {
            if let Some(child) = &service.child {
                // SAFETY: the negative PID is the process group created above.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGTERM);
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.services.iter_mut().all(|service| {
                service
                    .child
                    .as_mut()
                    .is_none_or(|child| child.try_wait().is_ok_and(|status| status.is_some()))
            }) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        for service in &mut self.services {
            if let Some(child) = &mut service.child {
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.wait();
            }
        }
    }
}

/// Readiness is the real runtime socket; a failed audio start cannot hold KDE.
fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}
