//! The wire protocol between clients and the daemon.
//!
//! Newline-delimited JSON over a Unix socket: each line a client writes is
//! one [`Envelope`]; each line the daemon writes is one [`Response`]. A
//! connection carries one request at a time. The first request on a
//! connection must be [`Request::Hello`]; the daemon answers `Ok` when the
//! versions match and `Err` with [`ErrorCode::VersionMismatch`] otherwise
//! (and to any other request sent before a successful `Hello`).
//!
//! Compatibility invariant: the encodings of `Hello`, `Shutdown` and of the
//! `VersionMismatch` error never change, and `Shutdown` is honoured even
//! without a successful `Hello`, so a newer client can always stop an older
//! daemon and start its own.
//!
//! [`Request::Watch`] is the one streaming request: the daemon answers `Ok`
//! once it is subscribed, then writes a [`Response::Event`] line per event
//! until the client disconnects.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{AgentView, Harness, Message, ProcId};
use crate::store::ThreadFilter;

/// Bumped on every incompatible change to [`Envelope`] or [`Response`].
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest request line the daemon accepts, in bytes. Comfortably above a
/// maximal body (256 KiB) even when every byte is JSON-escaped.
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

/// Who is making a request.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Caller {
    /// The human operator (`--as human` or `AGENTMAIL_HUMAN=1`).
    Human,
    /// An explicit agent address (`AGENTMAIL_ADDRESS`, or a registered MCP
    /// session).
    Address { address: String },
    /// The calling process's ancestry, nearest first; the daemon picks the
    /// agent bound to one of them.
    Procs { procs: Vec<ProcId> },
}

/// A request. See the module docs for framing.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Hello {
        version: u32,
    },
    Register {
        name: String,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        working_on: Option<String>,
        harness: Harness,
        /// Must be absolute.
        cwd: PathBuf,
        #[serde(default)]
        proc_id: Option<ProcId>,
        #[serde(default)]
        fresh: bool,
        #[serde(default)]
        wake: bool,
    },
    Whoami,
    Status {
        working_on: String,
    },
    Signoff,
    Agents {
        #[serde(default)]
        name_prefix: Option<String>,
        #[serde(default)]
        repo: Option<PathBuf>,
        /// Include gone agents.
        #[serde(default)]
        all: bool,
    },
    ContactAdd {
        address: String,
    },
    ContactRemove {
        address: String,
    },
    Contacts,
    Send {
        to: Vec<String>,
        subject: String,
        body: String,
        #[serde(default)]
        urgent: bool,
        #[serde(default)]
        refs: Vec<String>,
    },
    Reply {
        message_id: String,
        body: String,
        #[serde(default)]
        urgent: bool,
        #[serde(default)]
        refs: Vec<String>,
        #[serde(default)]
        answer: bool,
    },
    Ask {
        to: Vec<String>,
        subject: String,
        body: String,
        #[serde(default)]
        refs: Vec<String>,
        #[serde(default)]
        urgent: bool,
        timeout_secs: u64,
    },
    AwaitAnswer {
        question_id: String,
        timeout_secs: u64,
    },
    WaitForMail {
        timeout_secs: u64,
    },
    Inbox {
        /// Include read messages.
        #[serde(default)]
        all: bool,
    },
    Counts,
    /// A message by id; marks it read for the caller. When `id` is a thread
    /// id, the whole thread is returned too and marked read.
    Read {
        id: String,
    },
    Threads {
        #[serde(default)]
        filter: ThreadFilter,
    },
    Search {
        query: String,
    },
    Prune {
        older_than_secs: u64,
    },
    /// Stream hub events, optionally only those involving `agent`.
    Watch {
        #[serde(default)]
        agent: Option<String>,
    },
    Shutdown,
}

/// One request line.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Envelope {
    pub caller: Caller,
    pub req: Request,
}

/// One response line.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok {
        data: serde_json::Value,
    },
    Err {
        code: ErrorCode,
        message: String,
        #[serde(default)]
        candidates: Vec<AgentView>,
    },
    Event {
        event: Event,
    },
}

/// Why a request failed.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    VersionMismatch,
    NoIdentity,
    InvalidName,
    InvalidInput,
    InvalidRecipient,
    UnknownRecipient,
    AmbiguousRecipient,
    Deadlock,
    TooLarge,
    NotFound,
    /// Client side only: the daemon closed the connection mid-call.
    ConnectionLost,
    Internal,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// Something that happened, pushed to waiting and watching clients.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// `message` was put in `to`'s mailbox.
    Delivered { to: String, message: Message },
    /// An agent registered, changed status or signed off.
    Presence { agent: AgentView },
}

/// A failed request, as the client sees it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ClientError {
    pub code: ErrorCode,
    pub message: String,
    /// For [`ErrorCode::AmbiguousRecipient`]: the agents to choose from.
    pub candidates: Vec<AgentView>,
}

impl ClientError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> ClientError {
        ClientError {
            code,
            message: message.into(),
            candidates: Vec::new(),
        }
    }

    /// The daemon went away during a call.
    pub fn connection_lost() -> ClientError {
        ClientError::new(ErrorCode::ConnectionLost, "daemon connection lost")
    }

    /// The error as a response line.
    pub fn into_response(self) -> Response {
        Response::Err {
            code: self.code,
            message: self.message,
            candidates: self.candidates,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ClientError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn protocol_wire_shapes() {
        let env = Envelope {
            caller: Caller::Address {
                address: "a-12345678".into(),
            },
            req: Request::Inbox { all: true },
        };
        assert_eq!(
            serde_json::to_value(&env).unwrap(),
            json!({"caller":{"kind":"address","address":"a-12345678"},"req":{"op":"inbox","all":true}})
        );
        assert_eq!(
            serde_json::to_value(Request::Whoami).unwrap(),
            json!({"op":"whoami"})
        );
        assert_eq!(
            serde_json::to_value(Request::Hello { version: 1 }).unwrap(),
            json!({"op":"hello","version":1})
        );
        assert_eq!(
            serde_json::to_value(Request::Shutdown).unwrap(),
            json!({"op":"shutdown"})
        );
        let err = ClientError::new(ErrorCode::VersionMismatch, "x").into_response();
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            json!({"status":"err","code":"VersionMismatch","message":"x","candidates":[]})
        );
        assert_eq!(
            serde_json::to_value(Response::Ok { data: json!(1) }).unwrap(),
            json!({"status":"ok","data":1})
        );
    }

    #[test]
    fn protocol_optional_fields_default() {
        let req: Request = serde_json::from_value(json!({"op":"agents"})).unwrap();
        assert_eq!(
            req,
            Request::Agents {
                name_prefix: None,
                repo: None,
                all: false
            }
        );
        let req: Request = serde_json::from_value(json!({"op":"threads"})).unwrap();
        assert_eq!(
            req,
            Request::Threads {
                filter: ThreadFilter::default()
            }
        );
    }

    #[test]
    fn protocol_error_code_displays_as_name() {
        assert_eq!(
            ErrorCode::AmbiguousRecipient.to_string(),
            "AmbiguousRecipient"
        );
    }
}
