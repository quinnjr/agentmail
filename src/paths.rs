//! Filesystem locations for the database, socket and daemon lock.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

const DB_FILE: &str = "agentmail.redb";
const SOCKET_FILE: &str = "agentmail.sock";
const LOCK_FILE: &str = "agentmail.lock";

/// Where agentmail keeps its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Directory holding the database and lock file.
    pub data_dir: PathBuf,
    /// The redb database file. Only the daemon opens it.
    pub db: PathBuf,
    /// The daemon's Unix socket.
    pub socket: PathBuf,
    /// Lock file that guarantees a single daemon.
    pub lock: PathBuf,
}

impl Paths {
    /// Resolve paths from the process environment.
    ///
    /// Data dir: `AGENTMAIL_DATA_DIR`, else `$XDG_DATA_HOME/agentmail`, else
    /// `$HOME/.local/share/agentmail`. Socket: `AGENTMAIL_SOCKET`, else
    /// `$XDG_RUNTIME_DIR/agentmail.sock`, else `<data_dir>/agentmail.sock`.
    pub fn from_env() -> Result<Paths> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// Resolve paths from an arbitrary variable lookup. Empty values count as unset.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Paths> {
        let var = |k: &str| get(k).filter(|v| !v.is_empty()).map(PathBuf::from);

        let data_dir = if let Some(dir) = var("AGENTMAIL_DATA_DIR") {
            dir
        } else if let Some(xdg) = var("XDG_DATA_HOME") {
            xdg.join("agentmail")
        } else if let Some(home) = var("HOME") {
            home.join(".local").join("share").join("agentmail")
        } else {
            bail!("cannot locate data directory: set HOME, XDG_DATA_HOME or AGENTMAIL_DATA_DIR");
        };

        let socket = if let Some(sock) = var("AGENTMAIL_SOCKET") {
            sock
        } else if let Some(run) = var("XDG_RUNTIME_DIR") {
            run.join(SOCKET_FILE)
        } else {
            data_dir.join(SOCKET_FILE)
        };

        Ok(Paths {
            db: data_dir.join(DB_FILE),
            lock: data_dir.join(LOCK_FILE),
            socket,
            data_dir,
        })
    }

    /// Put everything, socket included, in `dir`. Used by tests.
    pub fn in_dir(dir: &Path) -> Paths {
        Paths {
            data_dir: dir.to_path_buf(),
            db: dir.join(DB_FILE),
            socket: dir.join(SOCKET_FILE),
            lock: dir.join(LOCK_FILE),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn paths_from_env_honours_overrides() {
        let p = Paths::from_vars(vars(&[
            ("AGENTMAIL_DATA_DIR", "/x"),
            ("AGENTMAIL_SOCKET", "/y.sock"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("HOME", "/home/u"),
        ]))
        .unwrap();
        assert_eq!(p.data_dir, PathBuf::from("/x"));
        assert_eq!(p.db, PathBuf::from("/x/agentmail.redb"));
        assert_eq!(p.socket, PathBuf::from("/y.sock"));
        assert_eq!(p.lock, PathBuf::from("/x/agentmail.lock"));
    }

    #[test]
    fn paths_xdg_runtime_dir_used_for_socket() {
        let p = Paths::from_vars(vars(&[
            ("XDG_DATA_HOME", "/data"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ]))
        .unwrap();
        assert_eq!(p.data_dir, PathBuf::from("/data/agentmail"));
        assert_eq!(p.db, PathBuf::from("/data/agentmail/agentmail.redb"));
        assert_eq!(p.socket, PathBuf::from("/run/user/1000/agentmail.sock"));
        assert_eq!(p.lock, PathBuf::from("/data/agentmail/agentmail.lock"));
    }

    #[test]
    fn paths_socket_falls_back_to_data_dir_without_runtime_dir() {
        let p = Paths::from_vars(vars(&[("HOME", "/home/u")])).unwrap();
        assert_eq!(p.data_dir, PathBuf::from("/home/u/.local/share/agentmail"));
        assert_eq!(
            p.socket,
            PathBuf::from("/home/u/.local/share/agentmail/agentmail.sock")
        );
    }

    #[test]
    fn paths_empty_vars_are_ignored() {
        let p = Paths::from_vars(vars(&[
            ("HOME", "/home/u"),
            ("XDG_DATA_HOME", ""),
            ("XDG_RUNTIME_DIR", ""),
            ("AGENTMAIL_SOCKET", ""),
        ]))
        .unwrap();
        assert_eq!(p.data_dir, PathBuf::from("/home/u/.local/share/agentmail"));
        assert_eq!(
            p.socket,
            PathBuf::from("/home/u/.local/share/agentmail/agentmail.sock")
        );
    }

    #[test]
    fn paths_without_home_or_xdg_is_an_error() {
        assert!(Paths::from_vars(vars(&[])).is_err());
    }

    #[test]
    fn paths_in_dir_puts_everything_in_dir() {
        let p = Paths::in_dir(Path::new("/t"));
        assert_eq!(p.data_dir, PathBuf::from("/t"));
        assert_eq!(p.db, PathBuf::from("/t/agentmail.redb"));
        assert_eq!(p.socket, PathBuf::from("/t/agentmail.sock"));
        assert_eq!(p.lock, PathBuf::from("/t/agentmail.lock"));
    }
}
