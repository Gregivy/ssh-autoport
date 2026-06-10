//! Discover active `ssh` client processes on the local machine and resolve
//! their canonical destination identity via `ssh -G`.

use std::process::Command;
use std::time::Duration;

use crate::util::output_timeout;

/// An active interactive ssh connection found in the local process table.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalConn {
    pub pid: u32,
    /// Destination as typed: alias, host, user@host or ssh:// URL.
    pub dest: String,
    /// Carried-through ssh CLI options that affect identity/auth
    /// (-p, -l, -F, -i, -J), so our own ssh invocations match.
    pub extra: Vec<String>,
    /// Raw `-L` forward specs this session was started with.
    pub lforwards: Vec<String>,
}

/// Canonical identity resolved through `ssh -G` (applies user config).
#[derive(Debug, Clone)]
pub struct Resolved {
    pub user: String,
    pub hostname: String,
    pub port: u16,
    /// Config LocalForwards that target the server itself (loopback):
    /// (local port, remote port).
    pub lforwards: Vec<(u16, u16)>,
    /// The resolved ControlPath from the user's config, if usable — lets us
    /// share their ControlMaster instead of opening our own connection.
    pub control_path: Option<std::path::PathBuf>,
}

/// ssh single-letter options that consume a value.
const ARG_OPTS: &[char] = &[
    'b', 'B', 'c', 'D', 'E', 'e', 'F', 'I', 'i', 'J', 'L', 'l', 'm', 'O', 'o', 'p', 'Q', 'R', 'S',
    'W', 'w',
];
/// Options we replicate on our own ssh invocations.
const CARRY_OPTS: &[char] = &['F', 'p', 'l', 'i', 'J'];

pub fn discover() -> Vec<LocalConn> {
    let mut cmd = Command::new("ps");
    cmd.args(["-eo", "pid=,args="]);
    let out = match output_timeout(cmd, Duration::from_secs(5), None) {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut conns: Vec<LocalConn> = text.lines().filter_map(parse_ps_line).collect();
    conns.sort_by(|a, b| (&a.dest, &a.extra).cmp(&(&b.dest, &b.extra)));
    conns.dedup_by(|a, b| a.dest == b.dest && a.extra == b.extra);
    conns
}

fn parse_ps_line(line: &str) -> Option<LocalConn> {
    let line = line.trim_start();
    let (pid_s, rest) = line.split_once(char::is_whitespace)?;
    let pid: u32 = pid_s.parse().ok()?;
    let args: Vec<&str> = rest.split_whitespace().collect();
    let prog = args.first()?;
    // Only the plain ssh client (not sshd, ssh-agent, sftp, scp...).
    let base = prog.rsplit('/').next()?;
    if base != "ssh" {
        return None;
    }
    // Skip our own master/forward/control processes.
    if rest.contains("ssh-autoport") {
        return None;
    }
    parse_ssh_args(&args[1..], pid)
}

fn parse_ssh_args(args: &[&str], pid: u32) -> Option<LocalConn> {
    let mut extra: Vec<String> = Vec::new();
    let mut lforwards: Vec<String> = Vec::new();
    let mut dest: Option<String> = None;
    let mut dest_idx = 0usize;
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i];
        if a == "--" {
            dest = args.get(i + 1).map(|s| s.to_string());
            dest_idx = i + 1;
            break;
        }
        if let Some(flags) = a.strip_prefix('-') {
            if flags.is_empty() {
                return None; // bare "-": not a normal client invocation
            }
            let chars: Vec<char> = flags.chars().collect();
            let mut j = 0usize;
            while j < chars.len() {
                let c = chars[j];
                // -O (mux control) and -W (stdio forward, used by ProxyJump
                // internals) are not user sessions worth tracking.
                if c == 'O' || c == 'W' {
                    return None;
                }
                if ARG_OPTS.contains(&c) {
                    let val: String = if j + 1 < chars.len() {
                        chars[j + 1..].iter().collect()
                    } else {
                        i += 1;
                        args.get(i)?.to_string()
                    };
                    if CARRY_OPTS.contains(&c) {
                        extra.push(format!("-{c}"));
                        extra.push(val);
                    } else if c == 'L' {
                        lforwards.push(val);
                    }
                    break;
                }
                j += 1;
            }
        } else {
            dest = Some(a.to_string());
            dest_idx = i;
            break;
        }
        i += 1;
    }
    let dest = dest?;
    // A trailing remote command means a one-off exec session (often short
    // lived, possibly ours via a shared master) — skip those.
    if args.len() > dest_idx + 1 {
        return None;
    }
    Some(LocalConn { pid, dest, extra, lforwards })
}

/// Resolve the effective user/hostname/port via `ssh -G` (no connection made).
pub fn resolve(dest: &str, extra: &[String]) -> Option<Resolved> {
    let mut cmd = Command::new("ssh");
    cmd.arg("-G").args(extra).arg(dest);
    let out = output_timeout(cmd, Duration::from_secs(5), None).ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut user = None;
    let mut hostname = None;
    let mut port = None;
    let mut lforwards = Vec::new();
    let mut control_path = None;
    for l in text.lines() {
        if let Some(v) = l.strip_prefix("user ") {
            user = Some(v.trim().to_string());
        } else if let Some(v) = l.strip_prefix("hostname ") {
            hostname = Some(v.trim().to_string());
        } else if let Some(v) = l.strip_prefix("port ") {
            port = v.trim().parse::<u16>().ok();
        } else if let Some(v) = l.strip_prefix("localforward ") {
            if let Some(f) = parse_g_localforward(v) {
                lforwards.push(f);
            }
        } else if let Some(v) = l.strip_prefix("controlpath ") {
            let v = v.trim();
            // "none" means unset; a leftover '%' token means this ssh didn't
            // expand it, so we can't use the path literally.
            if v != "none" && !v.contains('%') && !v.is_empty() {
                control_path = Some(std::path::PathBuf::from(v));
            }
        }
    }
    Some(Resolved {
        user: user?,
        hostname: hostname?,
        port: port?,
        lforwards,
        control_path,
    })
}

/// Split a forward spec on ':' while respecting IPv6 brackets;
/// brackets themselves are dropped.
fn split_fwd(s: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut depth = 0u32;
    for ch in s.chars() {
        match ch {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            ':' if depth == 0 => parts.push(String::new()),
            c => parts.last_mut().unwrap().push(c),
        }
    }
    parts
}

pub fn is_loopback_host(h: &str) -> bool {
    h.eq_ignore_ascii_case("localhost") || h == "127.0.0.1" || h == "::1"
}

/// Parse an `-L` spec: `[bind:]lport:host:rport` -> (lport, host, rport).
pub fn parse_l_spec(spec: &str) -> Option<(u16, String, u16)> {
    let p = split_fwd(spec);
    let (lp, host, rp) = match p.len() {
        3 => (&p[0], &p[1], &p[2]),
        4 => (&p[1], &p[2], &p[3]),
        _ => return None,
    };
    Some((lp.parse().ok()?, host.clone(), rp.parse().ok()?))
}

/// Parse the tail of an `ssh -G` "localforward" line:
/// `8888 [localhost]:8888` or `[127.0.0.1]:6006 [127.0.0.1]:6006`.
/// Returns (local port, remote port) only for forwards that target the
/// server itself (loopback connect host).
fn parse_g_localforward(v: &str) -> Option<(u16, u16)> {
    let mut toks = v.split_whitespace();
    let listen = split_fwd(toks.next()?);
    let connect = split_fwd(toks.next()?);
    let lport: u16 = listen.last()?.parse().ok()?;
    let [host, rport] = connect.as_slice() else {
        return None;
    };
    if !is_loopback_host(host) {
        return None;
    }
    Some((lport, rport.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_session() {
        let c = parse_ps_line("  1234 ssh web01").unwrap();
        assert_eq!(c.pid, 1234);
        assert_eq!(c.dest, "web01");
        assert!(c.extra.is_empty());
    }

    #[test]
    fn parses_options_and_carries_identity_opts() {
        let c = parse_ps_line("77 /usr/bin/ssh -p 2222 -l bob -i /k/id -C -v host.example.com")
            .unwrap();
        assert_eq!(c.dest, "host.example.com");
        assert_eq!(c.extra, vec!["-p", "2222", "-l", "bob", "-i", "/k/id"]);
    }

    #[test]
    fn parses_attached_option_value() {
        let c = parse_ps_line("9 ssh -p2222 user@h").unwrap();
        assert_eq!(c.dest, "user@h");
        assert_eq!(c.extra, vec!["-p", "2222"]);
    }

    #[test]
    fn skips_remote_command_control_and_nonssh() {
        assert!(parse_ps_line("1 ssh host uptime").is_none());
        assert!(parse_ps_line("2 ssh -O check host").is_none());
        assert!(parse_ps_line("3 ssh -W h:22 jump").is_none());
        assert!(parse_ps_line("4 sshd: user@pts/0").is_none());
        assert!(parse_ps_line("5 ssh -M -N -S /run/ssh-autoport/x.sock host").is_none());
    }

    #[test]
    fn keeps_user_forward_daemons_and_captures_l_specs() {
        let c = parse_ps_line("6 ssh -N -L 8080:localhost:80 -L 127.0.0.1:9090:127.0.0.1:90 web01")
            .unwrap();
        assert_eq!(c.dest, "web01");
        assert_eq!(c.lforwards, vec!["8080:localhost:80", "127.0.0.1:9090:127.0.0.1:90"]);
    }

    #[test]
    fn parses_l_specs() {
        assert_eq!(parse_l_spec("8888:localhost:8888"), Some((8888, "localhost".into(), 8888)));
        assert_eq!(
            parse_l_spec("127.0.0.1:6006:127.0.0.1:6006"),
            Some((6006, "127.0.0.1".into(), 6006))
        );
        assert_eq!(parse_l_spec("[::1]:7000:[::1]:70"), Some((7000, "::1".into(), 70)));
        assert_eq!(parse_l_spec("nonsense"), None);
    }

    #[test]
    fn parses_g_localforward_lines() {
        assert_eq!(parse_g_localforward("8888 [localhost]:8888"), Some((8888, 8888)));
        assert_eq!(parse_g_localforward("[127.0.0.1]:6006 [127.0.0.1]:6006"), Some((6006, 6006)));
        // Forward through the server to another machine: not this server's port.
        assert_eq!(parse_g_localforward("9999 [otherhost.internal]:80"), None);
    }
}
