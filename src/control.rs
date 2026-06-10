//! SSH ControlMaster management and dynamic port forwards.
//!
//! Strategy per host:
//!  1. If the user's own ssh config provides a ControlMaster (a live socket
//!     at the resolved ControlPath), piggyback on it ("shared").
//!  2. Otherwise reuse/create our own master on a private control socket.
//!     Auth runs in BatchMode (keys/agent only) so we never hang on a prompt.
//!
//! Every operation after that — scans, `-O forward`, `-O cancel` — talks to
//! the live socket with `-F /dev/null`. This is essential, not cosmetic: with
//! the user's config loaded, a mux client re-sends every config LocalForward
//! along with ours (failing because their session already binds those ports),
//! and host configs with RemoteCommand/SessionType/ProxyCommand would break
//! command execution. The config only applies where it belongs: once, when a
//! connection is actually established.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::remote::SCAN_SCRIPT;
use crate::util::{first_line, output_timeout};

/// Everything needed to address a live master from any thread.
#[derive(Debug, Clone)]
pub struct MasterRef {
    pub dest: String,
    pub socket: PathBuf,
}

/// A live (or adopted) master connection for one host.
#[derive(Debug)]
pub struct Master {
    pub r: MasterRef,
    /// True when this is the user's own ControlMaster — never torn down by us.
    pub external: bool,
    child: Option<Child>,
}

impl MasterRef {
    fn base(&self) -> Command {
        let mut c = Command::new("ssh");
        c.arg("-F").arg("/dev/null"); // mux ops: socket only, no config
        c.arg("-o").arg("BatchMode=yes");
        // If the socket has died, plain ssh would fall back to a direct
        // connection; a failing ProxyCommand turns that into a fast error
        // instead of a surprise second login.
        c.arg("-o").arg("ProxyCommand=false");
        c.arg("-S").arg(&self.socket);
        c
    }

    pub fn check(&self) -> bool {
        let mut c = self.base();
        c.arg("-O").arg("check").arg(&self.dest);
        matches!(
            output_timeout(c, Duration::from_secs(6), None),
            Ok(o) if o.status.success()
        )
    }

    /// Run the port scan on the remote over the multiplexed connection.
    /// The script goes over stdin into `sh`, so it works regardless of the
    /// user's login shell.
    pub fn scan(&self) -> Result<String, String> {
        let mut c = self.base();
        c.arg("-T");
        c.arg(&self.dest).arg("--").arg("sh");
        let out = output_timeout(c, Duration::from_secs(25), Some(SCAN_SCRIPT))?;
        if !out.status.success() && out.stdout.is_empty() {
            return Err(first_line(&out.stderr));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn forward(&self, lport: u16, rport: u16) -> Result<(), String> {
        self.mux_forward("forward", lport, rport)
    }

    pub fn cancel(&self, lport: u16, rport: u16) -> Result<(), String> {
        self.mux_forward("cancel", lport, rport)
    }

    fn mux_forward(&self, op: &str, lport: u16, rport: u16) -> Result<(), String> {
        let mut c = self.base();
        c.arg("-O")
            .arg(op)
            .arg("-L")
            .arg(format!("127.0.0.1:{lport}:localhost:{rport}"))
            .arg(&self.dest);
        let out = output_timeout(c, Duration::from_secs(10), None)?;
        if out.status.success() {
            Ok(())
        } else {
            Err(first_line(&out.stderr))
        }
    }
}

impl Master {
    /// Establish (or adopt) a master for this destination.
    /// `control_path` is the user's resolved ControlPath from `ssh -G`, if any.
    pub fn connect(
        dest: &str,
        extra: &[String],
        key: &str,
        control_path: Option<PathBuf>,
    ) -> Result<Master, String> {
        // 1. The user's own ControlMaster, if one is alive at their ControlPath.
        if let Some(cp) = control_path {
            let shared = MasterRef { dest: dest.to_string(), socket: cp };
            if shared.check() {
                return Ok(Master { r: shared, external: true, child: None });
            }
        }

        // 2. Our own socket — possibly still alive from a previous run.
        let sock = socket_path(key)?;
        let own = MasterRef { dest: dest.to_string(), socket: sock.clone() };
        if own.check() {
            return Ok(Master { r: own, external: false, child: None });
        }
        let _ = std::fs::remove_file(&sock); // stale socket file

        // 3. Spawn a fresh master. This is the only place the user's config
        //    applies (HostName, keys, ProxyJump...), so neutralize the parts
        //    that belong to *their* sessions, not ours. No -f: we keep the
        //    child handle so the master dies with us even if -O exit fails.
        let mut c = Command::new("ssh");
        c.arg("-M")
            .arg("-N")
            .arg("-S")
            .arg(&sock)
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("ServerAliveCountMax=3")
            // Config RemoteCommand (e.g. Jupyter tunnels) clashes with -N
            // sessions and our scans; the user's LocalForwards are already
            // bound by their own session — don't fight over them.
            .arg("-o")
            .arg("RemoteCommand=none")
            .arg("-o")
            .arg("ClearAllForwardings=yes")
            .args(extra)
            .arg(dest)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = c.spawn().map_err(|e| format!("cannot run ssh: {e}"))?;

        // Drain stderr in the background so the child never blocks on a full
        // pipe; keep what we read for error reporting.
        let mut stderr = child.stderr.take().unwrap();
        let errbuf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        {
            let errbuf = errbuf.clone();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut s = String::new();
                let _ = stderr.read_to_string(&mut s);
                *errbuf.lock().unwrap() = s;
            });
        }

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                std::thread::sleep(Duration::from_millis(100)); // let reader finish
                let err = errbuf.lock().unwrap().clone();
                let msg = err
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("connection failed")
                    .to_string();
                let hint = if msg.contains("Permission denied")
                    || msg.contains("Interactive authentication")
                {
                    " — needs non-interactive auth: add your key to ssh-agent, or enable ControlMaster in ~/.ssh/config so I can share your session"
                } else {
                    ""
                };
                return Err(format!("{msg}{hint}"));
            }
            if own.check() {
                return Ok(Master { r: own, external: false, child: Some(child) });
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out establishing connection".into());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    pub fn alive(&self) -> bool {
        self.r.check()
    }

    /// Tear down: exit our own master (never the user's).
    pub fn close(&mut self) {
        if !self.external {
            let mut c = self.r.base();
            c.arg("-O").arg("exit").arg(&self.r.dest);
            let _ = output_timeout(c, Duration::from_secs(5), None);
            if let Some(child) = &mut self.child {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_file(&self.r.socket);
        }
    }
}

fn socket_path(key: &str) -> Result<PathBuf, String> {
    let dir = if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(rt).join("ssh-autoport")
    } else {
        std::env::temp_dir().join(format!(
            "ssh-autoport-{}",
            std::env::var("USER").unwrap_or_else(|_| "u".into())
        ))
    };
    create_private_dir(&dir).map_err(|e| format!("cannot create socket dir: {e}"))?;
    let mut h = DefaultHasher::new();
    key.hash(&mut h);
    Ok(dir.join(format!("m-{:012x}.sock", h.finish() & 0xffff_ffff_ffff)))
}

fn create_private_dir(dir: &PathBuf) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}
