//! A tiny separate process that continues paused work if the governor dies or stalls.
//!
//! The governor starts it in its own session, so a Ctrl-C or a signal to the terminal's process
//! group does not reach it, and writes a heartbeat byte to its stdin at least every 100 ms. When
//! the pipe closes (the governor exited or was killed) the watchdog continues everything the
//! governor listed as possibly paused, and exits. When the heartbeat stops for `GRACE` (the
//! governor is hung or stopped), it continues them too and keeps watching.

use crate::{log, procs, state};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long the heartbeat may stop before the watchdog continues the work itself.
pub const GRACE: f64 = 1.0;

/// The governor's end: the watchdog process and the heartbeat pipe to it.
pub struct Dog {
    child: Child,
    beat: ChildStdin,
}

impl Dog {
    pub fn spawn() -> std::io::Result<Dog> {
        let exe = std::env::current_exe()?;
        let me = std::process::id().to_string();
        let mut cmd = Command::new(exe);
        cmd.args(["watchdog", &me]).stdin(Stdio::piped());
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let beat = child.stdin.take().expect("piped stdin");
        // Never block the governor on a full pipe, whatever the watchdog is doing.
        unsafe {
            let fd = beat.as_raw_fd();
            libc::fcntl(
                fd,
                libc::F_SETFL,
                libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK,
            );
        }
        Ok(Dog { child, beat })
    }

    pub fn beat(&mut self) {
        let _ = self.beat.write(&[0]);
    }

    pub fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    pub fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

static TERM: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: i32) {
    TERM.store(true, Ordering::SeqCst);
}

/// Continues everything this governor listed. Returns how many it listed.
fn release(governor: i32, clear: bool) -> usize {
    match state::read_stopped().filter(|s| s.governor == governor) {
        Some(s) => {
            for &pid in &s.pids {
                procs::signal(pid, libc::SIGCONT);
            }
            if clear {
                state::clear_stopped();
            }
            s.pids.len()
        }
        None => 0,
    }
}

pub fn run(governor: i32) -> ! {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
    }
    let mut last = Instant::now();
    let mut stalled = false;
    let mut buf = [0u8; 4096];
    loop {
        let mut fds = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut fds, 1, 100) };
        let gone = if ready > 0 {
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                last = Instant::now();
                stalled = false;
            }
            n == 0 || (n < 0 && fds.revents & libc::POLLHUP != 0)
        } else {
            false
        };
        if gone || TERM.load(Ordering::SeqCst) || unsafe { libc::getppid() } != governor {
            let n = release(governor, true);
            if n > 0 {
                log(&format!(
                    "watchdog: governor {governor} gone, continued {n} processes"
                ));
            }
            std::process::exit(0);
        }
        if !stalled && last.elapsed() > Duration::from_secs_f64(GRACE) {
            stalled = true;
            let n = release(governor, false);
            if n > 0 {
                log(&format!(
                    "watchdog: governor {governor} stalled, continued {n} processes"
                ));
            }
        }
    }
}
