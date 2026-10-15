//! Files shared by the governor, its watchdog and the CLI, in `~/.local/state/guvnor`.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

pub fn dir() -> PathBuf {
    let d = std::env::var_os("GUVNOR_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".local/state/guvnor"));
    let _ = fs::create_dir_all(&d);
    d
}

pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn write_atomic(name: &str, data: &[u8]) -> std::io::Result<()> {
    let path = dir().join(name);
    let tmp = dir().join(format!(".{name}.{}", std::process::id()));
    let mut f = fs::File::create(&tmp)?;
    f.write_all(data)?;
    f.sync_data()?;
    fs::rename(tmp, path)
}

/// The processes the governor has paused, written before it pauses them. If the governor dies,
/// its watchdog, a restarted governor or `guvnor status`/`pause` resume them from this file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stopped {
    pub governor: i32,
    /// No process may stay paused past this time; the watchdog resumes them shortly after.
    pub deadline: f64,
    pub pids: Vec<i32>,
}

const STOPPED: &str = "stopped.json";

pub fn write_stopped(s: &Stopped) -> std::io::Result<()> {
    write_atomic(STOPPED, &serde_json::to_vec(s).unwrap())
}

pub fn read_stopped() -> Option<Stopped> {
    serde_json::from_slice(&fs::read(dir().join(STOPPED)).ok()?).ok()
}

pub fn clear_stopped() {
    let _ = fs::remove_file(dir().join(STOPPED));
}

/// Continues every process in the stopped file. Returns how many it listed.
pub fn release_stopped() -> usize {
    match read_stopped() {
        Some(s) => {
            for &pid in &s.pids {
                crate::procs::signal(pid, libc::SIGCONT);
            }
            clear_stopped();
            s.pids.len()
        }
        None => 0,
    }
}

/// What the running governor last saw, for `guvnor status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub pid: i32,
    pub updated: f64,
    pub rpm: u32,
    pub duty: f64,
    pub cap: Option<f64>,
    pub target: u32,
    pub ceiling: f64,
    pub period: f64,
    pub paused: bool,
    /// "run" or "pause" within a throttled period; "full" at 100 % duty; "off" when paused.
    pub phase: String,
    /// When the current phase ends.
    pub phase_end: f64,
    /// One sample per period, the last hour or so.
    pub history: Vec<Sample>,
    /// The latest log lines.
    pub events: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Sample {
    pub t: f64,
    pub rpm: u32,
    pub duty: f64,
}

pub fn write_status(s: &Status) {
    let _ = write_atomic("status.json", &serde_json::to_vec(s).unwrap());
}

pub fn read_status() -> Option<Status> {
    serde_json::from_slice(&fs::read(dir().join("status.json")).ok()?).ok()
}

/// Throttling is off while this file exists (`guvnor pause`); it survives restarts.
pub fn paused() -> bool {
    dir().join("paused").exists()
}

pub fn set_paused(on: bool) -> std::io::Result<()> {
    let p = dir().join("paused");
    if on {
        fs::write(p, b"").map(|_| ())
    } else {
        fs::remove_file(p).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            }
        })
    }
}

/// Holds an exclusive lock for as long as the returned file is open: one governor at a time.
pub fn lock() -> Result<fs::File, String> {
    let path = dir().join("lock");
    let mut f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let pid = fs::read_to_string(&path).unwrap_or_default();
        return Err(format!(
            "another guvnor is already running (pid {})",
            pid.trim()
        ));
    }
    f.set_len(0).ok();
    let _ = write!(f, "{}", std::process::id());
    Ok(f)
}
