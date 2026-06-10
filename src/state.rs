//! Persistent port-assignment memory:
//! host identity -> remote port -> last local port (+ pinned flag).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub local_port: u16,
    #[serde(default)]
    pub process: Option<String>,
    /// True when the user chose this port by hand.
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub updated_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// "user@host:port" -> remote port -> assignment.
    #[serde(default)]
    pub assignments: BTreeMap<String, BTreeMap<u16, Assignment>>,
}

impl State {
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("SSH_AUTOPORT_STATE") {
            return PathBuf::from(p);
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                    .join(".config")
            });
        base.join("ssh-autoport").join("state.json")
    }

    pub fn load() -> State {
        fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let tmp = path.with_extension("json.tmp");
            if fs::write(&tmp, json).is_ok() {
                let _ = fs::rename(&tmp, &path);
            }
        }
    }

    pub fn get(&self, host: &str, rport: u16) -> Option<&Assignment> {
        self.assignments.get(host)?.get(&rport)
    }

    pub fn set(&mut self, host: &str, rport: u16, a: Assignment) {
        self.assignments
            .entry(host.to_string())
            .or_default()
            .insert(rport, a);
        self.save();
    }
}
