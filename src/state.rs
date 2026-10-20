//! Files shared by the governor, its watchdog and the CLI, in `~/.local/state/guvnor`.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
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
    // No fsync: the files only have to outlive the governor, not the machine, and the
    // temperature loop writes them every second.
    fs::rename(tmp, path)
}

/// The processes the governor may have paused this cycle, written before it pauses any. If the
/// governor dies or stalls, its watchdog, a restarted governor or `guvnor status`/`pause`
/// continue them from this file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stopped {
    pub governor: i32,
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
    /// "run" or "pause" within a synchronised cycle, "stagger" with staggered pauses, "full" at
    /// 100 % duty, "off" when paused.
    pub phase: String,
    /// When the current phase ends.
    pub phase_end: f64,
    /// One sample per period, the last hour or so.
    pub history: Vec<Sample>,
    /// The latest log lines.
    pub events: Vec<String>,
    /// "temperature" or "fans".
    #[serde(default)]
    pub control: String,
    /// The hottest die sensor and the setpoint the temperature loop holds it at.
    #[serde(default)]
    pub temp: Option<f64>,
    #[serde(default)]
    pub setpoint: Option<f64>,
    /// The mean of the die sensors, latest reading.
    #[serde(default)]
    pub avg: Option<f64>,
    /// Seconds per duty cycle (the temperature loop's, or the period).
    #[serde(default)]
    pub cycle: f64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Sample {
    pub t: f64,
    pub rpm: u32,
    pub duty: f64,
    #[serde(default)]
    pub temp: Option<f64>,
    #[serde(default)]
    pub setpoint: Option<f64>,
    /// The mean of the die sensors, averaged over the period.
    #[serde(default)]
    pub avg: Option<f64>,
    /// The 1-minute load average.
    #[serde(default)]
    pub load: Option<f64>,
    /// CPU power (`PSTR`, watts) at the end of the period, where the SMC has it.
    #[serde(default)]
    pub heat: Option<f64>,
}

/// The 1-minute load average.
pub fn load() -> Option<f64> {
    let mut l = [0f64; 3];
    (unsafe { libc::getloadavg(l.as_mut_ptr(), 3) } > 0).then_some(l[0])
}

pub fn write_status(s: &Status) {
    let _ = write_atomic("status.json", &serde_json::to_vec(s).unwrap());
}

pub fn read_status() -> Option<Status> {
    serde_json::from_slice(&fs::read(dir().join("status.json")).ok()?).ok()
}

/// Choices made in the dashboard: (pid, start time) of processes to control or to leave alone,
/// whatever the config's rules say, and of owners whose work to control. They lapse when the
/// process exits.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pins {
    pub on: Vec<(i32, u64)>,
    pub off: Vec<(i32, u64)>,
    /// Owners whose work is controlled (see `procs::owners`).
    pub owners: Vec<(i32, u64)>,
}

/// A long record of the periods, for working out how this machine's fans behave: one line per
/// period in trace.csv. Past 8 MB (about two weeks), or written with other columns, it becomes
/// trace.1.csv, replacing the one before.
pub fn append_trace(s: &Sample, target: u32, throttling: bool) {
    const HEADER: &str = "time,rpm,target,heat,hottest,average,setpoint,duty,load,throttling\n";
    let path = dir().join("trace.csv");
    let other_columns = || {
        let mut head = [0u8; HEADER.len()];
        fs::File::open(&path)
            .is_ok_and(|mut f| f.read_exact(&mut head).is_err() || head != HEADER.as_bytes())
    };
    if fs::metadata(&path).is_ok_and(|m| m.len() > 8 << 20) || other_columns() {
        let _ = fs::rename(&path, dir().join("trace.1.csv"));
    }
    let new = !path.exists();
    let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let o = |v: Option<f64>| v.map(|v| format!("{v:.2}")).unwrap_or_default();
    let line = format!(
        "{}{:.1},{},{},{},{},{},{},{:.3},{},{}\n",
        if new { HEADER } else { "" },
        s.t,
        s.rpm,
        target,
        o(s.heat),
        o(s.temp),
        o(s.avg),
        o(s.setpoint),
        s.duty,
        o(s.load),
        throttling as u8
    );
    let _ = f.write_all(line.as_bytes());
}

pub fn read_pins() -> Pins {
    fs::read(dir().join("pins.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Saves the choices, dropping those for processes that have exited.
pub fn write_pins(pins: &Pins) -> std::io::Result<()> {
    let live =
        |&(pid, start): &(i32, u64)| crate::procs::info(pid).is_some_and(|p| p.start == start);
    let pins = Pins {
        on: pins.on.iter().copied().filter(live).collect(),
        off: pins.off.iter().copied().filter(live).collect(),
        owners: pins.owners.iter().copied().filter(live).collect(),
    };
    write_atomic("pins.json", &serde_json::to_vec(&pins).unwrap())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trace_with_other_columns_is_moved_aside() {
        let _env = crate::tui::tests::ENV.lock().unwrap();
        let d = std::env::temp_dir().join(format!("guvnor-trace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        unsafe {
            std::env::set_var("GUVNOR_STATE", &d);
        }
        let old = "time,rpm,target,heat,predicted,hottest\n1,2500,2500,,,\n";
        fs::write(d.join("trace.csv"), old).unwrap();
        let s = Sample {
            t: 5.0,
            rpm: 2600,
            ..Sample::default()
        };
        append_trace(&s, 2500, true);
        append_trace(&s, 2500, true);
        assert_eq!(fs::read_to_string(d.join("trace.1.csv")).unwrap(), old);
        let new = fs::read_to_string(d.join("trace.csv")).unwrap();
        assert_eq!(new.lines().count(), 3);
        assert!(new.starts_with("time,rpm,target,heat,hottest,"));
        assert!(new.lines().all(|l| l.split(',').count() == 10));
        fs::remove_dir_all(&d).unwrap();
    }
}
