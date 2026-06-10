//! Local port availability and allocation.

use std::net::TcpListener;
use std::process::Command;
use std::time::Duration;

use crate::util::output_timeout;

pub fn is_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Pick a local port: first free entry of `preferred` (privileged ports are
/// never auto-picked), otherwise let the kernel hand us an ephemeral one.
/// `taken` are ports already claimed by our own pending/active forwards.
pub fn find_free(preferred: &[u16], taken: &[u16]) -> Option<u16> {
    for &p in preferred {
        if p >= 1024 && !taken.contains(&p) && is_free(p) {
            return Some(p);
        }
    }
    let l = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let port = l.local_addr().ok()?.port();
    drop(l);
    Some(port)
}

/// Best-effort: who is listening on this local port? For error messages.
pub fn who_uses(port: u16) -> String {
    for (prog, args) in [("ss", ["-tlnp"]), ("netstat", ["-tlnp"])] {
        let mut cmd = Command::new(prog);
        cmd.args(args);
        let Ok(out) = output_timeout(cmd, Duration::from_secs(3), None) else {
            continue;
        };
        let needle = format!(":{port}");
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if !line.contains("LISTEN") {
                continue;
            }
            // Local-address column ends with ":<port>".
            if !line.split_whitespace().take(5).any(|f| f.ends_with(&needle)) {
                continue;
            }
            if let Some(idx) = line.find("((\"") {
                let name: String = line[idx + 3..].chars().take_while(|&c| c != '"').collect();
                return format!("used by \"{name}\"");
            }
            if let Some(pp) = line.split_whitespace().find(|f| {
                f.contains('/') && f.split('/').next().is_some_and(|p| p.parse::<u32>().is_ok())
            }) {
                return format!("used by \"{}\"", pp.split_once('/').unwrap().1);
            }
            return "already in use by another process".into();
        }
    }
    "already in use by another process".into()
}
