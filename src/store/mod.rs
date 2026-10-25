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

pub use agents::{Alive, RegisterReq};
pub use mail::{MAX_BODY, NewMessage, ThreadFilter, ThreadSummary};
pub(crate) use mail::{agent_repo_key, lexical, questions_closed_by};

/// Agents by address. Value: JSON [`crate::model::Agent`].
pub(crate) const AGENTS: TableDefinition<&str, &[u8]> = TableDefinition::new("agents");
/// Contact lists: `(owner, contact)`.
pub(crate) const CONTACTS: TableDefinition<(&str, &str), ()> = TableDefinition::new("contacts");
/// Messages by id (ULID). Value: JSON [`crate::model::Message`].
pub(crate) const MESSAGES: TableDefinition<&str, &[u8]> = TableDefinition::new("messages");
/// Per-recipient delivery and read state: `(address, message id) -> read`.
pub(crate) const MAILBOX: TableDefinition<(&str, &str), bool> = TableDefinition::new("mailbox");
/// Thread membership: `(thread id, message id)`.
pub(crate) const THREADS: TableDefinition<(&str, &str), ()> = TableDefinition::new("threads");
/// Mail held for a name not yet registered: `(repo, name, message id)`.
pub(crate) const HELD: TableDefinition<(&str, &str, &str), ()> = TableDefinition::new("held");
/// Questions by message id. Value: JSON [`crate::model::Question`].
pub(crate) const QUESTIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("questions");
/// Index of the open (unanswered) rows of [`QUESTIONS`]: question id.
/// Rebuilt from [`QUESTIONS`] on every [`Store::open`].
pub(crate) const OPEN_QUESTIONS: TableDefinition<&str, ()> = TableDefinition::new("open_questions");
/// Store metadata, e.g. `schema_version`.
pub(crate) const META: TableDefinition<&str, &str> = TableDefinition::new("meta");

const SCHEMA_VERSION_KEY: &str = "schema_version";
/// The on-disk schema this build reads and writes.
pub const SCHEMA_VERSION: &str = "1";

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
            txn.open_table(AGENTS)?;
            txn.open_table(CONTACTS)?;
            txn.open_table(MESSAGES)?;
            txn.open_table(MAILBOX)?;
            txn.open_table(THREADS)?;
            txn.open_table(HELD)?;
            txn.open_table(QUESTIONS)?;
            questions::rebuild_open_index(&txn)?;
            let mut meta = txn.open_table(META)?;
            let existing = meta.get(SCHEMA_VERSION_KEY)?.map(|v| v.value().to_string());
            match existing {
                None => {
                    meta.insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION)?;
                }
                Some(v) if v == SCHEMA_VERSION => {}
                Some(v) => bail!(
                    "{} has schema version {v}, this build supports {SCHEMA_VERSION}",
                    path.display()
                ),
            }
        }
        txn.commit()?;
        Ok(Store { db })
    }

    /// Start a read transaction.
    pub(crate) fn read(&self) -> Result<redb::ReadTransaction, StoreError> {
        Ok(self.db.begin_read()?)
    }
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
