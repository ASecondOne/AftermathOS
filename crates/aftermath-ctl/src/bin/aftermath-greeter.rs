//! Keep the greeter compositor and its private bus inside one owned lifetime.
//! SDDM may stop the greeter while another user's compositor is already starting.
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// SIGTERM, SIGINT and SIGHUP from SDDM only set `STOP`; the main loop then
/// stops the compositor's whole process group.
fn install_stop_handler() {
    let handler = stop as extern "C" fn(libc::c_int) as libc::sighandler_t;
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: the handler only stores to a lock-free atomic and never allocates.
        unsafe {
            libc::signal(signal, handler);
        }
    }
}

fn main() -> ExitCode {
    install_stop_handler();
    let mut child = match Command::new("/usr/bin/dbus-run-session")
        .args([
            "--",
            "/usr/bin/kwin_wayland",
            "--drm",
            "--no-lockscreen",
            "--no-global-shortcuts",
        ])
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("Aftermath greeter: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut stopping = None;
    loop {
        if STOP.load(Ordering::Relaxed) && stopping.is_none() {
            // SAFETY: this is the process group created for our own child.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGTERM);
            }
            stopping = Some(Instant::now());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                return ExitCode::from(status.code().unwrap_or(1) as u8);
            }
            Err(error) => {
                eprintln!("Aftermath greeter wait: {error}");
                return ExitCode::FAILURE;
            }
            Ok(None) => {}
        }
        if stopping.is_some_and(|start| start.elapsed() > Duration::from_secs(3)) {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
