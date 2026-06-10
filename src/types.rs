//! Shared types between the worker (SSH side) and the UI.

/// Commands sent from the UI to the worker.
#[derive(Debug)]
pub enum Cmd {
    /// Manually assign a local port to a remote app (pins it).
    Assign { host: String, rport: u16, lport: u16 },
    /// Toggle forwarding on/off for a remote app.
    Toggle { host: String, rport: u16 },
    /// Attach/replace a user note on a remote app (empty text clears it).
    SetComment { host: String, rport: u16, text: String },
    /// Hide (demote to background) or unhide (promote to app) a port,
    /// overriding the heuristic. Persisted.
    ToggleHidden { host: String, rport: u16 },
    /// Pause/resume forwarding for a whole server.
    ToggleHost { host: String },
    /// Force a rescan of all hosts now.
    Refresh,
    /// Toggle global auto-forwarding.
    ToggleAuto,
    /// Shut down: tear down masters/forwards we created.
    Quit,
}

/// Events sent from the worker to the UI.
#[derive(Debug)]
pub enum Ev {
    Snapshot(Vec<HostView>),
    Toast(String),
    AutoMode(bool),
    CleanedUp,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MasterView {
    Connecting,
    /// `shared` means we piggyback on the user's own ControlMaster.
    Ready { shared: bool },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum FwdView {
    Off,
    Pending,
    Active,
    /// Already forwarded by the user's own ssh session/config (-L /
    /// LocalForward) — we show it but don't manage it.
    External,
    Error(String),
}

/// How a remote port is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// A real app: shown, auto-forwarded.
    App,
    /// Background noise (ephemeral/kernel ports): hidden, never
    /// auto-forwarded, manually forwardable.
    Bg,
    /// Infrastructure (sshd, DNS, ...): hidden, never auto-forwarded.
    System,
}

#[derive(Debug, Clone)]
pub struct AppView {
    pub rport: u16,
    /// Normalized remote bind address: "lo", "*", or a literal address.
    pub addr: String,
    pub process: Option<String>,
    pub pid: Option<u32>,
    pub cmdline: Option<String>,
    pub comment: Option<String>,
    /// Effective tier (user override already applied).
    pub tier: Tier,
    /// The user pinned this tier by hand (h key).
    pub overridden: bool,
    pub lport: Option<u16>,
    pub status: FwdView,
    pub pinned: bool,
    pub muted: bool,
}

#[derive(Debug, Clone)]
pub struct HostView {
    /// Canonical identity: user@hostname:port
    pub key: String,
    /// user@hostname for display.
    pub title: String,
    /// The destination as the user typed it (config alias etc.), if different.
    pub alias: Option<String>,
    pub master: MasterView,
    pub paused: bool,
    pub scan_err: Option<String>,
    pub scanned_once: bool,
    pub apps: Vec<AppView>,
}
