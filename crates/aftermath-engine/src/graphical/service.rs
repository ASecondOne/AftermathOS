//! Bounded foreground process supervision shared by login1 and the greeter.
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct Service {
    pub pid: Option<i32>,
    pub restarts: u32,
    starts: VecDeque<Instant>,
    stopping: Option<Instant>,
    started_once: bool,
}

impl Service {
    /// Three starts per thirty seconds, with each child in its own process group.
    pub fn spawn(&mut self, program: &str, log: &str) -> Result<(), String> {
        let now = Instant::now();
        self.starts
            .retain(|at| now.duration_since(*at) < Duration::from_secs(30));
        if self.starts.len() >= 3 {
            return Err(format!("{program}: restart limit reached"));
        }
        let output = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| e.to_string())?;
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/bin")
            .env("LANG", "C.UTF-8")
            .env("SYSTEMD_LOG_TARGET", "console")
            .env("ELOGIND_LOG_TARGET", "console")
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(output));
        // SAFETY: setsid is async-signal-safe between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(|e| format!("{program}: {e}"))?;
        self.restarts += u32::from(self.started_once);
        self.started_once = true;
        self.starts.push_back(now);
        self.pid = Some(child.id() as i32);
        self.stopping = None;
        Ok(())
    }

    /// Give normal cleanup three seconds, then kill the owned process group.
    pub fn stop(&mut self) {
        let Some(pid) = self.pid else { return };
        if self.stopping.is_none() {
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
            self.stopping = Some(Instant::now());
        }
        if self
            .stopping
            .is_some_and(|start| start.elapsed() > Duration::from_secs(3))
        {
            let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }

    /// Only the engine's central waitpid loop reaps supervised processes.
    pub fn exited(&mut self, pid: i32) -> bool {
        if self.pid != Some(pid) {
            return false;
        }
        self.pid = None;
        self.stopping = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_owned_child_crashes_reach_the_start_limit() {
        let root =
            std::env::temp_dir().join(format!("aftermath-greeter-limit-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let daemon = root.join("daemon");
        crate::test_support::install(&daemon);
        let mut service = Service::default();
        for _ in 0..3 {
            service
                .spawn(daemon.to_str().unwrap(), root.join("log").to_str().unwrap())
                .unwrap();
            let pid = service.pid.unwrap();
            kill(Pid::from_raw(pid), Signal::SIGKILL).unwrap();
            nix::sys::wait::waitpid(Pid::from_raw(pid), None).unwrap();
            assert!(service.exited(pid));
        }
        assert!(service
            .spawn(daemon.to_str().unwrap(), root.join("log").to_str().unwrap())
            .unwrap_err()
            .contains("restart limit"));
        assert_eq!(service.restarts, 2);
        std::fs::remove_dir_all(root).unwrap();
    }
}
