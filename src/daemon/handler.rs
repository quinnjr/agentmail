//! Request dispatch: resolves the caller, runs the store operation and
//! publishes the resulting events.
//!
//! Every method here is synchronous and may block on the database; the
//! connection loop runs it inside `spawn_blocking`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};

use super::hub::Hub;
use crate::ids::now_ms;
use crate::model::{Agent, AgentView, HUMAN, Kind, Message};
use crate::procinfo;
use crate::protocol::{Caller, ClientError, ErrorCode, Event, Request};
use crate::store::{NewMessage, RegisterReq, Store, StoreError, lexical};

/// How stale an agent's `last_seen` may get before a request refreshes it.
pub const TOUCH_INTERVAL_MS: i64 = 1000;

/// Shared state behind every connection.
pub struct Handler {
    store: Arc<Store>,
    hub: Hub,
}

/// Map a store error to the code clients see.
pub fn store_err(e: StoreError) -> ClientError {
    let code = match &e {
        StoreError::InvalidName(_) => ErrorCode::InvalidName,
        StoreError::InvalidInput(_) | StoreError::TooLong(_) => ErrorCode::InvalidInput,
        StoreError::TooLarge => ErrorCode::TooLarge,
        StoreError::NotFound(_) => ErrorCode::NotFound,
        StoreError::UnknownRecipient(_) => ErrorCode::UnknownRecipient,
        StoreError::InvalidRecipient(_) => ErrorCode::InvalidRecipient,
        StoreError::Deadlock(_) => ErrorCode::Deadlock,
        StoreError::RecipientRegistered(_) | StoreError::Db(_) => ErrorCode::Internal,
    };
    ClientError::new(code, e.to_string())
}

fn to_json<T: Serialize>(value: &T) -> Result<Value, ClientError> {
    serde_json::to_value(value).map_err(|e| ClientError::new(ErrorCode::Internal, e.to_string()))
}

fn unimplemented(op: &str) -> ClientError {
    ClientError::new(ErrorCode::Internal, format!("unimplemented: {op}"))
}

fn human_has_no(what: &str) -> ClientError {
    ClientError::new(
        ErrorCode::InvalidInput,
        format!("{HUMAN} has no {what}; only registered agents do"),
    )
}

/// The git toplevel containing `cwd`, if any. The caller's own git
/// environment is not inherited, so only `cwd` decides.
fn git_toplevel(cwd: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_CEILING_DIRECTORIES")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut raw = out.stdout;
    while raw.last() == Some(&b'\n') || raw.last() == Some(&b'\r') {
        raw.pop();
    }
    if raw.is_empty() {
        return None;
    }
    use std::os::unix::ffi::OsStringExt;
    let path = PathBuf::from(std::ffi::OsString::from_vec(raw));
    path.is_absolute().then_some(path)
}

impl Handler {
    pub fn new(store: Arc<Store>, hub: Hub) -> Handler {
        Handler { store, hub }
    }

    pub fn hub(&self) -> &Hub {
        &self.hub
    }

    /// Who `c` is: an agent address or [`HUMAN`]. Records activity for an
    /// agent, at most once per [`TOUCH_INTERVAL_MS`]: `touch` is a durable
    /// write, and most requests (counts, inbox polls) are read-only.
    pub fn resolve_caller(&self, c: &Caller) -> Result<String, ClientError> {
        let agent = match c {
            Caller::Human => return Ok(HUMAN.to_string()),
            Caller::Address { address } => {
                let found = if address == HUMAN {
                    None
                } else {
                    self.store.get_agent(address).map_err(store_err)?
                };
                found.ok_or_else(|| {
                    ClientError::new(
                        ErrorCode::NoIdentity,
                        format!(
                            "{address:?} is not a registered address: run \
                             `agentmail register <name>` (or use --as human)"
                        ),
                    )
                })?
            }
            Caller::Procs { .. } => return Err(unimplemented("process-ancestry callers")),
        };
        let now = now_ms();
        if now.saturating_sub(agent.last_seen) >= TOUCH_INTERVAL_MS {
            // Best effort: a failed touch never fails the request.
            let _ = self.store.touch(&agent.address, now);
        }
        Ok(agent.address)
    }

    fn view(&self, agent: Agent, contacts: &HashSet<String>) -> AgentView {
        AgentView {
            presence: Store::presence(&agent, &procinfo::is_alive),
            is_contact: contacts.contains(&agent.address),
            agent,
        }
    }

    fn contact_set(&self, owner: &str) -> Result<HashSet<String>, ClientError> {
        Ok(self
            .store
            .contacts(owner)
            .map_err(store_err)?
            .into_iter()
            .collect())
    }

    fn agent_view(&self, address: &str) -> Result<AgentView, ClientError> {
        let agent = self
            .store
            .get_agent(address)
            .map_err(store_err)?
            .ok_or_else(|| {
                ClientError::new(ErrorCode::NotFound, format!("no agent {address:?}"))
            })?;
        Ok(self.view(agent, &HashSet::new()))
    }

    fn publish_presence(&self, view: &AgentView) {
        self.hub.publish(Event::Presence {
            agent: view.clone(),
        });
    }

    fn publish_delivered(&self, msg: &Message) {
        for to in &msg.to {
            self.hub.publish(Event::Delivered {
                to: to.clone(),
                message: msg.clone(),
            });
        }
    }

    /// Run one non-streaming request. `Hello`, `Shutdown` and `Watch` belong
    /// to the connection loop.
    pub fn handle(&self, caller: &Caller, req: Request) -> Result<Value, ClientError> {
        match req {
            Request::Register {
                name,
                role,
                working_on,
                harness,
                cwd,
                proc_id,
                fresh,
                wake,
            } => self.register(RegisterReq {
                name,
                role,
                working_on,
                harness,
                repo: None,
                cwd,
                proc_id,
                fresh,
                wake,
            }),
            Request::Whoami => {
                let me = self.resolve_caller(caller)?;
                if me == HUMAN {
                    return Ok(json!({ "address": HUMAN, "agent": null }));
                }
                let view = self.agent_view(&me)?;
                Ok(json!({ "address": me, "agent": to_json(&view)? }))
            }
            Request::Status { working_on } => {
                let me = self.resolve_caller(caller)?;
                if me == HUMAN {
                    return Err(human_has_no("working-on summary"));
                }
                self.store.set_status(&me, &working_on).map_err(store_err)?;
                let view = self.agent_view(&me)?;
                self.publish_presence(&view);
                to_json(&view)
            }
            Request::Signoff => {
                let me = self.resolve_caller(caller)?;
                if me == HUMAN {
                    return Err(human_has_no("session to sign off"));
                }
                self.store.signoff(&me).map_err(store_err)?;
                let view = self.agent_view(&me)?;
                self.publish_presence(&view);
                to_json(&view)
            }
            Request::Agents {
                name_prefix,
                repo,
                all,
            } => self.agents(caller, name_prefix, repo, all),
            Request::ContactAdd { address } => {
                let me = self.resolve_caller(caller)?;
                self.store.add_contact(&me, &address).map_err(store_err)?;
                Ok(Value::Null)
            }
            Request::ContactRemove { address } => {
                let me = self.resolve_caller(caller)?;
                self.store
                    .remove_contact(&me, &address)
                    .map_err(store_err)?;
                Ok(Value::Null)
            }
            Request::Contacts => {
                let me = self.resolve_caller(caller)?;
                let contacts = self.contact_set(&me)?;
                let mut views = Vec::new();
                for address in self.store.contacts(&me).map_err(store_err)? {
                    if let Some(agent) = self.store.get_agent(&address).map_err(store_err)? {
                        views.push(self.view(agent, &contacts));
                    }
                }
                to_json(&views)
            }
            Request::Send {
                to,
                subject,
                body,
                urgent,
                refs,
            } => {
                let me = self.resolve_caller(caller)?;
                let msg = self
                    .store
                    .insert_message(
                        NewMessage {
                            from: me,
                            to,
                            subject: Some(subject),
                            body,
                            kind: Kind::Note,
                            urgent,
                            refs,
                            in_reply_to: None,
                        },
                        now_ms(),
                    )
                    .map_err(store_err)?;
                self.publish_delivered(&msg);
                to_json(&msg)
            }
            Request::Inbox { all } => {
                let me = self.resolve_caller(caller)?;
                let contacts = self.contact_set(&me)?;
                let mut out = Vec::new();
                for msg in self.store.inbox(&me, all).map_err(store_err)? {
                    let from_contact = contacts.contains(&msg.from);
                    let mut v = to_json(&msg)?;
                    v["from_contact"] = Value::Bool(from_contact);
                    out.push(v);
                }
                Ok(Value::Array(out))
            }
            Request::Counts => {
                let me = self.resolve_caller(caller)?;
                let (unread, open_questions) = self.store.unread_counts(&me).map_err(store_err)?;
                Ok(json!({ "unread": unread, "open_questions": open_questions }))
            }
            Request::Read { id } => self.read(caller, &id),
            Request::Threads { filter } => {
                self.resolve_caller(caller)?;
                to_json(&self.store.threads(&filter).map_err(store_err)?)
            }
            Request::Search { query } => {
                self.resolve_caller(caller)?;
                to_json(&self.store.search(&query).map_err(store_err)?)
            }
            Request::Prune { older_than_secs } => {
                // Mail is kept unless the human prunes it.
                if self.resolve_caller(caller)? != HUMAN {
                    return Err(ClientError::new(
                        ErrorCode::InvalidInput,
                        format!("only {HUMAN} may prune mail (use --as human)"),
                    ));
                }
                let age_ms =
                    i64::try_from(older_than_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
                let before = now_ms().saturating_sub(age_ms);
                let pruned = self.store.prune(before).map_err(store_err)?;
                Ok(json!({ "pruned": pruned }))
            }
            Request::Reply { .. } => Err(unimplemented("reply")),
            Request::Ask { .. } => Err(unimplemented("ask")),
            Request::AwaitAnswer { .. } => Err(unimplemented("await_answer")),
            Request::WaitForMail { .. } => Err(unimplemented("wait_for_mail")),
            Request::Hello { .. } | Request::Watch { .. } | Request::Shutdown => Err(
                ClientError::new(ErrorCode::Internal, "handled by the connection loop"),
            ),
        }
    }

    fn register(&self, mut req: RegisterReq) -> Result<Value, ClientError> {
        if !req.cwd.is_absolute() {
            return Err(ClientError::new(
                ErrorCode::InvalidInput,
                format!("cwd {} is not an absolute path", req.cwd.display()),
            ));
        }
        req.cwd = lexical(&req.cwd);
        req.repo = git_toplevel(&req.cwd);
        // One transaction: an error never follows a committed registration,
        // so a retry cannot mint an address the caller never learned.
        let (agent, reclaimed, released) = self
            .store
            .register_and_release(req, now_ms(), &procinfo::is_alive)
            .map_err(store_err)?;
        let view = self.view(agent, &HashSet::new());
        self.publish_presence(&view);
        for msg in &released {
            self.hub.publish(Event::Delivered {
                to: view.agent.address.clone(),
                message: msg.clone(),
            });
        }
        Ok(json!({
            "address": view.agent.address,
            "reclaimed": reclaimed,
            "released": released.len(),
            "agent": to_json(&view)?,
        }))
    }

    fn agents(
        &self,
        caller: &Caller,
        name_prefix: Option<String>,
        repo: Option<PathBuf>,
        all: bool,
    ) -> Result<Value, ClientError> {
        let me = self.resolve_caller(caller)?;
        let repo = match repo {
            Some(r) if !r.is_absolute() => {
                return Err(ClientError::new(
                    ErrorCode::InvalidInput,
                    format!("repo {} is not an absolute path", r.display()),
                ));
            }
            r => r.map(|r| lexical(&r)),
        };
        let contacts = self.contact_set(&me)?;
        let views: Vec<AgentView> = self
            .store
            .list_agents()
            .map_err(store_err)?
            .into_iter()
            .filter(|a| name_prefix.as_deref().is_none_or(|p| a.name.starts_with(p)))
            .filter(|a| {
                repo.as_deref().is_none_or(|r| {
                    a.repo
                        .as_deref()
                        .is_some_and(|ar| lexical(ar).starts_with(r))
                        || lexical(&a.cwd).starts_with(r)
                })
            })
            .map(|a| self.view(a, &contacts))
            .filter(|v| all || v.presence == crate::model::Presence::Active)
            .collect();
        to_json(&views)
    }

    fn read(&self, caller: &Caller, id: &str) -> Result<Value, ClientError> {
        let me = self.resolve_caller(caller)?;
        let msg = self
            .store
            .get_message(id)
            .map_err(store_err)?
            .ok_or_else(|| ClientError::new(ErrorCode::NotFound, format!("no message {id:?}")))?;
        let mut out = to_json(&msg)?;
        if msg.thread_id == msg.id {
            let thread = self.store.thread(id).map_err(store_err)?;
            let ids: Vec<String> = thread.iter().map(|m| m.id.clone()).collect();
            self.store.mark_read(&me, &ids).map_err(store_err)?;
            out["thread"] = to_json(&thread)?;
        } else {
            self.store
                .mark_read(&me, std::slice::from_ref(&msg.id))
                .map_err(store_err)?;
        }
        Ok(out)
    }
}
