//! Persistent memory: per host+remote-port — local port, pinned flag,
//! on/off state, user comment; plus per-host forwarding pause.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Assignment {
    /// Last local port used (None if the app was never forwarded, e.g. the
    /// entry only carries a comment or mute flag).
    #[serde(default)]
    pub local_port: Option<u16>,
    #[serde(default)]
    pub process: Option<String>,
    /// True when the user chose this port by hand.
    #[serde(default)]
    pub pinned: bool,
    /// User turned forwarding off for this app.
    #[serde(default)]
    pub muted: bool,
    /// Free-text note attached by the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// User override of the visibility heuristic: "app" (always show and
    /// auto-forward) or "bg" (hide, never auto-forward).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default)]
    pub updated_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// "user@host:port" -> remote port -> assignment.
    #[serde(default)]
    pub assignments: BTreeMap<String, BTreeMap<u16, Assignment>>,
    /// Hosts with forwarding turned off entirely.
    #[serde(default)]
    pub paused_hosts: BTreeSet<String>,
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

    /// Get-or-create the entry; caller mutates it and then calls save().
    pub fn entry(&mut self, host: &str, rport: u16) -> &mut Assignment {
        self.assignments
            .entry(host.to_string())
            .or_default()
            .entry(rport)
            .or_default()
    }

    pub fn paused(&self, host: &str) -> bool {
        self.paused_hosts.contains(host)
    }

    pub fn set_paused(&mut self, host: &str, paused: bool) {
        if paused {
            self.paused_hosts.insert(host.to_string());
        } else {
            self.paused_hosts.remove(host);
        }
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_v1_format() {
        // The original schema stored local_port as a bare number and had no
        // muted/comment/paused_hosts — old state files must keep working.
        let old = r#"{
            "assignments": {
                "u@h:22": { "3000": { "local_port": 3000, "process": "node",
                                       "pinned": true, "updated_at": 1 } }
            }
        }"#;
        let st: State = serde_json::from_str(old).unwrap();
        let a = st.get("u@h:22", 3000).unwrap();
        assert_eq!(a.local_port, Some(3000));
        assert!(a.pinned);
        assert!(!a.muted);
        assert_eq!(a.comment, None);
        assert!(!st.paused("u@h:22"));
    }

    #[test]
    fn roundtrips_mute_comment_pause() {
        let mut st = State::default();
        st.entry("u@h:22", 9000).muted = true;
        st.entry("u@h:22", 9000).comment = Some("gpu training".into());
        st.paused_hosts.insert("u@h:22".into());
        let json = serde_json::to_string(&st).unwrap();
        let st2: State = serde_json::from_str(&json).unwrap();
        let a = st2.get("u@h:22", 9000).unwrap();
        assert!(a.muted);
        assert_eq!(a.comment.as_deref(), Some("gpu training"));
        assert_eq!(a.local_port, None); // never forwarded, entry still valid
        assert!(st2.paused("u@h:22"));
    }
}
