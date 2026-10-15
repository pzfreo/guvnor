//! Crash safety against the real binary: whatever happens to the governor during a pause, no
//! work process stays stopped. Each test throttles only its own `sleep` process, with its own
//! state directory and config (period 2 s, duty between 0.2 and 0.3, so it pauses every period).

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_fangov");
const PERIOD: f64 = 2.0;
const FLOOR: f64 = 0.2;

struct Rig {
    dir: PathBuf,
    victim: Child,
    governors: Vec<Child>,
}

impl Rig {
    fn new(name: &str, rules: &str) -> Rig {
        let dir = std::env::temp_dir().join(format!("fangov-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A unique duration makes a unique command line.
        let n: u32 = name
            .bytes()
            .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32))
            % 100_000;
        let secs = format!("7531.{}{n:05}", std::process::id());
        let victim = Command::new("/bin/sleep").arg(&secs).spawn().unwrap();
        let pattern = format!("^/bin/sleep {}$", secs.replace('.', "\\\\."));
        std::fs::write(
            dir.join("config.toml"),
            format!("period = {PERIOD}\n[rules]\nfloor = {FLOOR}\n{rules}\n[work]\nnames = []\ncmdlines = [\"{pattern}\"]\n"),
        )
        .unwrap();
        Rig {
            dir,
            victim,
            governors: vec![],
        }
    }

    fn cmd(&self, arg: &str) -> Command {
        let mut c = Command::new(BIN);
        c.arg(arg)
            .env("FANGOV_STATE", &self.dir)
            .env("FANGOV_CONFIG", self.dir.join("config.toml"));
        c
    }

    fn start(&mut self) -> u32 {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("log"))
            .unwrap();
        let child = self
            .cmd("run")
            .stdout(log)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        self.governors.push(child);
        pid
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("log")).unwrap_or_default()
    }

    fn stopped(&self) -> bool {
        state(self.victim.id()).starts_with('T')
    }

    /// Waits until the victim's stopped state is `want`, up to `secs`.
    fn wait_for(&self, want: bool, secs: f64) -> Option<f64> {
        let t = Instant::now();
        while t.elapsed().as_secs_f64() < secs {
            if self.stopped() == want {
                return Some(t.elapsed().as_secs_f64());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    /// Asserts the victim is never stopped for `secs`.
    fn never_stopped(&self, secs: f64) {
        let t = Instant::now();
        while t.elapsed().as_secs_f64() < secs {
            assert!(!self.stopped(), "victim stopped\n{}", self.log());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        for g in &mut self.governors {
            unsafe { libc::kill(g.id() as i32, libc::SIGKILL) };
            let _ = g.wait();
        }
        unsafe { libc::kill(self.victim.id() as i32, libc::SIGCONT) };
        let _ = self.victim.kill();
        let _ = self.victim.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn state(pid: u32) -> String {
    let out = Command::new("ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn children(pid: u32) -> Vec<i32> {
    let out = Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect()
}

const THROTTLE: &str = "ceiling = 0.3";

#[test]
fn sigkill_during_a_pause_the_watchdog_continues_the_work() {
    let mut rig = Rig::new("kill", THROTTLE);
    let gov = rig.start();
    rig.wait_for(true, 6.0)
        .expect("governor never paused the victim");
    unsafe { libc::kill(gov as i32, libc::SIGKILL) };
    let t = rig
        .wait_for(false, 2.0)
        .expect("victim left stopped after the governor was killed");
    assert!(t < 1.0, "took {t:.2} s");
    rig.never_stopped(3.0);
    assert!(!rig.dir.join("stopped.json").exists());
    assert!(rig.log().contains("watchdog: governor"), "{}", rig.log());
}

#[test]
fn sigkill_of_governor_and_watchdog_is_recovered_by_status_or_restart() {
    let mut rig = Rig::new("killboth", THROTTLE);
    let gov = rig.start();
    rig.wait_for(true, 6.0)
        .expect("governor never paused the victim");
    let dogs = children(gov);
    assert_eq!(dogs.len(), 1, "expected one watchdog");
    unsafe {
        libc::kill(dogs[0], libc::SIGKILL);
        libc::kill(gov as i32, libc::SIGKILL);
    }
    std::thread::sleep(Duration::from_millis(500));
    assert!(rig.stopped(), "with both killed nothing has resumed it yet");
    // `fangov status` resumes what a dead governor left paused.
    let out = rig.cmd("status").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("continued 1 processes"));
    assert!(!rig.stopped());

    // And so does a restart (the LaunchAgent's KeepAlive). Kill both again, then start a
    // governor with throttling paused, so whatever it does at start is all that runs.
    let gov = rig.start();
    rig.wait_for(true, 6.0)
        .expect("second governor never paused the victim");
    let dogs = children(gov);
    unsafe {
        libc::kill(dogs[0], libc::SIGKILL);
        libc::kill(gov as i32, libc::SIGKILL);
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(rig.stopped());
    std::fs::write(rig.dir.join("paused"), b"").unwrap();
    rig.start();
    rig.wait_for(false, 2.0)
        .expect("restart did not continue the victim");
    rig.never_stopped(3.0);
}

#[test]
fn a_hung_governor_cannot_hold_a_pause_past_its_bound() {
    let mut rig = Rig::new("hang", THROTTLE);
    let gov = rig.start();
    rig.wait_for(true, 6.0)
        .expect("governor never paused the victim");
    let began = Instant::now();
    unsafe { libc::kill(gov as i32, libc::SIGSTOP) };
    rig.wait_for(false, 4.0)
        .expect("victim left stopped while the governor hung");
    let held = began.elapsed().as_secs_f64();
    let bound = PERIOD * (1.0 - FLOOR) + 0.5 + 0.25 + 0.1; // + watchdog grace, poll, slack
    assert!(held <= bound, "paused {held:.2} s, bound {bound:.2} s");
    unsafe { libc::kill(gov as i32, libc::SIGCONT) };
}

#[test]
fn sigterm_continues_all_work_and_exits() {
    let mut rig = Rig::new("term", THROTTLE);
    let gov = rig.start();
    rig.wait_for(true, 6.0)
        .expect("governor never paused the victim");
    unsafe { libc::kill(gov as i32, libc::SIGTERM) };
    let status = rig.governors[0].wait().unwrap();
    assert!(status.success());
    assert!(!rig.stopped());
    assert!(rig.log().contains("exit: continued"));
    rig.never_stopped(1.0);
}

#[test]
fn every_pause_is_bounded() {
    // Floor = ceiling = 0.2: the longest pause allowed, every period.
    let mut rig = Rig::new("bound", "ceiling = 0.2");
    rig.start();
    rig.wait_for(true, 6.0)
        .expect("governor never paused the victim");
    let (mut longest, mut since, mut pauses) = (0.0f64, Some(Instant::now()), 1);
    let t = Instant::now();
    while t.elapsed().as_secs_f64() < 7.0 {
        match (rig.stopped(), since) {
            (true, None) => {
                since = Some(Instant::now());
                pauses += 1;
            }
            (false, Some(s)) => {
                longest = longest.max(s.elapsed().as_secs_f64());
                since = None;
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(pauses >= 3, "only {pauses} pauses");
    assert!(
        longest <= PERIOD * (1.0 - FLOOR) + 0.15,
        "longest pause {longest:.2} s"
    );
}

#[test]
fn pause_continues_everything_and_resume_throttles_again() {
    let mut rig = Rig::new("pause", THROTTLE);
    rig.start();
    rig.wait_for(true, 6.0)
        .expect("governor never paused the victim");
    assert!(rig.cmd("pause").status().unwrap().success());
    assert!(!rig.stopped());
    rig.never_stopped(2.0 * PERIOD);
    let out = rig.cmd("status").output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("paused"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(rig.cmd("resume").status().unwrap().success());
    rig.wait_for(true, 2.0 * PERIOD + 1.0)
        .expect("resume did not throttle again");
}

#[test]
fn at_full_duty_nothing_is_stopped() {
    let mut rig = Rig::new("full", "floor = 1.0\nceiling = 1.0");
    // The config above sets floor twice; use a clean one.
    let cfg = rig.dir.join("config.toml");
    let text = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("floor = 0.2\n", "");
    std::fs::write(&cfg, text).unwrap();
    rig.start();
    rig.never_stopped(3.0 * PERIOD);
    assert!(!rig.dir.join("stopped.json").exists());
    assert!(rig.log().contains("duty 100%"), "{}", rig.log());
}

#[test]
fn only_one_governor_runs() {
    let mut rig = Rig::new("lock", THROTTLE);
    rig.start();
    let t = Instant::now();
    while !rig.dir.join("status.json").exists() {
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "first governor never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut second = rig
        .cmd("run")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let t = Instant::now();
    while second.try_wait().unwrap().is_none() {
        if t.elapsed() > Duration::from_secs(3) {
            let _ = second.kill();
            panic!("a second governor kept running");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = second.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already running"));
}
