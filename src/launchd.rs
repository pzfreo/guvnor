//! `guvnor install` / `uninstall`: a per-user LaunchAgent with KeepAlive.

use crate::config::Config;
use std::path::PathBuf;
use std::process::Command;

const LABEL: &str = "local.guvnor";

fn plist_path() -> PathBuf {
    crate::home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn install() -> Result<(), String> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| e.to_string())?;
    let exe = exe.to_string_lossy().into_owned();
    if exe.contains("/target/") {
        eprintln!(
            "warning: {exe} is inside a cargo target directory; `cargo clean` would remove it."
        );
        eprintln!("         `cargo install --path .` puts it in ~/.cargo/bin instead.");
    }
    let log = crate::home().join("Library/Logs/guvnor.log");
    // A non-default config or state directory set now is pinned into the agent.
    let env: String = ["GUVNOR_CONFIG", "GUVNOR_STATE"]
        .iter()
        .filter_map(|k| {
            std::env::var(k)
                .ok()
                .map(|v| format!("<key>{k}</key><string>{}</string>", esc(&v)))
        })
        .collect();
    let env = if env.is_empty() {
        String::new()
    } else {
        format!("\n  <key>EnvironmentVariables</key><dict>{env}</dict>")
    };
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>run</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>{}
</dict>
</plist>
"#,
        esc(&exe),
        esc(&log.to_string_lossy()),
        esc(&log.to_string_lossy()),
        env
    );
    let path = plist_path();
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    // Replace a loaded copy, if any.
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{}/{LABEL}", domain())])
        .output();
    std::fs::write(&path, plist).map_err(|e| e.to_string())?;
    let out = Command::new("launchctl")
        .args(["bootstrap", &domain(), &path.to_string_lossy()])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    println!(
        "installed {} (runs {exe} run; log {})",
        path.display(),
        log.display()
    );
    Ok(())
}

pub fn uninstall(c: &Config) -> Result<(), String> {
    // bootout sends SIGTERM; the governor continues all work before it exits.
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{}/{LABEL}", domain())])
        .output();
    let path = plist_path();
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    // Belt and braces, in case the governor was killed rather than stopped.
    std::thread::sleep(std::time::Duration::from_millis(500));
    if !crate::running().is_some() {
        crate::state::release_stopped();
        for p in crate::procs::Matcher::new(&c.work).find() {
            crate::procs::signal(p.pid, libc::SIGCONT);
        }
    }
    println!("uninstalled {}", path.display());
    Ok(())
}
