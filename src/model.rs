//! Core data types shared by the store, protocol and clients.
//!
//! Times are Unix milliseconds.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The reserved address of the human operator.
pub const HUMAN: &str = "human";

/// Longest message body, in bytes (256 KiB).
pub const MAX_BODY_BYTES: usize = 262_144;

/// Longest working-on summary, in characters.
pub const MAX_WORKING_ON_CHARS: usize = 280;

/// Default `ask` timeout, in seconds.
pub const DEFAULT_ASK_TIMEOUT_SECS: u64 = 240;

/// The coding-agent harness an agent runs in.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    ClaudeCode,
    Opencode,
    Kiro,
    Other,
}

/// A process identified by PID plus start time, so PID reuse is detected.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ProcId {
    pub pid: u32,
    pub start_time: u64,
}

/// A registered agent.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Agent {
    /// Full address, `<name>-<cuid2>`.
    pub address: String,
    pub name: String,
    pub role: Option<String>,
    /// Short summary of current work, at most [`MAX_WORKING_ON_CHARS`].
    pub working_on: Option<String>,
    pub harness: Harness,
    /// Working directory. Must be valid UTF-8: serde_json cannot serialize a
    /// non-UTF-8 path (see `model_agent_with_non_utf8_cwd_fails_to_serialize`).
    /// This type does not enforce that; the registration path is required to
    /// check `cwd` and `repo` with `to_str()` and reject a non-UTF-8 path with
    /// a clear error before storing the agent.
    pub cwd: PathBuf,
    /// Git toplevel of `cwd`, if any. Must be valid UTF-8, like `cwd`.
    pub repo: Option<PathBuf>,
    /// The harness process this address is bound to.
    pub proc_id: Option<ProcId>,
    /// Whether the agent wants to be woken for new mail.
    pub wake: bool,
    pub registered_at: i64,
    pub last_seen: i64,
    pub signed_off: bool,
}

/// Whether an agent's harness process is still alive.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Presence {
    Active,
    Gone,
}

/// What a message is.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Note,
    Question,
    Answer,
}

/// A piece of mail.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Message {
    /// ULID.
    pub id: String,
    /// Id of the first message in the thread.
    pub thread_id: String,
    pub in_reply_to: Option<String>,
    pub from: String,
    pub to: Vec<String>,
    pub subject: String,
    /// Markdown, at most [`MAX_BODY_BYTES`].
    pub body: String,
    pub kind: Kind,
    pub urgent: bool,
    pub refs: Vec<String>,
    pub created_at: i64,
    pub from_human: bool,
}

/// An open or answered blocking question.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Question {
    /// Id of the question message.
    pub id: String,
    pub thread_id: String,
    pub asker: String,
    pub to: Vec<String>,
    pub answered_by: Option<String>,
}

/// An agent as shown in listings.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AgentView {
    #[serde(flatten)]
    pub agent: Agent,
    pub presence: Presence,
    pub is_contact: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_wire<T>(value: T, wire: &str)
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        assert_eq!(json!(value), json!(wire), "{value:?}");
        let back: T = serde_json::from_value(json!(wire)).unwrap();
        assert_eq!(back, value, "{wire}");
    }

    #[test]
    fn model_enums_serialize_as_specified() {
        assert_wire(Harness::ClaudeCode, "claude-code");
        assert_wire(Harness::Opencode, "opencode");
        assert_wire(Harness::Kiro, "kiro");
        assert_wire(Harness::Other, "other");
        assert_wire(Presence::Active, "active");
        assert_wire(Presence::Gone, "gone");
        assert_wire(Kind::Note, "note");
        assert_wire(Kind::Question, "question");
        assert_wire(Kind::Answer, "answer");
    }

    #[cfg(unix)]
    #[test]
    fn model_agent_with_non_utf8_cwd_fails_to_serialize() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let agent = Agent {
            address: "backend-abcd1234".into(),
            name: "backend".into(),
            role: None,
            working_on: None,
            harness: Harness::Other,
            cwd: PathBuf::from(OsStr::from_bytes(b"/w/\xff")),
            repo: None,
            proc_id: None,
            wake: false,
            registered_at: 0,
            last_seen: 0,
            signed_off: false,
        };
        let err = serde_json::to_value(&agent).unwrap_err();
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    #[test]
    fn model_agent_view_flattens_agent() {
        let agent = Agent {
            address: "backend-abcd1234".into(),
            name: "backend".into(),
            role: None,
            working_on: Some("orders".into()),
            harness: Harness::Kiro,
            cwd: PathBuf::from("/w"),
            repo: None,
            proc_id: Some(ProcId {
                pid: 1,
                start_time: 2,
            }),
            wake: true,
            registered_at: 3,
            last_seen: 4,
            signed_off: false,
        };
        let view = AgentView {
            agent,
            presence: Presence::Gone,
            is_contact: true,
        };
        let v = serde_json::to_value(&view).unwrap();
        assert_eq!(v["address"], "backend-abcd1234");
        assert_eq!(v["presence"], "gone");
        let back: AgentView = serde_json::from_value(v).unwrap();
        assert_eq!(back, view);
    }
}
