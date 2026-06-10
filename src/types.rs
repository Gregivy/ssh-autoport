//! Shared types between the worker (SSH side) and the UI.

/// Commands sent from the UI to the worker.
#[derive(Debug)]
pub enum Cmd {
    /// Manually assign a local port to a remote app (pins it).
    Assign { host: String, rport: u16, lport: u16 },
    /// Toggle forwarding on/off for a remote app.
    Toggle { host: String, rport: u16 },
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

#[derive(Debug, Clone)]
pub struct AppView {
    pub rport: u16,
    /// Normalized remote bind address: "lo", "*", or a literal address.
    pub addr: String,
    pub process: Option<String>,
    pub system: bool,
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
    pub scan_err: Option<String>,
    pub scanned_once: bool,
    pub apps: Vec<AppView>,
}
