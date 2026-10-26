//! Filesystem locations for the database, socket and daemon lock.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

const DB_FILE: &str = "agentmail.redb";
const SOCKET_FILE: &str = "agentmail.sock";
const LOCK_FILE: &str = "agentmail.lock";
const WAITER_FILE: &str = "agentmail.waiter";

/// What a daemon writes to the lock file once it holds the lock, read by
/// clients deciding whether to wait for a successor after it has gone: one
/// line `supervised` or `on-demand`, one line with its process id, then one
/// line with the socket it serves, a backslash and a newline in it written
/// as `\\` and `\n`.
///
/// Only daemons of this version write it. One from before holds the lock
/// without touching the file, so while it runs the file holds whatever an
/// earlier daemon left there, or nothing.
///
/// A supervised daemon waiting for the lock (see [`Paths::waiter`]) writes
/// the same record to the waiter file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockRecord {
    /// Run by a supervisor (`daemon run --wait-for-lock`), which starts a
    /// new daemon when this one exits.
    pub supervised: bool,
    /// The daemon's process id, which tells a successor's record from its
    /// own.
    pub pid: u32,
    /// The socket the daemon serves.
    pub socket: PathBuf,
}

impl LockRecord {
    const SUPERVISED: &[u8] = b"supervised";
    const ON_DEMAND: &[u8] = b"on-demand";

    /// The lock file contents for this record.
    pub fn encode(&self) -> Vec<u8> {
        use std::os::unix::ffi::OsStrExt;
        let mut out = if self.supervised {
            Self::SUPERVISED
        } else {
            Self::ON_DEMAND
        }
        .to_vec();
        out.push(b'\n');
        out.extend_from_slice(self.pid.to_string().as_bytes());
        out.push(b'\n');
        for &b in self.socket.as_os_str().as_bytes() {
            match b {
                b'\\' => out.extend_from_slice(b"\\\\"),
                b'\n' => out.extend_from_slice(b"\\n"),
                b => out.push(b),
            }
        }
        out.push(b'\n');
        out
    }

    /// Parse lock file contents; `None` unless they are exactly a record
    /// (an empty file, as left by an older daemon, is none).
    pub fn decode(bytes: &[u8]) -> Option<LockRecord> {
        use std::os::unix::ffi::OsStrExt;
        let body = bytes.strip_suffix(b"\n")?;
        let mut lines = body.split(|b| *b == b'\n');
        let supervised = match lines.next()? {
            Self::SUPERVISED => true,
            Self::ON_DEMAND => false,
            _ => return None,
        };
        let pid = lines.next()?;
        if pid.is_empty() || !pid.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let pid = std::str::from_utf8(pid).ok()?.parse().ok()?;
        let socket = lines.next().filter(|s| !s.is_empty())?;
        if lines.next().is_some() {
            return None;
        }
        let mut path = Vec::with_capacity(socket.len());
        let mut bytes = socket.iter();
        while let Some(&b) = bytes.next() {
            path.push(match b {
                b'\\' => match bytes.next()? {
                    b'\\' => b'\\',
                    b'n' => b'\n',
                    _ => return None,
                },
                b => b,
            });
        }
        Some(LockRecord {
            supervised,
            pid,
            socket: PathBuf::from(std::ffi::OsStr::from_bytes(&path)),
        })
    }
}

/// Whether some process holds the `flock` on the file at `path`; see
/// [`Paths::lock_held`].
fn held(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => false,
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

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

    /// Whether some process holds the daemon lock at [`Paths::lock`].
    ///
    /// A non-blocking probe on a fresh open file description, so it also
    /// sees a lock held elsewhere in this process. A missing or unreadable
    /// lock file counts as not held. When the lock is free the probe takes
    /// it for an instant; a starting daemon retries briefly, so this cannot
    /// make it give up.
    pub fn lock_held(&self) -> bool {
        held(&self.lock)
    }

    /// The record the last daemon to take the lock left in the lock file,
    /// if any.
    pub fn lock_record(&self) -> Option<LockRecord> {
        LockRecord::decode(&std::fs::read(&self.lock).ok()?)
    }

    /// The waiter file, next to [`Paths::lock`]. A supervised daemon
    /// (`daemon run --wait-for-lock`) holds a lock on it, and has written its
    /// [`LockRecord`] to it, for as long as it waits for the daemon lock, so
    /// a client that stops the daemon it waits behind knows a successor is
    /// on its way. It lets go only once it holds the daemon lock and has
    /// recorded itself there.
    pub fn waiter(&self) -> PathBuf {
        self.data_dir.join(WAITER_FILE)
    }

    /// Whether a supervised daemon waits for the daemon lock: some process
    /// holds the lock on [`Paths::waiter`]. Probes like [`Paths::lock_held`].
    pub fn waiter_held(&self) -> bool {
        held(&self.waiter())
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

    #[test]
    fn lock_record_round_trips_and_rejects_anything_else() {
        for supervised in [true, false] {
            for socket in [
                "/run/user/1000/agentmail.sock",
                "/tmp/a\nb/agentmail.sock",
                "/tmp/a\\nb/agentmail.sock",
                "/tmp/\\\n\\/s\n",
            ] {
                let rec = LockRecord {
                    supervised,
                    pid: 4_000_000,
                    socket: PathBuf::from(socket),
                };
                assert_eq!(LockRecord::decode(&rec.encode()), Some(rec), "{socket:?}");
            }
        }
        assert_eq!(
            LockRecord::decode(b"supervised\n42\n/s\n"),
            Some(LockRecord {
                supervised: true,
                pid: 42,
                socket: PathBuf::from("/s"),
            })
        );
        for bad in [
            &b""[..],
            b"supervised\n",
            b"supervised\n42\n",
            b"supervised\n42\n/s",
            b"supervised\n42\n\n",
            b"maybe\n42\n/s\n",
            b"supervised\n42\n/s\nextra\n",
            // The record of a daemon from before process ids were recorded.
            b"supervised\n/s\n",
            // A process id that is not one.
            b"supervised\n\n/s\n",
            b"supervised\n-1\n/s\n",
            b"supervised\n+1\n/s\n",
            b"supervised\n99999999999\n/s\n",
            // A lone backslash, or one before anything but `\` or `n`.
            b"supervised\n42\n/s\\\n",
            b"supervised\n42\n/s\\x\n",
        ] {
            assert_eq!(LockRecord::decode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn lock_record_reads_the_lock_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::in_dir(dir.path());
        assert_eq!(paths.lock_record(), None);
        std::fs::write(&paths.lock, b"").unwrap();
        assert_eq!(paths.lock_record(), None);
        let rec = LockRecord {
            supervised: true,
            pid: 42,
            socket: paths.socket.clone(),
        };
        std::fs::write(&paths.lock, rec.encode()).unwrap();
        assert_eq!(paths.lock_record(), Some(rec));
    }

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
    fn paths_lock_held_sees_a_held_lock_only() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = Paths::in_dir(dir.path());
        // `lock_held` probes by taking the lock itself, and a child that
        // another test thread forks shares every lock file description we
        // have open until it execs. So right after a probe or a release the
        // lock can look held for a moment: the "not held" checks and our
        // own `try_lock` retry briefly (see `test_util::eventually`).
        let not_held =
            || crate::test_util::eventually(|| if p.lock_held() { Err(()) } else { Ok(()) });
        assert!(!p.lock_held(), "a missing lock file must not count as held");
        let lock = std::fs::File::create(&p.lock).unwrap();
        assert_eq!(not_held(), Ok(()), "an unlocked file counts as held");
        crate::test_util::eventually(|| lock.try_lock()).unwrap();
        assert!(p.lock_held());
        drop(lock);
        assert_eq!(not_held(), Ok(()), "a released lock still counts as held");
    }

    #[test]
    fn paths_in_dir_puts_everything_in_dir() {
        let p = Paths::in_dir(Path::new("/t"));
        assert_eq!(p.data_dir, PathBuf::from("/t"));
        assert_eq!(p.db, PathBuf::from("/t/agentmail.redb"));
        assert_eq!(p.socket, PathBuf::from("/t/agentmail.sock"));
        assert_eq!(p.lock, PathBuf::from("/t/agentmail.lock"));
        assert_eq!(p.waiter(), PathBuf::from("/t/agentmail.waiter"));
    }
}
