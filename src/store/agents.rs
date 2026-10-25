//! Agents: registration and reclaim, presence, status, signoff and contacts.

use std::path::{Path, PathBuf};

use redb::{ReadableTable, WriteTransaction};

use super::{AGENTS, CONTACTS, Store, StoreError, all_json, encode, get_json};
use crate::ids::{new_address, validate_name};
use crate::model::{Agent, HUMAN, Harness, MAX_WORKING_ON_CHARS, Presence, ProcId};

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
    /// Always take a new address; never reclaim.
    pub fresh: bool,
    pub wake: bool,
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

/// Whether `agent` may be rebound to a registration from `proc_id`.
///
/// An agent bound to a live process is only rebound by that same process (a
/// repeat registration). Anything else bound to a live process, active or
/// signed off, is never taken.
fn reclaimable(agent: &Agent, proc_id: Option<ProcId>, alive: Alive) -> bool {
    match agent.proc_id {
        None => true,
        Some(bound) => Some(bound) == proc_id || !alive(&bound),
    }
}

/// Whether `agent` is already bound to the requesting process `proc_id`.
fn is_own(agent: &Agent, proc_id: Option<ProcId>) -> bool {
    agent.proc_id.is_some() && agent.proc_id == proc_id
}

impl Store {
    /// Register an agent, reclaiming a gone agent's address when the spec's
    /// reclaim rule applies. Returns the agent and whether it was reclaimed.
    ///
    /// Reclaim: unless `fresh`, an agent with the same `name` and `cwd` that
    /// is not bound to a live process (or is bound to the requesting process
    /// itself) is rebound. With several, one already bound to the requesting
    /// process wins (a re-registration keeps the caller's current address);
    /// otherwise the most recently seen wins. The
    /// address, `registered_at` and contacts are kept; `role`, `working_on`,
    /// `harness`, `repo`, `wake`, `proc_id` and `last_seen` are replaced and
    /// `signed_off` is cleared.
    ///
    /// The returned flag is true only when the address was taken back from a
    /// different or absent process; a re-registration that keeps the address
    /// already bound to `proc_id` is not a reclaim and reports false.
    ///
    /// One address per harness process: any other agent bound to `proc_id`
    /// is unbound (its `proc_id` cleared, so it shows as gone).
    pub fn register(
        &self,
        req: RegisterReq,
        now: i64,
        alive: Alive,
    ) -> Result<(Agent, bool), StoreError> {
        let txn = self.db.begin_write()?;
        let result = self.register_in(&txn, req, now, alive)?;
        txn.commit()?;
        Ok(result)
    }

    /// [`Store::register`] inside the caller's write transaction, which the
    /// caller commits.
    pub(super) fn register_in(
        &self,
        txn: &WriteTransaction,
        req: RegisterReq,
        now: i64,
        alive: Alive,
    ) -> Result<(Agent, bool), StoreError> {
        validate_name(&req.name).map_err(StoreError::InvalidName)?;
        check_utf8("cwd", &req.cwd)?;
        if let Some(repo) = &req.repo {
            check_utf8("repo", repo)?;
        }
        if let Some(w) = &req.working_on {
            check_working_on(w)?;
        }

        let mut table = txn.open_table(AGENTS)?;
        let agents: Vec<Agent> = all_json(&table)?;
        let reclaim = if req.fresh {
            None
        } else {
            agents
                .iter()
                .filter(|a| a.name == req.name && a.cwd == req.cwd)
                .filter(|a| reclaimable(a, req.proc_id, alive))
                .max_by(|x, y| {
                    let key = |a: &Agent| (is_own(a, req.proc_id), a.last_seen, a.registered_at);
                    key(x).cmp(&key(y)).then_with(|| x.address.cmp(&y.address))
                })
                .cloned()
        };

        let reclaimed = reclaim.as_ref().is_some_and(|a| !is_own(a, req.proc_id));
        let agent = match reclaim {
            Some(mut a) => {
                a.role = req.role;
                a.working_on = req.working_on;
                a.harness = req.harness;
                a.repo = req.repo;
                a.proc_id = req.proc_id;
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
                    wake: req.wake,
                    registered_at: now,
                    last_seen: now,
                    signed_off: false,
                }
            }
        };

        if let Some(bound) = agent.proc_id {
            for mut other in agents {
                if other.address != agent.address && other.proc_id == Some(bound) {
                    other.proc_id = None;
                    table.insert(other.address.as_str(), encode(&other)?.as_slice())?;
                }
            }
        }
        table.insert(agent.address.as_str(), encode(&agent)?.as_slice())?;
        Ok((agent, reclaimed))
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

    /// The agent bound to the first of `procs` (in the given order, nearest
    /// ancestor first) that has a non-signed-off agent. Matches on PID and
    /// start time, so a reused PID never matches.
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
            let found = agents
                .iter()
                .filter(|a| a.proc_id == Some(*p))
                .max_by_key(|a| (a.last_seen, a.registered_at));
            if let Some(a) = found {
                return Ok(Some(a.clone()));
            }
        }
        Ok(None)
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

    /// Mark the agent gone. Its mail and contacts stay.
    pub fn signoff(&self, addr: &str) -> Result<(), StoreError> {
        self.update_agent(addr, |a| {
            a.signed_off = true;
            Ok(())
        })
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
