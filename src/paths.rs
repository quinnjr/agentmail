//! Filesystem locations for the database, socket and daemon lock.

use std::ffi::OsString;
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
    ///
    /// Values are read with [`std::env::var_os`], so non-UTF-8 paths are used
    /// as given rather than treated as unset.
    ///
    /// This is a thin wrapper over [`Paths::from_vars`]; the tests exercise
    /// `from_vars` directly to avoid mutating the process environment.
    pub fn from_env() -> Result<Paths> {
        Self::from_vars(|k| std::env::var_os(k))
    }

    /// Resolve paths from an arbitrary variable lookup. Empty values count as unset.
    ///
    /// Per the XDG Base Directory spec, relative `XDG_DATA_HOME` and
    /// `XDG_RUNTIME_DIR` values are ignored, as is a relative `HOME`, so every
    /// process resolves the same paths regardless of its working directory.
    /// For the same reason, relative `AGENTMAIL_DATA_DIR` and `AGENTMAIL_SOCKET`
    /// overrides are rejected with an error: an explicit override that cannot
    /// be honoured consistently should fail loudly rather than fall back.
    pub fn from_vars(get: impl Fn(&str) -> Option<OsString>) -> Result<Paths> {
        let var = |k: &str| get(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let abs_var = |k: &str| var(k).filter(|p| p.is_absolute());
        let override_var = |k: &str| -> Result<Option<PathBuf>> {
            match var(k) {
                Some(p) if !p.is_absolute() => {
                    bail!("{k} must be an absolute path, got {}", p.display())
                }
                other => Ok(other),
            }
        };

        let data_dir = if let Some(dir) = override_var("AGENTMAIL_DATA_DIR")? {
            dir
        } else if let Some(xdg) = abs_var("XDG_DATA_HOME") {
            xdg.join("agentmail")
        } else if let Some(home) = abs_var("HOME") {
            home.join(".local").join("share").join("agentmail")
        } else {
            bail!("cannot locate data directory: set HOME, XDG_DATA_HOME or AGENTMAIL_DATA_DIR");
        };

        let socket = if let Some(sock) = override_var("AGENTMAIL_SOCKET")? {
            sock
        } else if let Some(run) = abs_var("XDG_RUNTIME_DIR") {
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
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
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
    fn paths_relative_runtime_dir_is_ignored() {
        let p = Paths::from_vars(vars(&[("HOME", "/home/u"), ("XDG_RUNTIME_DIR", "rel")])).unwrap();
        assert_eq!(
            p.socket,
            PathBuf::from("/home/u/.local/share/agentmail/agentmail.sock")
        );
    }

    #[test]
    fn paths_relative_xdg_data_home_is_ignored() {
        let p = Paths::from_vars(vars(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "rel")])).unwrap();
        assert_eq!(p.data_dir, PathBuf::from("/home/u/.local/share/agentmail"));
    }

    #[test]
    fn paths_relative_home_is_ignored() {
        assert!(Paths::from_vars(vars(&[("HOME", "rel")])).is_err());
    }

    #[test]
    fn paths_relative_data_dir_override_is_rejected() {
        let err = Paths::from_vars(vars(&[("HOME", "/home/u"), ("AGENTMAIL_DATA_DIR", "rel")]))
            .unwrap_err();
        assert!(err.to_string().contains("AGENTMAIL_DATA_DIR"), "{err}");
    }

    #[test]
    fn paths_relative_socket_override_is_rejected() {
        let err = Paths::from_vars(vars(&[("HOME", "/home/u"), ("AGENTMAIL_SOCKET", "a.sock")]))
            .unwrap_err();
        assert!(err.to_string().contains("AGENTMAIL_SOCKET"), "{err}");
    }

    #[test]
    fn paths_non_utf8_home_is_used_as_a_path() {
        use std::os::unix::ffi::OsStringExt;
        let home = OsString::from_vec(b"/home/\xffu".to_vec());
        let expected = PathBuf::from(home.clone())
            .join(".local")
            .join("share")
            .join("agentmail");
        let p = Paths::from_vars(move |k| (k == "HOME").then(|| home.clone())).unwrap();
        assert_eq!(p.data_dir, expected);
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
