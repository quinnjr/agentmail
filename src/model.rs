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
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    ClaudeCode,
    Opencode,
    Kiro,
    Other,
}

impl Harness {
    /// The wire name, e.g. `claude-code`.
    pub fn as_str(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Opencode => "opencode",
            Harness::Kiro => "kiro",
            Harness::Other => "other",
        }
    }

    /// Whether this harness runs several sessions in one process (opencode's
    /// shared service). An agent bound to such a session belongs to that
    /// session alone: process ancestry can never tell it apart.
    pub fn shares_process(self) -> bool {
        matches!(self, Harness::Opencode)
    }
}

/// Longest harness session id, in characters.
pub const MAX_SESSION_ID_CHARS: usize = 128;

/// A harness session: the harness plus its own session id. Some harnesses
/// (opencode) run every session in one shared process, so an agent can be
/// bound to its session as well as to a process.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub struct SessionKey {
    pub harness: Harness,
    /// 1 to [`MAX_SESSION_ID_CHARS`] characters, no control characters.
    pub id: String,
}

impl SessionKey {
    /// Check the id's length and characters.
    pub fn validate(&self) -> Result<(), String> {
        let n = self.id.chars().count();
        if n == 0 || n > MAX_SESSION_ID_CHARS {
            return Err(format!(
                "session id must be 1 to {MAX_SESSION_ID_CHARS} characters, got {n}"
            ));
        }
        if self.id.chars().any(char::is_control) {
            return Err(format!("session id {:?} has control characters", self.id));
        }
        Ok(())
    }
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
    /// The harness session this address is bound to, if any. The store's
    /// session index is authoritative: after signoff the field may still
    /// name the last session, but it no longer resolves.
    #[serde(default)]
    pub session: Option<SessionKey>,
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
        for h in [
            Harness::ClaudeCode,
            Harness::Opencode,
            Harness::Kiro,
            Harness::Other,
        ] {
            assert_eq!(json!(h), json!(h.as_str()));
        }
    }

    #[test]
    fn model_session_key_validation() {
        let k = |id: &str| SessionKey {
            harness: Harness::Opencode,
            id: id.into(),
        };
        assert!(k("ses_abc").validate().is_ok());
        assert!(k(&"é".repeat(128)).validate().is_ok());
        assert!(k("").validate().is_err());
        assert!(k(&"x".repeat(129)).validate().is_err());
        assert!(k("a\tb").validate().is_err());
        assert!(k("a\u{7f}").validate().is_err());
    }

    #[test]
    fn model_agent_without_session_field_decodes() {
        let v = json!({
            "address": "a-abcd1234", "name": "a", "role": null, "working_on": null,
            "harness": "other", "cwd": "/w", "repo": null, "proc_id": null,
            "wake": false, "registered_at": 0, "last_seen": 0, "signed_off": false
        });
        let a: Agent = serde_json::from_value(v).unwrap();
        assert_eq!(a.session, None);
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
            session: None,
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
            session: Some(SessionKey {
                harness: Harness::Opencode,
                id: "ses_1".into(),
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
