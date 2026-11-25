use crate::cli::SessionCmd;
use crate::output::{items, need_engine, text, Ctx};

pub fn run(cx: &Ctx, _cmd: SessionCmd) -> Result<(), String> {
    let report = cx
        .ask("sessions", "list", &[])?
        .ok_or_else(|| need_engine("sessions list"))?;
    cx.emit(&report, |v| {
        cx.style.header("authenticated sessions");
        let mut count = 0;
        for s in items(v, "sessions") {
            println!("  {}  {} (uid {})  {}  leader {}  {}", text(s, "id"), text(s, "user"), s["uid"], text(s, "tty"), s["leader"], text(s, "runtime_dir"));
            count += 1;
        }
        if count == 0 { println!("  No authenticated sessions. Login is available on configured login terminals (tty2 by default)."); }
        for error in items(v, "cleanup_errors") { println!("  cleanup: {error}"); }
        println!("  Graphical sessions use elogind seat/device access and a session-scoped D-Bus.");
    });
    Ok(())
}
