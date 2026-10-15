//! A tiny separate process that resumes paused work if the governor dies or stalls.
//!
//! The governor starts it in its own session, so a Ctrl-C or a signal to the terminal's process
//! group does not reach it. Every quarter second it checks that the governor is still its parent
//! and that no pause has outlived its deadline.

use crate::{log, procs, state};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

/// How long past a pause's deadline the watchdog waits for the governor before acting itself.
pub const GRACE: f64 = 0.5;
const POLL: std::time::Duration = std::time::Duration::from_millis(250);

pub fn spawn() -> std::io::Result<Child> {
    let exe = std::env::current_exe()?;
    let me = std::process::id().to_string();
    let mut cmd = Command::new(exe);
    cmd.args(["watchdog", &me]).stdin(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
}

static TERM: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: i32) {
    TERM.store(true, Ordering::SeqCst);
}

pub fn run(governor: i32) -> ! {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
    }
    let mut handled = f64::NAN;
    loop {
        let orphaned = unsafe { libc::getppid() } != governor;
        let stopped = state::read_stopped().filter(|s| s.governor == governor);
        if orphaned || TERM.load(Ordering::SeqCst) {
            if let Some(s) = stopped {
                for &pid in &s.pids {
                    procs::signal(pid, libc::SIGCONT);
                }
                state::clear_stopped();
                log(&format!(
                    "watchdog: governor {governor} gone, continued {} paused processes",
                    s.pids.len()
                ));
            }
            std::process::exit(0);
        }
        if let Some(s) = stopped
            && state::now() > s.deadline + GRACE
            && s.deadline != handled
        {
            for &pid in &s.pids {
                procs::signal(pid, libc::SIGCONT);
            }
            handled = s.deadline;
            log(&format!(
                "watchdog: pause overran its deadline, continued {} processes",
                s.pids.len()
            ));
        }
        std::thread::sleep(POLL);
    }
}
