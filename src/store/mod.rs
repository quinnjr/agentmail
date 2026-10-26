//! The redb-backed store. Only the daemon opens it.
//!
//! Values are JSON (serde_json) so records can grow fields without a
//! migration. Every public method runs in its own transaction.

use std::path::Path;

use anyhow::{Context, bail};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::Serialize;
use serde::de::DeserializeOwned;

mod agents;
mod mail;
mod questions;

pub use agents::{Alive, RegisterReq, Registered};
pub use mail::{HookSnapshot, MAX_BODY, NewMessage, ThreadFilter, ThreadSummary};
pub(crate) use mail::{agent_repo_key, lexical, questions_closed_by};

/// Agents by address. Value: JSON [`crate::model::Agent`].
pub(crate) const AGENTS: TableDefinition<&str, &[u8]> = TableDefinition::new("agents");
/// Contact lists: `(owner, contact)`.
pub(crate) const CONTACTS: TableDefinition<(&str, &str), ()> = TableDefinition::new("contacts");
/// Messages by id (ULID). Value: JSON [`crate::model::Message`].
pub(crate) const MESSAGES: TableDefinition<&str, &[u8]> = TableDefinition::new("messages");
/// Per-recipient delivery state: `(address, message id) -> flags`, a set
/// of [`READ`] and [`SURFACED`]. A row with no flags is delivered, unread
/// and not yet surfaced.
///
/// Not named `mailbox`: a schema-1 build opens its bool `mailbox` table
/// before checking the schema version, and redb refuses a table opened
/// under another value type, so reusing the name would turn that build's
/// clear "schema version 2" refusal into a type-mismatch error.
pub(crate) const MAILBOX: TableDefinition<(&str, &str), u8> = TableDefinition::new("mailbox_flags");
/// [`MAILBOX`] flag: the recipient has read the message.
pub(crate) const READ: u8 = 0b01;
/// [`MAILBOX`] flag: an urgent message was pushed to the recipient mid-run
/// (see [`Store::mark_surfaced`]). Independent of [`READ`].
pub(crate) const SURFACED: u8 = 0b10;
/// The mailbox as schema 1 stored it: `(address, message id) -> read`.
/// Only the migration opens it, and deletes it.
const MAILBOX_V1: TableDefinition<(&str, &str), bool> = TableDefinition::new("mailbox");
/// Thread membership: `(thread id, message id)`.
pub(crate) const THREADS: TableDefinition<(&str, &str), ()> = TableDefinition::new("threads");
/// Mail held for a name not yet registered: `(repo, name, message id)`.
pub(crate) const HELD: TableDefinition<(&str, &str, &str), ()> = TableDefinition::new("held");
/// Questions by message id. Value: JSON [`crate::model::Question`].
pub(crate) const QUESTIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("questions");
/// Index of the open (unanswered) rows of [`QUESTIONS`]: question id.
/// Rebuilt from [`QUESTIONS`] on every [`Store::open`].
pub(crate) const OPEN_QUESTIONS: TableDefinition<&str, ()> = TableDefinition::new("open_questions");
/// Harness session bindings: `(harness, session id) -> address`. Holds
/// only bindings of non-signed-off agents (signoff removes the row).
pub(crate) const SESSIONS: TableDefinition<(&str, &str), &str> = TableDefinition::new("sessions");
/// Store metadata, e.g. `schema_version`.
pub(crate) const META: TableDefinition<&str, &str> = TableDefinition::new("meta");

const SCHEMA_VERSION_KEY: &str = "schema_version";
/// The on-disk schema this build reads and writes.
pub const SCHEMA_VERSION: &str = "2";

/// Errors from store operations.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    #[error("invalid name: {0}")]
    InvalidName(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("{0} is too long")]
    TooLong(&'static str),
    #[error("message body is too large (over 256 KiB)")]
    TooLarge,
    #[error("not found: {0}")]
    NotFound(String),
    /// A recipient address that is neither `human` nor a registered agent.
    #[error("unknown recipient: {0}")]
    UnknownRecipient(String),
    /// A recipient that may not receive this message, e.g. its own sender.
    #[error("invalid recipient: {0}")]
    InvalidRecipient(String),
    /// [`Store::insert_held`] found an active agent registered under a
    /// name it was asked to hold mail for (it registered after the caller
    /// resolved the name). Nothing was stored; resolve the recipients again
    /// and send to that agent.
    #[error("recipient registered: {0}; resolve the recipients again")]
    RecipientRegistered(String),
    /// Storing this question would close a wait cycle: the recipients
    /// already wait, directly or through others, on its asker. Nothing was
    /// stored. Carries the open question addressed to the asker that it
    /// should answer (or reply to in that thread) instead; see
    /// [`Store::would_deadlock`].
    #[error("asking would deadlock: answer question {} first", .0.id)]
    Deadlock(Box<crate::model::Question>),
    #[error("database error: {0}")]
    Db(String),
}

macro_rules! db_error_from {
    ($($t:ty),* $(,)?) => {$(
        impl From<$t> for StoreError {
            fn from(e: $t) -> Self {
                StoreError::Db(e.to_string())
            }
        }
    )*};
}

db_error_from!(
    redb::Error,
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    serde_json::Error,
);

/// Handle to the agentmail database.
pub struct Store {
    db: Database,
}

impl Store {
    /// Open (or create) the database at `path`, creating its parent
    /// directory and every table, and checking the schema version.
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let db = Database::create(path).with_context(|| format!("opening {}", path.display()))?;
        let txn = db.begin_write()?;
        {
            let mut meta = txn.open_table(META)?;
            let existing = meta.get(SCHEMA_VERSION_KEY)?.map(|v| v.value().to_string());
            match existing.as_deref() {
                None => {
                    meta.insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION)?;
                }
                Some(SCHEMA_VERSION) => {}
                Some("1") => {
                    migrate_mailbox_v1(&txn)?;
                    meta.insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION)?;
                }
                Some(v) => bail!(
                    "{} has schema version {v}, this build supports {SCHEMA_VERSION}",
                    path.display()
                ),
            }
            txn.open_table(AGENTS)?;
            txn.open_table(CONTACTS)?;
            txn.open_table(MESSAGES)?;
            txn.open_table(MAILBOX)?;
            txn.open_table(THREADS)?;
            txn.open_table(HELD)?;
            txn.open_table(QUESTIONS)?;
            txn.open_table(SESSIONS)?;
            questions::rebuild_open_index(&txn)?;
        }
        txn.commit()?;
        Ok(Store { db })
    }

    /// Start a read transaction.
    pub(crate) fn read(&self) -> Result<redb::ReadTransaction, StoreError> {
        Ok(self.db.begin_read()?)
    }
}

/// Schema 1 to 2: move every row of the old bool mailbox into [`MAILBOX`]
/// as flags (`true` becomes [`READ`], `false` no flags) and delete the old
/// table, inside the caller's write transaction, so the version bump
/// commits with it or not at all.
fn migrate_mailbox_v1(txn: &redb::WriteTransaction) -> Result<(), StoreError> {
    let mut rows = Vec::new();
    {
        let old = txn.open_table(MAILBOX_V1)?;
        for row in old.iter()? {
            let (key, read) = row?;
            let (addr, id) = key.value();
            rows.push((addr.to_string(), id.to_string(), read.value()));
        }
    }
    txn.delete_table(MAILBOX_V1)?;
    let mut mailbox = txn.open_table(MAILBOX)?;
    for (addr, id, read) in rows {
        let flags = if read { READ } else { 0 };
        mailbox.insert((addr.as_str(), id.as_str()), flags)?;
    }
    Ok(())
}

/// Encode a record as JSON.
pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, StoreError> {
    Ok(serde_json::to_vec(value)?)
}

/// Decode a JSON record.
pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, StoreError> {
    Ok(serde_json::from_slice(bytes)?)
}

/// Read and decode one JSON record from a `&str`-keyed table.
pub(crate) fn get_json<T: DeserializeOwned>(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    key: &str,
) -> Result<Option<T>, StoreError> {
    table.get(key)?.map(|v| decode(v.value())).transpose()
}

/// Decode every JSON record in a `&str`-keyed table, in key order.
pub(crate) fn all_json<T: DeserializeOwned>(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<Vec<T>, StoreError> {
    let mut out = Vec::new();
    for row in table.iter()? {
        let (_, v) = row?;
        out.push(decode(v.value())?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_open_rejects_unknown_schema_version() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("agentmail.redb");
        let store = Store::open(&path).unwrap();
        let txn = store.db.begin_write().unwrap();
        txn.open_table(META)
            .unwrap()
            .insert(SCHEMA_VERSION_KEY, "999")
            .unwrap();
        txn.commit().unwrap();
        drop(store);
        let err = Store::open(&path)
            .err()
            .expect("schema 999 must be refused");
        assert!(err.to_string().contains("schema version 999"), "{err}");
    }

    fn v1_message(id: &str, to: &str) -> crate::model::Message {
        crate::model::Message {
            id: id.into(),
            thread_id: id.into(),
            in_reply_to: None,
            from_human: true,
            from: crate::model::HUMAN.into(),
            to: vec![to.into()],
            subject: format!("subject {id}"),
            body: "body".into(),
            kind: crate::model::Kind::Note,
            urgent: true,
            refs: Vec::new(),
            created_at: 1,
        }
    }

    #[test]
    fn migrates_schema_1_mailbox() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("agentmail.redb");
        let addr = "backend-aaaaaaaa";
        {
            // A schema-1 database as the core build wrote it: no sessions
            // table, a bool mailbox.
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut messages = txn.open_table(MESSAGES).unwrap();
                let mut threads = txn.open_table(THREADS).unwrap();
                let mut mailbox = txn.open_table(MAILBOX_V1).unwrap();
                for (id, read) in [("01AREAD", true), ("01BUNREAD", false)] {
                    let m = v1_message(id, addr);
                    messages.insert(id, encode(&m).unwrap().as_slice()).unwrap();
                    threads.insert((id, id), ()).unwrap();
                    mailbox.insert((addr, id), read).unwrap();
                }
                txn.open_table(META)
                    .unwrap()
                    .insert(SCHEMA_VERSION_KEY, "1")
                    .unwrap();
            }
            txn.commit().unwrap();
        }

        let store = Store::open(&path).unwrap();
        {
            let txn = store.read().unwrap();
            let meta = txn.open_table(META).unwrap();
            assert_eq!(meta.get(SCHEMA_VERSION_KEY).unwrap().unwrap().value(), "2");
            let mailbox = txn.open_table(MAILBOX).unwrap();
            assert_eq!(
                mailbox.get((addr, "01AREAD")).unwrap().unwrap().value(),
                READ
            );
            assert_eq!(
                mailbox.get((addr, "01BUNREAD")).unwrap().unwrap().value(),
                0
            );
        }
        let ids = |msgs: Vec<crate::model::Message>| -> Vec<String> {
            msgs.into_iter().map(|m| m.id).collect()
        };
        assert_eq!(ids(store.inbox(addr, false).unwrap()), vec!["01BUNREAD"]);
        assert_eq!(
            ids(store.inbox(addr, true).unwrap()),
            vec!["01AREAD", "01BUNREAD"]
        );
        // Read state survives; surfaced starts clear.
        assert_eq!(
            ids(store.unsurfaced_urgent(addr).unwrap()),
            vec!["01BUNREAD"]
        );

        // The old table is gone.
        {
            let txn = store.read().unwrap();
            let names: Vec<String> = txn
                .list_tables()
                .unwrap()
                .map(|t| redb::TableHandle::name(&t).to_string())
                .collect();
            assert!(!names.iter().any(|n| n == "mailbox"), "{names:?}");
        }
        drop(store);

        // Opening the migrated database again changes nothing.
        let store = Store::open(&path).unwrap();
        assert_eq!(ids(store.inbox(addr, false).unwrap()), vec!["01BUNREAD"]);
        assert_eq!(
            ids(store.unsurfaced_urgent(addr).unwrap()),
            vec!["01BUNREAD"]
        );
    }

    /// A schema-1 build opens every table, its bool `mailbox` among them,
    /// before checking the version. On a schema-2 database that must
    /// succeed, so the build gets to its clear "schema version 2" refusal
    /// instead of failing on a table type mismatch.
    #[test]
    fn schema_1_build_reaches_its_version_check_on_schema_2() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("agentmail.redb");
        let store = Store::open(&path).unwrap();
        let txn = store.db.begin_write().unwrap();
        {
            txn.open_table(MAILBOX_V1)
                .expect("a schema-1 build must be able to open its mailbox");
            let meta = txn.open_table(META).unwrap();
            assert_eq!(meta.get(SCHEMA_VERSION_KEY).unwrap().unwrap().value(), "2");
        }
        // The old build bails here, so its transaction never commits.
        txn.abort().unwrap();
    }

    #[test]
    fn store_open_records_schema_version() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(&dir.path().join("agentmail.redb")).unwrap();
        let txn = store.read().unwrap();
        let meta = txn.open_table(META).unwrap();
        assert_eq!(
            meta.get(SCHEMA_VERSION_KEY).unwrap().unwrap().value(),
            SCHEMA_VERSION
        );
    }
}
