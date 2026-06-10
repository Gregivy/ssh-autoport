//! Agentless remote port scanning.
//!
//! We pipe a POSIX shell script into `sh` on the remote (so it works whatever
//! the user's login shell is) and parse whichever tool was available:
//! `ss` -> `netstat` -> raw /proc/net/tcp{,6} (with an inode->process map),
//! the same trick VS Code Remote uses. Nothing is ever installed remotely.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr};

/// A TCP listener found on the remote host.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RemoteApp {
    pub port: u16,
    /// Normalized bind address: "lo" (loopback), "*" (all), or literal.
    pub addr: String,
    pub process: Option<String>,
    pub pid: Option<u32>,
    /// Full command line, fetched in a second pass for owned processes.
    pub cmdline: Option<String>,
}

/// Fed to `sh` on the remote via stdin: `ssh <dest> -- sh`.
pub const SCAN_SCRIPT: &str = r#"LANG=C; export LANG
if command -v ss >/dev/null 2>&1; then
echo "@@FMT ss"
ss -tlnp 2>/dev/null || ss -tln 2>/dev/null
elif command -v netstat >/dev/null 2>&1; then
echo "@@FMT netstat"
netstat -tlnp 2>/dev/null || netstat -tln 2>/dev/null
else
echo "@@FMT proc"
cat /proc/net/tcp /proc/net/tcp6 2>/dev/null
echo "@@MAP"
for d in /proc/[0-9]*; do
c=`cat "$d/comm" 2>/dev/null` || continue
for f in "$d"/fd/*; do
s=`readlink "$f" 2>/dev/null` || continue
case "$s" in "socket:["*) i=${s#socket:[}; echo "${i%]} ${d#/proc/} $c";; esac
done
done
fi
"#;

pub fn parse_scan(out: &str) -> Vec<RemoteApp> {
    let mut fmt = "";
    let mut listeners: Vec<RemoteApp> = Vec::new();
    let mut proc_rows: Vec<(u16, String, u64)> = Vec::new(); // port, addr, inode
    let mut inode_map: BTreeMap<u64, (u32, String)> = BTreeMap::new();
    let mut in_map = false;

    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(f) = line.strip_prefix("@@FMT ") {
            fmt = f.trim();
            continue;
        }
        if line == "@@MAP" {
            in_map = true;
            continue;
        }
        if in_map {
            let mut it = line.splitn(3, ' ');
            if let (Some(ino), Some(pid)) = (it.next(), it.next()) {
                if let (Ok(ino), Ok(pid)) = (ino.parse::<u64>(), pid.parse::<u32>()) {
                    let comm = it.next().unwrap_or("").trim().to_string();
                    inode_map.entry(ino).or_insert((pid, comm));
                }
            }
            continue;
        }
        match fmt {
            "ss" => {
                if let Some(app) = parse_ss_line(line) {
                    listeners.push(app);
                }
            }
            "netstat" => {
                if let Some(app) = parse_netstat_line(line) {
                    listeners.push(app);
                }
            }
            "proc" => {
                if let Some(row) = parse_proc_line(line) {
                    proc_rows.push(row);
                }
            }
            _ => {}
        }
    }

    for (port, addr, inode) in proc_rows {
        let (pid, process) = match inode_map.get(&inode) {
            Some((pid, comm)) if !comm.is_empty() => (Some(*pid), Some(comm.clone())),
            Some((pid, _)) => (Some(*pid), None),
            None => (None, None),
        };
        listeners.push(RemoteApp { port, addr, process, pid, ..Default::default() });
    }

    dedupe(listeners)
}

/// Merge v4/v6 duplicates of the same port, preferring entries that carry a
/// process name and widening the bind address.
fn dedupe(apps: Vec<RemoteApp>) -> Vec<RemoteApp> {
    let mut by_port: BTreeMap<u16, RemoteApp> = BTreeMap::new();
    for app in apps {
        match by_port.get_mut(&app.port) {
            None => {
                by_port.insert(app.port, app);
            }
            Some(cur) => {
                if cur.process.is_none() && app.process.is_some() {
                    cur.process = app.process;
                    cur.pid = app.pid;
                }
                if app.addr == "*" {
                    cur.addr = "*".into();
                }
            }
        }
    }
    by_port.into_values().collect()
}

/// `LISTEN 0 511 127.0.0.1:3000 0.0.0.0:* users:(("node",pid=1234,fd=23))`
fn parse_ss_line(line: &str) -> Option<RemoteApp> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 5 || fields[0] != "LISTEN" {
        return None;
    }
    let (addr, port) = split_addr_port(fields[3])?;
    let (process, pid) = parse_ss_process(line);
    Some(RemoteApp { port, addr, process, pid, ..Default::default() })
}

fn parse_ss_process(line: &str) -> (Option<String>, Option<u32>) {
    let Some(idx) = line.find("((\"") else {
        return (None, None);
    };
    let rest = &line[idx + 3..];
    let name = rest.split('"').next().map(|s| s.to_string());
    let pid = rest
        .find("pid=")
        .and_then(|i| rest[i + 4..].split(|c: char| !c.is_ascii_digit()).next()?.parse().ok());
    (name, pid)
}

/// `tcp 0 0 127.0.0.1:3000 0.0.0.0:* LISTEN 1234/node`
fn parse_netstat_line(line: &str) -> Option<RemoteApp> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 6 || !fields[0].starts_with("tcp") || !fields.contains(&"LISTEN") {
        return None;
    }
    let (addr, port) = split_addr_port(fields[3])?;
    let mut process = None;
    let mut pid = None;
    if let Some(pp) = fields.iter().find(|f| f.contains('/')) {
        if let Some((p, name)) = pp.split_once('/') {
            if let Ok(p) = p.parse::<u32>() {
                pid = Some(p);
                process = Some(name.to_string());
            }
        }
    }
    Some(RemoteApp { port, addr, process, pid, ..Default::default() })
}

/// `0: 0100007F:0BB8 00000000:0000 0A ... uid timeout inode ...`
/// Returns (port, addr, inode) for sockets in LISTEN (st == 0A).
fn parse_proc_line(line: &str) -> Option<(u16, String, u64)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 10 || !fields[0].ends_with(':') || fields[3] != "0A" {
        return None;
    }
    let (addr, port) = decode_hex_addr(fields[1])?;
    let inode: u64 = fields[9].parse().ok()?;
    Some((port, addr, inode))
}

/// Decode kernel hex "ADDR:PORT" from /proc/net/tcp{,6}.
fn decode_hex_addr(s: &str) -> Option<(String, u16)> {
    let (h, p) = s.split_once(':')?;
    let port = u16::from_str_radix(p, 16).ok()?;
    let addr = match h.len() {
        8 => {
            let v = u32::from_str_radix(h, 16).ok()?;
            normalize_v4(Ipv4Addr::from(v.to_le_bytes()))
        }
        32 => {
            // Four 32-bit words, each in host (little-endian) byte order.
            let mut bytes = [0u8; 16];
            for w in 0..4 {
                let v = u32::from_str_radix(&h[w * 8..w * 8 + 8], 16).ok()?;
                bytes[w * 4..w * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
            let v6 = Ipv6Addr::from(bytes);
            if let Some(v4) = v6.to_ipv4_mapped() {
                normalize_v4(v4)
            } else if v6.is_loopback() {
                "lo".into()
            } else if v6 == Ipv6Addr::UNSPECIFIED {
                "*".into()
            } else {
                "v6".into()
            }
        }
        _ => return None,
    };
    Some((addr, port))
}

fn normalize_v4(a: Ipv4Addr) -> String {
    if a.is_loopback() {
        "lo".into()
    } else if a == Ipv4Addr::UNSPECIFIED {
        "*".into()
    } else {
        a.to_string()
    }
}

/// Normalize textual addresses from ss/netstat output and split off the port.
fn split_addr_port(s: &str) -> Option<(String, u16)> {
    let (addr, port) = s.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let addr = addr.trim_matches(|c| c == '[' || c == ']');
    let norm = match addr {
        "" | "*" | "0.0.0.0" | "::" => "*".to_string(),
        "127.0.0.1" | "::1" => "lo".to_string(),
        other => {
            if let Ok(v4) = other.parse::<Ipv4Addr>() {
                normalize_v4(v4)
            } else if other.parse::<Ipv6Addr>().map(|a| a.is_loopback()).unwrap_or(false) {
                "lo".to_string()
            } else {
                other.to_string()
            }
        }
    };
    Some((norm, port))
}

/// Processes that are infrastructure, not "apps" — hidden by default.
const DENY_PROCS: &[&str] = &[
    "sshd", "ssh", "systemd", "systemd-resolve", "systemd-resolved", "init", "dnsmasq", "named",
    "rpcbind", "rpc.statd", "cupsd", "exim4", "master", "sendmail", "chronyd", "ntpd",
    "avahi-daemon", "NetworkManager",
];
const DENY_PORTS: &[u16] = &[22, 25, 53, 111, 631, 5355];

/// Heuristic: is this a system service rather than a user app?
/// `sshd_port` is the port of the very connection we're scanning over.
pub fn is_system(app: &RemoteApp, sshd_port: u16) -> bool {
    if app.port == sshd_port || DENY_PORTS.contains(&app.port) {
        return true;
    }
    if let Some(p) = &app.process {
        if DENY_PROCS.contains(&p.trim_matches('"')) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ss_output() {
        let out = "@@FMT ss\n\
            State  Recv-Q Send-Q Local Address:Port Peer Address:Port Process\n\
            LISTEN 0      511        127.0.0.1:3000      0.0.0.0:*    users:((\"node\",pid=1234,fd=23))\n\
            LISTEN 0      128             [::]:8000         [::]:*    users:((\"python3\",pid=99,fd=5))\n\
            LISTEN 0      128          0.0.0.0:22        0.0.0.0:*\n";
        let apps = parse_scan(out);
        assert_eq!(apps.len(), 3);
        assert_eq!(apps[0], RemoteApp { port: 22, addr: "*".into(), process: None, pid: None, ..Default::default() });
        assert_eq!(apps[1].process.as_deref(), Some("node"));
        assert_eq!(apps[1].addr, "lo");
        assert_eq!(apps[2].port, 8000);
        assert_eq!(apps[2].addr, "*");
        assert_eq!(apps[2].pid, Some(99));
    }

    #[test]
    fn parses_netstat_output() {
        let out = "@@FMT netstat\n\
            Active Internet connections (only servers)\n\
            Proto Recv-Q Send-Q Local Address           Foreign Address         State       PID/Program name\n\
            tcp        0      0 127.0.0.1:5432          0.0.0.0:*               LISTEN      888/postgres\n\
            tcp6       0      0 :::8080                 :::*                    LISTEN      -\n";
        let apps = parse_scan(out);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].port, 5432);
        assert_eq!(apps[0].process.as_deref(), Some("postgres"));
        assert_eq!(apps[1].port, 8080);
        assert_eq!(apps[1].addr, "*");
        assert!(apps[1].process.is_none());
    }

    #[test]
    fn parses_proc_output_with_map() {
        let out = "@@FMT proc\n\
            sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
            0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000 0 5555 1\n\
            1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000 0 6666 1\n\
            2: 0100007F:1234 0200007F:0050 01 00000000:00000000 00:00000000 00000000  1000 0 7777 1\n\
            0: 00000000000000000000000001000000:0FA0 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000 0 8888 1\n\
            @@MAP\n\
            5555 4242 node\n\
            8888 4300 my web app\n";
        let apps = parse_scan(out);
        assert_eq!(apps.len(), 3); // established socket excluded
        assert_eq!(apps[0], RemoteApp { port: 3000, addr: "lo".into(), process: Some("node".into()), pid: Some(4242), ..Default::default() });
        assert_eq!(apps[1], RemoteApp { port: 4000, addr: "lo".into(), process: Some("my web app".into()), pid: Some(4300), ..Default::default() });
        assert_eq!(apps[2], RemoteApp { port: 8080, addr: "*".into(), process: None, pid: None, ..Default::default() });
    }

    #[test]
    fn dedupes_v4_v6_same_port() {
        let out = "@@FMT ss\n\
            LISTEN 0 128 0.0.0.0:9000 0.0.0.0:*\n\
            LISTEN 0 128 [::]:9000 [::]:* users:((\"gunicorn\",pid=7,fd=5))\n";
        let apps = parse_scan(out);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].process.as_deref(), Some("gunicorn"));
    }

    #[test]
    fn system_filter() {
        let sshd = RemoteApp { port: 2222, addr: "*".into(), process: Some("sshd".into()), pid: None, ..Default::default() };
        assert!(is_system(&sshd, 2222));
        let app = RemoteApp { port: 3000, addr: "lo".into(), process: Some("node".into()), pid: None, ..Default::default() };
        assert!(!is_system(&app, 22));
        let dns = RemoteApp { port: 53, addr: "lo".into(), process: None, pid: None, ..Default::default() };
        assert!(is_system(&dns, 22));
    }
}
