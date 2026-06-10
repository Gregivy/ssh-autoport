//! Background orchestrator: watches local ssh connections, keeps a master
//! per host, scans remote listeners, auto-assigns + forwards local ports,
//! and persists assignments. Talks to the UI via Cmd/Ev channels.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use crate::control::Master;
use crate::discover::{self, LocalConn};
use crate::ports;
use crate::remote::{self, RemoteApp};
use crate::state::{Assignment, State};
use crate::types::{AppView, Cmd, Ev, FwdView, HostView, MasterView};
use crate::util::now_unix;

pub struct Options {
    pub interval: Duration,
    pub auto: bool,
}

enum WMsg {
    Cmd(Cmd),
    Tick,
    MasterDone { key: String, result: Result<Master, String> },
    ScanDone { key: String, result: Result<Vec<RemoteApp>, String> },
    FwdDone { key: String, rport: u16, lport: u16, pin: bool, result: Result<(), String> },
}

enum MState {
    Connecting,
    Ready(Master),
    Failed { err: String, at: Instant },
}

struct AppW {
    rport: u16,
    addr: String,
    process: Option<String>,

    system: bool,
    lport: Option<u16>,
    status: FwdView,
    muted: bool,
    pinned: bool,
    /// One automatic retry with a fresh port after a local bind failure.
    retried_new_port: bool,
}

struct HostW {
    key: String,
    title: String,
    dest: String,
    extra: Vec<String>,
    sshd_port: u16,
    control_path: Option<std::path::PathBuf>,
    /// remote port -> local port already forwarded by the user's own
    /// sessions (config LocalForward or CLI -L). Rebuilt every tick.
    ext_forwards: BTreeMap<u16, u16>,
    master: MState,
    connect_inflight: bool,
    apps: BTreeMap<u16, AppW>,
    scanning: bool,
    last_scan: Option<Instant>,
    scan_err: Option<String>,
    present: bool,
}

pub fn run(cmd_rx: Receiver<Cmd>, ev_tx: Sender<Ev>, opts: Options) {
    let (wtx, wrx) = std::sync::mpsc::channel::<WMsg>();

    // Bridge UI commands into the worker channel.
    {
        let wtx = wtx.clone();
        std::thread::spawn(move || {
            for c in cmd_rx {
                let quit = matches!(c, Cmd::Quit);
                if wtx.send(WMsg::Cmd(c)).is_err() || quit {
                    break;
                }
            }
        });
    }
    // Ticker: fires immediately, then every interval.
    {
        let wtx = wtx.clone();
        let iv = opts.interval;
        std::thread::spawn(move || loop {
            if wtx.send(WMsg::Tick).is_err() {
                break;
            }
            std::thread::sleep(iv);
        });
    }

    let mut w = Worker {
        hosts: BTreeMap::new(),
        state: State::load(),
        auto: opts.auto,
        interval: opts.interval,
        ev_tx,
        wtx,
        resolve_cache: HashMap::new(),
    };
    let _ = w.ev_tx.send(Ev::AutoMode(w.auto));

    for msg in wrx {
        let quit = matches!(msg, WMsg::Cmd(Cmd::Quit));
        w.handle(msg);
        w.push_snapshot();
        if quit {
            w.cleanup();
            let _ = w.ev_tx.send(Ev::CleanedUp);
            break;
        }
    }
}

struct Worker {
    hosts: BTreeMap<String, HostW>,
    state: State,
    auto: bool,
    interval: Duration,
    ev_tx: Sender<Ev>,
    wtx: Sender<WMsg>,
    /// (dest, extra) -> resolved identity, so we don't fork `ssh -G` each tick.
    resolve_cache: HashMap<(String, Vec<String>), Option<discover::Resolved>>,
}

impl Worker {
    fn handle(&mut self, msg: WMsg) {
        match msg {
            WMsg::Tick => self.on_tick(),
            WMsg::MasterDone { key, result } => self.on_master(key, result),
            WMsg::ScanDone { key, result } => self.on_scan(key, result),
            WMsg::FwdDone { key, rport, lport, pin, result } => {
                self.on_fwd(key, rport, lport, pin, result)
            }
            WMsg::Cmd(cmd) => self.on_cmd(cmd),
        }
    }

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Assign { host, rport, lport } => self.assign(host, rport, lport),
            Cmd::Toggle { host, rport } => self.toggle(host, rport),
            Cmd::Refresh => {
                let keys: Vec<String> = self.hosts.keys().cloned().collect();
                for k in keys {
                    self.spawn_scan(&k, true);
                }
            }
            Cmd::ToggleAuto => {
                self.auto = !self.auto;
                let _ = self.ev_tx.send(Ev::AutoMode(self.auto));
                if self.auto {
                    let keys: Vec<String> = self.hosts.keys().cloned().collect();
                    for k in keys {
                        self.auto_forward_pass(&k);
                    }
                }
            }
            Cmd::Quit => {}
        }
    }

    // ---- tick: watch local ssh connections ----

    fn on_tick(&mut self) {
        let conns = discover::discover();
        for h in self.hosts.values_mut() {
            h.present = false;
        }
        // Forwards the user's own sessions provide, gathered fresh each tick.
        let mut ext: HashMap<String, BTreeMap<u16, u16>> = HashMap::new();
        for conn in conns {
            let Some(r) = self.resolved(&conn) else {
                continue;
            };
            let key = format!("{}@{}:{}", r.user, r.hostname, r.port);
            let fwds = ext.entry(key.clone()).or_default();
            for (lp, rp) in &r.lforwards {
                fwds.insert(*rp, *lp);
            }
            for spec in &conn.lforwards {
                if let Some((lp, host, rp)) = discover::parse_l_spec(spec) {
                    if discover::is_loopback_host(&host) {
                        fwds.insert(rp, lp);
                    }
                }
            }
            let host = self.hosts.entry(key.clone()).or_insert_with(|| HostW {
                key: key.clone(),
                title: format!("{}@{}", r.user, r.hostname),
                dest: conn.dest.clone(),
                extra: conn.extra.clone(),
                sshd_port: r.port,
                control_path: r.control_path.clone(),
                ext_forwards: BTreeMap::new(),
                master: MState::Connecting,
                connect_inflight: false,
                apps: BTreeMap::new(),
                scanning: false,
                last_scan: None,
                scan_err: None,
                present: true,
            });
            host.present = true;
        }
        for (key, fwds) in ext {
            if let Some(h) = self.hosts.get_mut(&key) {
                h.ext_forwards = fwds;
            }
        }

        // Hosts whose ssh connection vanished: tear down what we created.
        let gone: Vec<String> = self
            .hosts
            .iter()
            .filter(|(_, h)| !h.present)
            .map(|(k, _)| k.clone())
            .collect();
        for key in gone {
            if let Some(host) = self.hosts.remove(&key) {
                teardown_host(host);
            }
        }

        // Connect new hosts, retry failed ones, kick periodic scans.
        let keys: Vec<String> = self.hosts.keys().cloned().collect();
        for key in keys {
            self.connect_if_needed(&key);
            self.spawn_scan(&key, false);
        }
    }

    fn resolved(&mut self, conn: &LocalConn) -> Option<discover::Resolved> {
        let ck = (conn.dest.clone(), conn.extra.clone());
        self.resolve_cache
            .entry(ck)
            .or_insert_with(|| discover::resolve(&conn.dest, &conn.extra))
            .clone()
    }

    // ---- master lifecycle ----

    fn connect_if_needed(&mut self, key: &str) {
        let Some(host) = self.hosts.get_mut(key) else { return };
        if host.connect_inflight {
            return;
        }
        let go = match &host.master {
            MState::Connecting => true,
            MState::Failed { at, .. } => at.elapsed() > Duration::from_secs(30),
            MState::Ready(_) => false,
        };
        if !go {
            return;
        }
        host.master = MState::Connecting;
        host.connect_inflight = true;
        let dest = host.dest.clone();
        let extra = host.extra.clone();
        let control_path = host.control_path.clone();
        let key = key.to_string();
        let wtx = self.wtx.clone();
        std::thread::spawn(move || {
            let result = Master::connect(&dest, &extra, &key, control_path);
            let _ = wtx.send(WMsg::MasterDone { key, result });
        });
    }

    fn on_master(&mut self, key: String, result: Result<Master, String>) {
        let Some(host) = self.hosts.get_mut(&key) else {
            // Host disappeared while connecting; close what we just opened.
            if let Ok(mut m) = result {
                std::thread::spawn(move || m.close());
            }
            return;
        };
        host.connect_inflight = false;
        match result {
            Ok(m) => {
                host.master = MState::Ready(m);
                host.scan_err = None;
                self.spawn_scan(&key, true);
            }
            Err(e) => {
                host.master = MState::Failed { err: e, at: Instant::now() };
            }
        }
    }

    // ---- scanning ----

    fn spawn_scan(&mut self, key: &str, force: bool) {
        let Some(host) = self.hosts.get_mut(key) else { return };
        let MState::Ready(master) = &host.master else { return };
        if host.scanning {
            return;
        }
        if !force {
            if let Some(t) = host.last_scan {
                if t.elapsed() < self.interval {
                    return;
                }
            }
        }
        host.scanning = true;
        let mref = master.r.clone();
        let key = key.to_string();
        let wtx = self.wtx.clone();
        std::thread::spawn(move || {
            let result = mref.scan().map(|out| remote::parse_scan(&out));
            let _ = wtx.send(WMsg::ScanDone { key, result });
        });
    }

    fn on_scan(&mut self, key: String, result: Result<Vec<RemoteApp>, String>) {
        let Some(host) = self.hosts.get_mut(&key) else { return };
        host.scanning = false;
        host.last_scan = Some(Instant::now());
        let apps = match result {
            Ok(a) => {
                host.scan_err = None;
                a
            }
            Err(e) => {
                host.scan_err = Some(e);
                // If the master died (e.g. network drop), reconnect.
                if let MState::Ready(m) = &host.master {
                    if !m.alive() {
                        // Backdate the failure so the next tick reconnects
                        // immediately instead of waiting the 30s cooldown.
                        let at = Instant::now()
                            .checked_sub(Duration::from_secs(31))
                            .unwrap_or_else(Instant::now);
                        host.master = MState::Failed { err: "connection lost".into(), at };
                        for app in host.apps.values_mut() {
                            app.status = FwdView::Off;
                        }
                    }
                }
                return;
            }
        };

        let sshd_port = host.sshd_port;
        let mut seen: Vec<u16> = Vec::new();
        for ra in apps {
            seen.push(ra.port);
            let system = remote::is_system(&ra, sshd_port);
            let ext_lport = host.ext_forwards.get(&ra.port).copied();
            match host.apps.get_mut(&ra.port) {
                Some(app) => {
                    app.addr = ra.addr;
                    if ra.process.is_some() {
                        app.process = ra.process;
                    }
                    app.system = system;
                    match (&app.status, ext_lport) {
                        // The user's own session forwards this port; ours (if
                        // any) stays untouched, but idle apps adopt theirs.
                        (FwdView::Off | FwdView::Error(_), Some(lp)) => {
                            app.status = FwdView::External;
                            app.lport = Some(lp);
                        }
                        // Their forward disappeared (session/config changed):
                        // release it so auto-forward can take over.
                        (FwdView::External, None) => {
                            app.status = FwdView::Off;
                            app.lport = None;
                        }
                        (FwdView::External, Some(lp)) => app.lport = Some(lp),
                        _ => {}
                    }
                }
                None => {
                    let remembered = self.state.get(&key, ra.port);
                    let (status, lport) = match ext_lport {
                        Some(lp) => (FwdView::External, Some(lp)),
                        None => (FwdView::Off, None),
                    };
                    host.apps.insert(
                        ra.port,
                        AppW {
                            rport: ra.port,
                            addr: ra.addr,
                            process: ra.process,
                            system,
                            lport,
                            status,
                            muted: false,
                            pinned: remembered.map(|a| a.pinned).unwrap_or(false),
                            retried_new_port: false,
                        },
                    );
                }
            }
        }

        // Apps that vanished from the remote: cancel their forwards.
        let gone: Vec<u16> = host.apps.keys().filter(|p| !seen.contains(p)).cloned().collect();
        for rport in gone {
            if let Some(app) = host.apps.remove(&rport) {
                if let (FwdView::Active | FwdView::Pending, Some(lport)) = (&app.status, app.lport)
                {
                    if let MState::Ready(m) = &host.master {
                        let mref = m.r.clone();
                        std::thread::spawn(move || {
                            let _ = mref.cancel(lport, rport);
                        });
                    }
                }
            }
        }

        self.auto_forward_pass(&key);
    }

    // ---- forwarding ----

    /// Local ports claimed by our own active/pending forwards (any host).
    fn used_lports(&self) -> Vec<u16> {
        self.hosts
            .values()
            .flat_map(|h| h.apps.values())
            .filter(|a| matches!(a.status, FwdView::Active | FwdView::Pending))
            .filter_map(|a| a.lport)
            .collect()
    }

    fn auto_forward_pass(&mut self, key: &str) {
        if !self.auto {
            return;
        }
        let Some(host) = self.hosts.get(key) else { return };
        if !matches!(host.master, MState::Ready(_)) {
            return;
        }
        let candidates: Vec<u16> = host
            .apps
            .values()
            .filter(|a| !a.system && !a.muted && a.status == FwdView::Off)
            .map(|a| a.rport)
            .collect();
        for rport in candidates {
            self.start_forward(key, rport);
        }
    }

    /// Pick a local port (remembered -> same-as-remote -> ephemeral) and
    /// request the forward.
    fn start_forward(&mut self, key: &str, rport: u16) {
        let taken = self.used_lports();
        let Some(host) = self.hosts.get_mut(key) else { return };
        let MState::Ready(master) = &host.master else { return };
        let mref = master.r.clone();
        let Some(app) = host.apps.get_mut(&rport) else { return };

        let mut prefs: Vec<u16> = Vec::new();
        if let Some(lp) = app.lport {
            prefs.push(lp);
        }
        if let Some(a) = self.state.get(key, rport) {
            prefs.push(a.local_port);
        }
        prefs.push(rport);
        let Some(lport) = ports::find_free(&prefs, &taken) else {
            app.status = FwdView::Error("no free local port".into());
            return;
        };

        app.status = FwdView::Pending;
        app.lport = Some(lport);
        let pin = app.pinned;
        let key = key.to_string();
        let wtx = self.wtx.clone();
        std::thread::spawn(move || {
            let result = mref.forward(lport, rport);
            let _ = wtx.send(WMsg::FwdDone { key, rport, lport, pin, result });
        });
    }

    fn on_fwd(&mut self, key: String, rport: u16, lport: u16, pin: bool, result: Result<(), String>) {
        let process = self
            .app_mut(&key, rport)
            .and_then(|a| a.process.clone());
        let Some(host) = self.hosts.get_mut(&key) else { return };
        let stray_cancel = |host: &HostW, lp: u16| {
            if let MState::Ready(m) = &host.master {
                let mref = m.r.clone();
                std::thread::spawn(move || {
                    let _ = mref.cancel(lp, rport);
                });
            }
        };
        let Some(app) = host.apps.get_mut(&rport) else {
            // App vanished while the forward was in flight.
            if result.is_ok() {
                stray_cancel(host, lport);
            }
            return;
        };
        if app.lport != Some(lport) {
            // A newer assignment superseded this one.
            if result.is_ok() {
                stray_cancel(host, lport);
            }
            return;
        }
        match result {
            Ok(()) => {
                app.status = FwdView::Active;
                app.retried_new_port = false;
                app.pinned = pin;
                self.state.set(
                    &key,
                    rport,
                    Assignment {
                        local_port: lport,
                        process,
                        pinned: pin,
                        updated_at: now_unix(),
                    },
                );
            }
            Err(e) => {
                let bindish = e.contains("bind") || e.contains("address") || e.contains("listen");
                if bindish && !app.retried_new_port {
                    // Local port got stolen between the check and the bind:
                    // retry once with a fresh port (it will be remembered).
                    app.retried_new_port = true;
                    app.lport = None;
                    app.status = FwdView::Off;
                    self.auto_forward_pass(&key);
                } else {
                    app.status = FwdView::Error(e);
                }
            }
        }
    }

    // ---- user actions ----

    fn assign(&mut self, key: String, rport: u16, lport: u16) {
        let taken = self.used_lports();
        let Some(host) = self.hosts.get_mut(&key) else { return };
        let MState::Ready(master) = &host.master else {
            self.toast(format!("{key}: not connected yet"));
            return;
        };
        let mref = master.r.clone();
        let Some(app) = host.apps.get_mut(&rport) else { return };

        if app.status == FwdView::External {
            self.toast(format!(
                "remote :{rport} is already forwarded by your own ssh session (-L/LocalForward) — manage it there"
            ));
            return;
        }
        if app.status == FwdView::Active && app.lport == Some(lport) {
            app.pinned = true;
            self.state.set(
                &key,
                rport,
                Assignment {
                    local_port: lport,
                    process: app.process.clone(),
                    pinned: true,
                    updated_at: now_unix(),
                },
            );
            self.toast(format!("port {lport} pinned"));
            return;
        }
        let ours = app.lport == Some(lport)
            && matches!(app.status, FwdView::Active | FwdView::Pending);
        if !ours {
            if taken.contains(&lport) {
                self.toast(format!(
                    "can't use {lport}: already forwarding another app on it"
                ));
                return;
            }
            if !ports::is_free(lport) {
                let who = ports::who_uses(lport);
                self.toast(format!("can't use {lport}: {who}"));
                return;
            }
        }

        let old = if matches!(app.status, FwdView::Active | FwdView::Pending) {
            app.lport
        } else {
            None
        };
        app.status = FwdView::Pending;
        app.lport = Some(lport);
        app.pinned = true;
        app.muted = false;
        app.retried_new_port = false;
        let wtx = self.wtx.clone();
        std::thread::spawn(move || {
            if let Some(old) = old {
                let _ = mref.cancel(old, rport);
            }
            let result = mref.forward(lport, rport);
            let _ = wtx.send(WMsg::FwdDone { key, rport, lport, pin: true, result });
        });
    }

    fn toggle(&mut self, key: String, rport: u16) {
        let Some(host) = self.hosts.get_mut(&key) else { return };
        let MState::Ready(master) = &host.master else {
            self.toast(format!("{key}: not connected yet"));
            return;
        };
        let mref = master.r.clone();
        let Some(app) = host.apps.get_mut(&rport) else { return };
        match app.status {
            FwdView::External => {
                self.toast(format!(
                    "remote :{rport} is forwarded by your own ssh session (-L/LocalForward) — close it there"
                ));
            }
            FwdView::Active | FwdView::Pending => {
                app.muted = true;
                app.status = FwdView::Off;
                if let Some(lport) = app.lport {
                    std::thread::spawn(move || {
                        let _ = mref.cancel(lport, rport);
                    });
                }
            }
            FwdView::Off | FwdView::Error(_) => {
                app.muted = false;
                app.status = FwdView::Off;
                app.retried_new_port = false;
                self.start_forward(&key, rport);
            }
        }
    }

    // ---- plumbing ----

    fn app_mut(&mut self, key: &str, rport: u16) -> Option<&mut AppW> {
        self.hosts.get_mut(key)?.apps.get_mut(&rport)
    }

    fn toast(&self, msg: String) {
        let _ = self.ev_tx.send(Ev::Toast(msg));
    }

    fn push_snapshot(&self) {
        let snap: Vec<HostView> = self
            .hosts
            .values()
            .map(|h| HostView {
                key: h.key.clone(),
                title: h.title.clone(),
                alias: if h.dest != h.title && !h.title.ends_with(&h.dest) {
                    Some(h.dest.clone())
                } else {
                    None
                },
                master: match &h.master {
                    MState::Connecting => MasterView::Connecting,
                    MState::Ready(m) => MasterView::Ready { shared: m.external },
                    MState::Failed { err, .. } => MasterView::Failed(err.clone()),
                },
                scan_err: h.scan_err.clone(),
                scanned_once: h.last_scan.is_some(),
                apps: h
                    .apps
                    .values()
                    .map(|a| AppView {
                        rport: a.rport,
                        addr: a.addr.clone(),
                        process: a.process.clone(),
                        system: a.system,
                        lport: a.lport,
                        status: a.status.clone(),
                        pinned: a.pinned,
                        muted: a.muted,
                    })
                    .collect(),
            })
            .collect();
        let _ = self.ev_tx.send(Ev::Snapshot(snap));
    }

    /// Synchronous teardown at quit — the process exits right after this, so
    /// we must not leave forwards/masters dangling in detached threads.
    fn cleanup(&mut self) {
        let hosts = std::mem::take(&mut self.hosts);
        let handles: Vec<_> = hosts.into_values().map(teardown_host).collect();
        for h in handles {
            let _ = h.join();
        }
    }
}

/// Close our own master (which drops all its forwards), or — when sharing the
/// user's master — cancel just the forwards we registered on it.
fn teardown_host(host: HostW) -> std::thread::JoinHandle<()> {
    let master = host.master;
    let forwards: Vec<(u16, u16)> = host
        .apps
        .values()
        .filter(|a| matches!(a.status, FwdView::Active | FwdView::Pending))
        .filter_map(|a| a.lport.map(|lp| (lp, a.rport)))
        .collect();
    std::thread::spawn(move || {
        if let MState::Ready(mut m) = master {
            if m.external {
                for (lp, rp) in forwards {
                    let _ = m.r.cancel(lp, rp);
                }
            } else {
                m.close();
            }
        }
    })
}
