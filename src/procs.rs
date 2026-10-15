//! Finding work processes (like pgrep, through libproc and sysctl) and signalling them.

use crate::config::Work;
use regex::Regex;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    pub name: String,
    pub stopped: bool,
    zombie: bool,
}

fn c_str(bytes: &[libc::c_char]) -> String {
    let b: Vec<u8> = bytes
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&b).into_owned()
}

/// One process's BSD info, or None if it has gone or is not visible.
pub fn info(pid: i32) -> Option<Proc> {
    let mut bi: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_bsdinfo>() as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut bi as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    // The registered name if there is one (it can be longer), else the 16-byte command name.
    let name = match c_str(&bi.pbi_name) {
        n if n.is_empty() => c_str(&bi.pbi_comm),
        n => n,
    };
    Some(Proc {
        pid,
        ppid: bi.pbi_ppid as i32,
        uid: bi.pbi_uid,
        name,
        stopped: bi.pbi_status == libc::SSTOP,
        zombie: bi.pbi_status == 5, // SZOMB
    })
}

pub fn all() -> Vec<Proc> {
    let mut pids = vec![0i32; 8192];
    loop {
        let bytes = (pids.len() * size_of::<i32>()) as i32;
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if n < 0 {
            return vec![];
        }
        if (n as usize) < pids.len() {
            pids.truncate(n as usize);
            break;
        }
        pids.resize(pids.len() * 2, 0);
    }
    pids.into_iter()
        .filter(|&p| p > 0)
        .filter_map(info)
        .collect()
}

/// The command line, arguments joined by spaces, as `pgrep -f` sees it.
pub fn cmdline(pid: i32) -> Option<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut buf = vec![0u8; 256 * 1024];
    let mut size = buf.len();
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 4 {
        return None;
    }
    let buf = &buf[..size];
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
    // After argc: the executable path, NUL padding, then argv.
    let mut rest = &buf[4..];
    let path_end = rest.iter().position(|&b| b == 0)?;
    rest = &rest[path_end..];
    let start = rest.iter().position(|&b| b != 0)?;
    let args: Vec<String> = rest[start..]
        .split(|&b| b == 0)
        .take(argc)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    Some(args.join(" "))
}

fn any_of(patterns: &[String], whole: bool) -> Option<Regex> {
    if patterns.is_empty() {
        return None;
    }
    let alts: Vec<String> = patterns.iter().map(|p| format!("(?:{p})")).collect();
    let body = alts.join("|");
    Regex::new(&if whole { format!("^(?:{body})$") } else { body }).ok()
}

pub struct Matcher {
    names: Option<Regex>,
    cmdlines: Option<Regex>,
    tree_names: Option<Regex>,
    tree_cmdlines: Option<Regex>,
}

impl Matcher {
    pub fn new(w: &Work) -> Matcher {
        Matcher {
            names: any_of(&w.names, true),
            cmdlines: any_of(&w.cmdlines, false),
            tree_names: any_of(&w.tree_names, true),
            tree_cmdlines: any_of(&w.tree_cmdlines, false),
        }
    }

    /// The current user's work processes. Never this process, its ancestors (the shell and
    /// terminal), its descendants (the watchdog), any other fangov, or other users' processes.
    pub fn find(&self) -> Vec<Proc> {
        self.find_in(
            all(),
            std::process::id() as i32,
            unsafe { libc::getuid() },
            cmdline,
        )
    }

    pub fn find_in(
        &self,
        procs: Vec<Proc>,
        me: i32,
        uid: u32,
        cmdline: impl Fn(i32) -> Option<String>,
    ) -> Vec<Proc> {
        let by_pid: HashMap<i32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for p in &procs {
            children.entry(p.ppid).or_default().push(p.pid);
        }
        let descendants = |roots: &mut Vec<i32>| {
            let mut seen = HashSet::new();
            while let Some(pid) = roots.pop() {
                if seen.insert(pid) {
                    roots.extend(
                        children
                            .get(&pid)
                            .into_iter()
                            .flatten()
                            .filter(|&&c| c != pid),
                    );
                }
            }
            seen
        };

        let mut protected = descendants(&mut vec![me]);
        let mut pid = me;
        while let Some(p) = by_pid.get(&pid) {
            protected.insert(pid);
            if p.ppid == pid || p.ppid <= 1 {
                break;
            }
            pid = p.ppid;
        }

        let candidates: Vec<&Proc> = procs
            .iter()
            .filter(|p| {
                p.uid == uid
                    && !p.zombie
                    && p.pid > 1
                    && !protected.contains(&p.pid)
                    && p.name != "fangov"
            })
            .collect();
        let want_cmdline = self.cmdlines.is_some() || self.tree_cmdlines.is_some();
        let mut work = HashSet::new();
        let mut roots = vec![];
        for p in &candidates {
            let line = if want_cmdline {
                cmdline(p.pid).unwrap_or_default()
            } else {
                String::new()
            };
            let hit = |name: &Option<Regex>, cmd: &Option<Regex>| {
                name.as_ref().is_some_and(|r| r.is_match(&p.name))
                    || cmd.as_ref().is_some_and(|r| r.is_match(&line))
            };
            if hit(&self.names, &self.cmdlines) {
                work.insert(p.pid);
            }
            if hit(&self.tree_names, &self.tree_cmdlines) {
                roots.push(p.pid);
            }
        }
        work.extend(descendants(&mut roots));
        let mut out: Vec<Proc> = candidates
            .into_iter()
            .filter(|p| work.contains(&p.pid))
            .cloned()
            .collect();
        out.sort_by_key(|p| p.pid);
        out
    }
}

/// Sends a signal; a process that has gone is not an error.
pub fn signal(pid: i32, sig: i32) {
    if pid > 1 {
        unsafe { libc::kill(pid, sig) };
    }
}

/// Whether the process exists and has not exited (a zombie awaiting its parent has).
pub fn alive(pid: i32) -> bool {
    pid > 0 && info(pid).is_some_and(|p| !p.zombie)
}

/// `taskpolicy -B -p <pid>`: moves a process out of the background band, so it is not confined
/// to the efficiency cores. It keeps its normal priority.
pub fn not_background(pid: i32) -> bool {
    unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, pid as libc::id_t, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, uid: u32, name: &str) -> Proc {
        Proc {
            pid,
            ppid,
            uid,
            name: name.into(),
            stopped: false,
            zombie: false,
        }
    }

    #[test]
    fn matches_names_cmdlines_and_trees_but_never_the_governor_side() {
        let procs = vec![
            p(1, 0, 0, "launchd"),
            p(10, 1, 501, "Terminal"),
            p(11, 10, 501, "zsh"),
            p(12, 11, 501, "fangov"), // me
            p(13, 12, 501, "fangov"), // my watchdog
            p(20, 11, 501, "cargo"),  // work by name
            p(21, 20, 501, "rustc"),
            p(22, 20, 501, "build-script"), // not matched by name
            p(23, 11, 501, "test-bin"),     // matched by command line
            p(24, 1, 0, "rustc"),           // another user's
            p(30, 11, 501, "make"),         // a tree root
            p(31, 30, 501, "sh"),
            p(32, 31, 501, "cc1"),
            p(40, 11, 501, "ldx"), // names are whole-name matches
        ];
        let w = Work {
            tree_names: vec!["make".into()],
            ..Work::default()
        };
        let m = Matcher::new(&w);
        let cmd = |pid| {
            Some(if pid == 23 {
                "/x/target/debug/deps/test-bin --exact".into()
            } else {
                String::new()
            })
        };
        let found: Vec<i32> = m
            .find_in(procs, 12, 501, cmd)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(found, vec![20, 21, 23, 30, 31, 32]);
    }

    #[test]
    fn a_cargo_process_launching_the_governor_is_protected() {
        // `cargo run -- run` makes cargo an ancestor of the governor.
        let procs = vec![
            p(11, 1, 501, "zsh"),
            p(20, 11, 501, "cargo"),
            p(21, 20, 501, "fangov"),
        ];
        let found = Matcher::new(&Work::default()).find_in(procs, 21, 501, |_| None);
        assert!(found.is_empty());
    }

    #[test]
    fn reads_this_process() {
        let me = info(std::process::id() as i32).unwrap();
        assert!(!me.stopped);
        assert!(cmdline(me.pid).unwrap().contains("fangov"));
    }
}
