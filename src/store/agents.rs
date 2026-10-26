//! Agents: registration and reclaim, presence, status, signoff and contacts.

use std::path::{Path, PathBuf};

use redb::{ReadableTable, WriteTransaction};

use super::{AGENTS, CONTACTS, SESSIONS, Store, StoreError, all_json, encode, get_json};
use crate::ids::{new_address, validate_name};
use crate::model::{Agent, HUMAN, Harness, MAX_WORKING_ON_CHARS, Presence, ProcId, SessionKey};

/// How many fresh addresses to try before giving up on a collision streak.
const ADDRESS_ATTEMPTS: usize = 16;

/// Liveness check for a harness process (`procinfo::is_alive` in production).
pub type Alive<'a> = &'a dyn Fn(&ProcId) -> bool;

/// A registration request.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisterReq {
    pub name: String,
    pub role: Option<String>,
    pub working_on: Option<String>,
    pub harness: Harness,
    /// Must be valid UTF-8.
    pub cwd: PathBuf,
    /// Must be valid UTF-8.
    pub repo: Option<PathBuf>,
    /// The harness process to bind the address to.
    pub proc_id: Option<ProcId>,
    /// Always take a new address; never reclaim. `InvalidInput` when
    /// `session` is already bound to a non-signed-off agent.
    pub fresh: bool,
    pub wake: bool,
    /// The harness session to bind the address to, if any.
    pub session: Option<SessionKey>,
}

/// The outcome of a registration.
#[derive(Debug, Clone, PartialEq)]
pub struct Registered {
    pub agent: Agent,
    /// The address was taken back from a different or absent binding.
    pub reclaimed: bool,
    /// A repeat registration of a live identity: the requesting session, or
    /// the requesting process as its own identity (`is_own`), was already
    /// bound to this agent, which was returned (updated).
    pub existing: bool,
}

/// The [`SESSIONS`] key of `k`.
fn session_key(k: &SessionKey) -> (&'static str, &str) {
    (k.harness.as_str(), k.id.as_str())
}

fn check_utf8(field: &str, path: &Path) -> Result<(), StoreError> {
    match path.to_str() {
        Some(_) => Ok(()),
        None => Err(StoreError::InvalidInput(format!(
            "{field} {} is not valid UTF-8",
            path.display()
        ))),
    }
}

fn check_working_on(working_on: &str) -> Result<(), StoreError> {
    if working_on.chars().count() > MAX_WORKING_ON_CHARS {
        return Err(StoreError::TooLong("working-on summary"));
    }
    Ok(())
}

fn unknown(addr: &str) -> StoreError {
    StoreError::NotFound(format!("no agent with address {addr:?}"))
}

/// Whether `agent` belongs to a session of a harness that runs several
/// sessions in one process (opencode). Such an agent is identified by its
/// session alone: another registration on its process never unbinds or
/// adopts it, and process ancestry never resolves to it.
fn shared_session(agent: &Agent) -> bool {
    agent
        .session
        .as_ref()
        .is_some_and(|k| k.harness.shares_process())
}

/// Whether `agent` may be rebound to `req`.
///
/// The requesting process's own agent always may (a repeat registration,
/// see [`is_own`]). A non-signed-off agent of a shared session
/// ([`shared_session`]) other than `req.session` never may, even when its
/// process is dead: it belongs to that session until it signs off or the
/// session registers again (an opencode session outlives a restart of the
/// service process). A session running in its own process (Claude Code,
/// kiro) ends with that process, so its agent follows the process rule:
/// an agent bound to a live process, active or signed off, is never taken.
fn reclaimable(agent: &Agent, req: &RegisterReq, alive: Alive) -> bool {
    if is_own(agent, req) {
        return true;
    }
    if !agent.signed_off && shared_session(agent) && agent.session != req.session {
        return false;
    }
    match agent.proc_id {
        None => true,
        Some(bound) => !alive(&bound),
    }
}

/// Whether `agent` is already the requesting process's own identity.
///
/// It must be bound to `req.proc_id`, with the same session or, when the
/// sessions run in their own process (every harness but opencode), with
/// any session on either side: kiro's MCP server registers without a
/// session and its CLI with `KIRO_SESSION_ID`, from the same kiro-cli
/// process, and that process may move on to a new session (the agent is
/// then rebound to it and the old session unbound).
fn is_own(agent: &Agent, req: &RegisterReq) -> bool {
    if agent.proc_id.is_none() || agent.proc_id != req.proc_id {
        return false;
    }
    let own_process =
        |k: &Option<SessionKey>| k.as_ref().is_none_or(|k| !k.harness.shares_process());
    agent.session == req.session || (own_process(&agent.session) && own_process(&req.session))
}

/// Remove the binding of session `k` if it points at `addr`.
fn unbind_session(
    sessions: &mut redb::Table<(&'static str, &'static str), &'static str>,
    k: &SessionKey,
    addr: &str,
) -> Result<(), StoreError> {
    let points_here = sessions
        .get(session_key(k))?
        .is_some_and(|v| v.value() == addr);
    if points_here {
        sessions.remove(session_key(k))?;
    }
    Ok(())
}

/// Bind session `k` to `addr` unless its row names another agent still
/// bound to `k` (not signed off, `session == Some(k)`): a session pair names
/// at most one non-signed-off agent. A row left pointing at an agent that
/// no longer holds `k` is stale and is overwritten. Returns whether `k` is
/// now bound to `addr`.
fn bind_session(
    table: &redb::Table<&'static str, &'static [u8]>,
    sessions: &mut redb::Table<(&'static str, &'static str), &'static str>,
    k: &SessionKey,
    addr: &str,
) -> Result<bool, StoreError> {
    let holder = sessions.get(session_key(k))?.map(|v| v.value().to_string());
    let free = match holder {
        None => true,
        Some(h) if h == addr => true,
        Some(h) => {
            let held: Option<Agent> = get_json(table, &h)?;
            !held.is_some_and(|o| !o.signed_off && o.session.as_ref() == Some(k))
        }
    };
    if free {
        sessions.insert(session_key(k), addr)?;
    }
    Ok(free)
}

/// One process-level address per process: unbind (clear `proc_id` of)
/// every other agent bound to `agent`'s process, except agents of shared
/// sessions ([`shared_session`]), which belong to their session.
///
/// A session-less `agent` also takes over the session of the most recently
/// seen non-signed-off agent it displaces: that session runs in this very
/// process (kiro), so it now names `agent` too. Every other displaced agent
/// loses its session binding: the session ran in this process, which no
/// longer names it. Writes the displaced agents; the caller writes `agent`.
fn claim_process(
    table: &mut redb::Table<&'static str, &'static [u8]>,
    sessions: &mut redb::Table<(&'static str, &'static str), &'static str>,
    agents: &[Agent],
    agent: &mut Agent,
) -> Result<(), StoreError> {
    // A session agent of a shared process (opencode) is not process-level:
    // it claims nothing, so process-level agents there keep their binding.
    if shared_session(agent) {
        return Ok(());
    }
    let Some(bound) = agent.proc_id else {
        return Ok(());
    };
    let mut displaced: Vec<Agent> = agents
        .iter()
        .filter(|o| o.address != agent.address && o.proc_id == Some(bound) && !shared_session(o))
        .cloned()
        .collect();
    displaced.sort_by_key(|o| (o.last_seen, o.registered_at));
    let heir = if agent.session.is_none() {
        displaced.iter().rposition(|o| {
            // Only a session of the claiming agent's own harness: a
            // `--harness` override must not inherit another harness's.
            !o.signed_off
                && o.session
                    .as_ref()
                    .is_some_and(|k| k.harness == agent.harness)
        })
    } else {
        None
    };
    for (i, mut other) in displaced.into_iter().enumerate() {
        other.proc_id = None;
        let session = other.session.take();
        // Written before the heir binding so `other` no longer holds it.
        table.insert(other.address.as_str(), encode(&other)?.as_slice())?;
        if let Some(k) = session {
            if Some(i) == heir {
                if bind_session(table, sessions, &k, &agent.address)? {
                    agent.session = Some(k);
                }
            } else {
                unbind_session(sessions, &k, &other.address)?;
            }
        }
    }
    Ok(())
}

impl Store {
    /// Register an agent, reclaiming a gone agent's address when the spec's
    /// reclaim rule applies. Returns the agent and whether it was reclaimed.
    ///
    /// Reclaim: unless `fresh`, an agent with the same `name` and `cwd` that
    /// [`reclaimable`] allows is rebound: one not bound to a live process,
    /// or the requesting process's own ([`is_own`]), and never one still
    /// bound to another opencode session. With several, the requesting process's own
    /// wins (a re-registration keeps the caller's current address);
    /// otherwise the most recently seen wins. The address, `registered_at`
    /// and contacts are kept; `role`, `working_on`, `harness`, `repo`,
    /// `wake`, `proc_id`, `session` and `last_seen` are replaced and
    /// `signed_off` is cleared. A session-less re-registration of an agent
    /// bound to its process's own session keeps that session, unless the
    /// agent signed off (signoff freed it). A session is bound only when no
    /// other non-signed-off agent holds it; otherwise the result has none.
    ///
    /// The returned flag is true only when the address was taken back from a
    /// different or absent process; a re-registration that keeps the address
    /// already bound to `proc_id` is not a reclaim and reports false.
    ///
    /// One process-level address per harness process: any other agent bound
    /// to `proc_id` is unbound (its `proc_id` cleared, so it shows as gone),
    /// except agents of other opencode sessions sharing the process. An
    /// opencode session agent is not process-level and unbinds nothing.
    /// A session-less claimant inherits a displaced agent's session only
    /// when it is of the claimant's own harness. See `claim_process`.
    ///
    /// Sessions: with `session = Some(k)` and a non-signed-off agent bound
    /// to `k`, that agent is returned with `role`, `working_on`, `wake`,
    /// `proc_id` (kept when `req.proc_id` is `None`) and `last_seen` updated
    /// (not reclaimed, `existing`); its name must match `req.name` and
    /// `req.fresh` must be false (a bound session cannot take a new
    /// address until it signs off), otherwise `InvalidInput`. Otherwise the rules above apply and the
    /// result is bound to `k`, moving any earlier session binding of a
    /// reclaimed agent off it.
    pub fn register(
        &self,
        req: RegisterReq,
        now: i64,
        alive: Alive,
    ) -> Result<(Agent, bool), StoreError> {
        let txn = self.db.begin_write()?;
        let r = self.register_in(&txn, req, now, alive)?;
        txn.commit()?;
        Ok((r.agent, r.reclaimed))
    }

    /// [`Store::register`] inside the caller's write transaction, which the
    /// caller commits.
    pub(super) fn register_in(
        &self,
        txn: &WriteTransaction,
        req: RegisterReq,
        now: i64,
        alive: Alive,
    ) -> Result<Registered, StoreError> {
        validate_name(&req.name).map_err(StoreError::InvalidName)?;
        if let Some(k) = &req.session {
            k.validate().map_err(StoreError::InvalidInput)?;
            if k.harness != req.harness {
                return Err(StoreError::InvalidInput(format!(
                    "session harness {} does not match registration harness {}",
                    k.harness.as_str(),
                    req.harness.as_str()
                )));
            }
        }
        check_utf8("cwd", &req.cwd)?;
        if let Some(repo) = &req.repo {
            check_utf8("repo", repo)?;
        }
        if let Some(w) = &req.working_on {
            check_working_on(w)?;
        }

        let mut table = txn.open_table(AGENTS)?;
        let mut sessions = txn.open_table(SESSIONS)?;
        let agents: Vec<Agent> = all_json(&table)?;
        if let Some(k) = &req.session {
            let bound = sessions.get(session_key(k))?.map(|v| v.value().to_string());
            let bound: Option<Agent> = match bound {
                Some(addr) => get_json(&table, &addr)?,
                None => None,
            };
            if let Some(mut a) = bound.filter(|a| !a.signed_off && a.session.as_ref() == Some(k)) {
                if a.name != req.name {
                    return Err(StoreError::InvalidInput(format!(
                        "session already registered as {}",
                        a.address
                    )));
                }
                if req.fresh {
                    return Err(StoreError::InvalidInput(format!(
                        "session already registered as {}; sign off first",
                        a.address
                    )));
                }
                a.role = req.role;
                a.working_on = req.working_on;
                a.wake = req.wake;
                if req.proc_id.is_some() {
                    a.proc_id = req.proc_id;
                }
                a.last_seen = now;
                claim_process(&mut table, &mut sessions, &agents, &mut a)?;
                table.insert(a.address.as_str(), encode(&a)?.as_slice())?;
                return Ok(Registered {
                    agent: a,
                    reclaimed: false,
                    existing: true,
                });
            }
        }
        let reclaim = if req.fresh {
            None
        } else {
            agents
                .iter()
                .filter(|a| a.name == req.name && a.cwd == req.cwd)
                .filter(|a| reclaimable(a, &req, alive))
                .max_by(|x, y| {
                    let key = |a: &Agent| (is_own(a, &req), a.last_seen, a.registered_at);
                    key(x).cmp(&key(y)).then_with(|| x.address.cmp(&y.address))
                })
                .cloned()
        };

        let own = reclaim.as_ref().is_some_and(|a| is_own(a, &req));
        let reclaimed = reclaim.is_some() && !own;
        // A repeat registration of the requesting process's live identity:
        // the process-binding counterpart of a session's `existing`.
        let existing = own && reclaim.as_ref().is_some_and(|a| !a.signed_off);
        let mut old_session = None;
        let mut agent = match reclaim {
            Some(mut a) => {
                let was_signed_off = a.signed_off;
                old_session = a.session.take();
                a.role = req.role;
                a.working_on = req.working_on;
                a.harness = req.harness;
                a.repo = req.repo;
                a.proc_id = req.proc_id;
                // The process's own session stays bound when the process
                // registers again without naming it, unless the agent signed
                // off: signoff freed that session, which another agent may
                // have bound since.
                a.session = match req.session {
                    None if own && !was_signed_off => old_session.clone(),
                    s => s,
                };
                a.wake = req.wake;
                a.last_seen = now;
                a.signed_off = false;
                a
            }
            None => {
                let mut address = None;
                for _ in 0..ADDRESS_ATTEMPTS {
                    let candidate = new_address(&req.name);
                    if table.get(candidate.as_str())?.is_none() {
                        address = Some(candidate);
                        break;
                    }
                }
                let address = address.ok_or_else(|| {
                    StoreError::Db(format!("could not find a free address for {:?}", req.name))
                })?;
                Agent {
                    address,
                    name: req.name,
                    role: req.role,
                    working_on: req.working_on,
                    harness: req.harness,
                    cwd: req.cwd,
                    repo: req.repo,
                    proc_id: req.proc_id,
                    session: req.session,
                    wake: req.wake,
                    registered_at: now,
                    last_seen: now,
                    signed_off: false,
                }
            }
        };

        if let Some(old) = old_session.filter(|old| Some(old) != agent.session.as_ref()) {
            unbind_session(&mut sessions, &old, &agent.address)?;
        }
        claim_process(&mut table, &mut sessions, &agents, &mut agent)?;
        if let Some(k) = agent.session.take()
            && bind_session(&table, &mut sessions, &k, &agent.address)?
        {
            agent.session = Some(k);
        }
        table.insert(agent.address.as_str(), encode(&agent)?.as_slice())?;
        Ok(Registered {
            agent,
            reclaimed,
            existing,
        })
    }

    /// Active while bound to a live process and not signed off; Gone otherwise.
    pub fn presence(agent: &Agent, alive: Alive) -> Presence {
        match agent.proc_id {
            Some(p) if !agent.signed_off && alive(&p) => Presence::Active,
            _ => Presence::Gone,
        }
    }

    pub fn get_agent(&self, addr: &str) -> Result<Option<Agent>, StoreError> {
        let txn = self.read()?;
        let table = txn.open_table(AGENTS)?;
        get_json(&table, addr)
    }

    /// Every agent, active or gone, in address order.
    pub fn list_agents(&self) -> Result<Vec<Agent>, StoreError> {
        let txn = self.read()?;
        let table = txn.open_table(AGENTS)?;
        all_json(&table)
    }

    /// The agent identified by the nearest of `procs` (in the given order,
    /// nearest ancestor first) that has a non-signed-off agent bound to it.
    /// Matches on PID and start time, so a reused PID never matches.
    ///
    /// Agents of shared sessions (opencode) are never resolved this way:
    /// every session of the process has the same ancestry. The nearest
    /// process with any non-signed-off agent decides: its one process-level
    /// agent, or `None` when it has none (only session agents) or, against
    /// the one-address-per-process rule, several.
    pub fn find_by_procs(&self, procs: &[ProcId]) -> Result<Option<Agent>, StoreError> {
        if procs.is_empty() {
            return Ok(None);
        }
        let agents: Vec<Agent> = self
            .list_agents()?
            .into_iter()
            .filter(|a| !a.signed_off && a.proc_id.is_some())
            .collect();
        for p in procs {
            let here: Vec<&Agent> = agents.iter().filter(|a| a.proc_id == Some(*p)).collect();
            if here.is_empty() {
                continue;
            }
            let mut own = here.into_iter().filter(|a| !shared_session(a));
            return Ok(match (own.next(), own.next()) {
                (Some(a), None) => Some(a.clone()),
                _ => None,
            });
        }
        Ok(None)
    }

    /// The non-signed-off agent bound to the harness session `key`.
    pub fn find_by_session(&self, key: &SessionKey) -> Result<Option<Agent>, StoreError> {
        let txn = self.read()?;
        let sessions = txn.open_table(SESSIONS)?;
        let Some(addr) = sessions
            .get(session_key(key))?
            .map(|v| v.value().to_string())
        else {
            return Ok(None);
        };
        let agents = txn.open_table(AGENTS)?;
        let agent: Option<Agent> = get_json(&agents, &addr)?;
        Ok(agent.filter(|a| !a.signed_off && a.session.as_ref() == Some(key)))
    }

    /// Read-modify-write one agent in its own transaction.
    fn update_agent(
        &self,
        addr: &str,
        f: impl FnOnce(&mut Agent) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(AGENTS)?;
            let mut agent: Agent = get_json(&table, addr)?.ok_or_else(|| unknown(addr))?;
            f(&mut agent)?;
            table.insert(addr, encode(&agent)?.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Record activity at `now`.
    pub fn touch(&self, addr: &str, now: i64) -> Result<(), StoreError> {
        self.update_agent(addr, |a| {
            a.last_seen = now;
            Ok(())
        })
    }

    /// Set the working-on summary (at most 280 characters).
    pub fn set_status(&self, addr: &str, working_on: &str) -> Result<(), StoreError> {
        check_working_on(working_on)?;
        self.update_agent(addr, |a| {
            a.working_on = Some(working_on.to_string());
            Ok(())
        })
    }

    /// Mark the agent gone and free its session binding. Its mail and
    /// contacts stay.
    pub fn signoff(&self, addr: &str) -> Result<(), StoreError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(AGENTS)?;
            let mut agent: Agent = get_json(&table, addr)?.ok_or_else(|| unknown(addr))?;
            agent.signed_off = true;
            if let Some(k) = &agent.session {
                let mut sessions = txn.open_table(SESSIONS)?;
                unbind_session(&mut sessions, k, addr)?;
            }
            table.insert(addr, encode(&agent)?.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Add `contact` to `owner`'s contacts. `owner` is an agent or `human`;
    /// `contact` must be the full address of an existing agent other than
    /// `owner`. Adding an existing contact is a no-op.
    pub fn add_contact(&self, owner: &str, contact: &str) -> Result<(), StoreError> {
        if owner == contact {
            return Err(StoreError::InvalidInput(
                "an agent cannot add itself as a contact".into(),
            ));
        }
        let txn = self.db.begin_write()?;
        {
            let agents = txn.open_table(AGENTS)?;
            if owner != HUMAN && agents.get(owner)?.is_none() {
                return Err(unknown(owner));
            }
            if agents.get(contact)?.is_none() {
                return Err(StoreError::NotFound(format!(
                    "no agent with address {contact:?}; a contact must be a full \
                     <name>-<id> address from `agents`"
                )));
            }
            let mut contacts = txn.open_table(CONTACTS)?;
            contacts.insert((owner, contact), ())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Remove `contact` from `owner`'s contacts; `NotFound` if it wasn't one.
    pub fn remove_contact(&self, owner: &str, contact: &str) -> Result<(), StoreError> {
        let txn = self.db.begin_write()?;
        {
            let mut contacts = txn.open_table(CONTACTS)?;
            if contacts.remove((owner, contact))?.is_none() {
                return Err(StoreError::NotFound(format!(
                    "{contact:?} is not a contact of {owner:?}"
                )));
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// `owner`'s contacts, sorted by address.
    pub fn contacts(&self, owner: &str) -> Result<Vec<String>, StoreError> {
        let txn = self.read()?;
        let table = txn.open_table(CONTACTS)?;
        let mut out = Vec::new();
        for row in table.range((owner, "")..)? {
            let (key, _) = row?;
            let (o, c) = key.value();
            if o != owner {
                break;
            }
            out.push(c.to_string());
        }
        Ok(out)
    }
}
