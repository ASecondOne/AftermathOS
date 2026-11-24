//! Strata start-up: runs `aftermath-strata setup` (crates/aftermath-strata)
//! and waits for it. It mounts the strata disk, shares /proc /sys /dev /run
//! /tmp /home /root and the /etc pieces into every stratum and generates the
//! cross launchers in /aftermath/cross. Any error here is reported to main,
//! which continues in plain Arch host mode (plan B).

use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

const SERVICE: &str = "/usr/lib/aftermath/aftermath-strata";
const TIMEOUT: Duration = Duration::from_secs(120);

pub fn start(simulate_failure: bool) -> Result<(), String> {
    if simulate_failure {
        return Err("TEST: simulated strata failure".into());
    }
    if !Path::new(SERVICE).exists() {
        return Err(format!("{SERVICE} is not installed"));
    }
    let mut child = Command::new(SERVICE)
        .arg("setup")
        .env("AFTERMATH_KMSG", "1")
        .spawn()
        .map_err(|e| format!("{SERVICE}: {e}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(st)) => return Err(format!("aftermath-strata setup {st}")),
            Ok(None) if started.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "aftermath-strata setup hung for {}s",
                    TIMEOUT.as_secs()
                ));
            }
            Ok(None) => sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("waiting for aftermath-strata: {e}")),
        }
    }
}
