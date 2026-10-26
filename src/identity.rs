//! Who the CLI or MCP server is acting as, and which process a registration
//! binds to.

use std::ffi::OsString;

use crate::model::{Harness, ProcId, SessionKey};
use crate::procinfo;
use crate::protocol::Caller;

/// Names an agent address to act as, overriding the process binding.
pub const ENV_ADDRESS: &str = "AGENTMAIL_ADDRESS";
/// Set to `1` to act as the human.
pub const ENV_HUMAN: &str = "AGENTMAIL_HUMAN";
/// The opencode session id, set in the shell commands opencode runs.
pub const ENV_OPENCODE_SESSION: &str = "OPENCODE_SESSION_ID";
/// The kiro-cli session id, set in the environment of kiro's hooks.
pub const ENV_KIRO_SESSION: &str = "KIRO_SESSION_ID";

/// The harness session this process runs in, from the environment the
/// harness sets: `OPENCODE_SESSION_ID`, then `KIRO_SESSION_ID`. The first
/// non-empty one wins.
pub fn session_from_env(var: impl Fn(&str) -> Option<OsString>) -> Option<SessionKey> {
    [
        (ENV_OPENCODE_SESSION, Harness::Opencode),
        (ENV_KIRO_SESSION, Harness::Kiro),
    ]
    .into_iter()
    .find_map(|(name, harness)| {
        let id = var(name).filter(|v| !v.is_empty())?;
        Some(SessionKey {
            harness,
            id: id.to_string_lossy().into_owned(),
        })
    })
}

/// The caller for a CLI invocation:
///
/// 1. `--as human` (`as_human`): [`Caller::Human`]. An explicit flag wins
///    over the environment.
/// 2. `AGENTMAIL_ADDRESS` set and non-empty: [`Caller::Address`].
/// 3. `AGENTMAIL_HUMAN=1`: [`Caller::Human`].
/// 4. A harness session from the environment ([`session_from_env`]):
///    [`Caller::Session`]. Shell commands of different opencode sessions
///    share one process tree, so the session must win over ancestry.
/// 5. Otherwise this process's ancestry, nearest first
///    ([`Caller::Procs`]); the daemon picks the agent bound to one of them
///    and refuses with `NoIdentity` when none is. It never falls back to the
///    human.
pub fn cli_caller(as_human: bool) -> Caller {
    caller_from(as_human, |k| std::env::var_os(k), std::process::id())
}

/// [`cli_caller`] with the environment and own pid injected.
fn caller_from(as_human: bool, var: impl Fn(&str) -> Option<OsString>, pid: u32) -> Caller {
    if as_human {
        return Caller::Human;
    }
    if let Some(address) = var(ENV_ADDRESS).filter(|a| !a.is_empty()) {
        return Caller::Address {
            address: address.to_string_lossy().into_owned(),
        };
    }
    if var(ENV_HUMAN).is_some_and(|v| v == "1") {
        return Caller::Human;
    }
    if let Some(session) = session_from_env(&var) {
        return Caller::Session { session };
    }
    Caller::Procs {
        procs: procinfo::ancestors(pid),
    }
}

/// The process a registration binds to, with its harness:
///
/// 1. `explicit_pid` that is one of our own ancestors (the MCP server passes
///    its parent): the nearest known harness process above us, if any. A
///    harness may run its MCP servers through a helper (kiro V3 spawns them
///    from `node`), and binding the helper would record [`Harness::Other`].
///    With no harness ancestor: that process, with the harness its command
///    name suggests.
/// 2. Any other `explicit_pid` (`register --pid`): exactly that process,
///    with the harness its command name suggests.
/// 3. Otherwise the nearest ancestor that is a known harness process.
/// 4. Otherwise the grandparent, i.e. the parent of the shell running the
///    CLI, as [`Harness::Other`].
///
/// `None` when the process is gone or unreadable, or would be PID 1: every
/// process descends from init, so binding to it would make the agent the
/// identity of every session on the machine.
pub fn register_proc(explicit_pid: Option<u32>) -> Option<(ProcId, Harness)> {
    register_proc_for(std::process::id(), explicit_pid)
}

/// [`register_proc`] for process `me`.
fn register_proc_for(me: u32, explicit_pid: Option<u32>) -> Option<(ProcId, Harness)> {
    register_proc_with(me, explicit_pid, procinfo::find_harness_ancestor)
}

/// [`register_proc_for`] with the harness-ancestor lookup injected, so the
/// grandparent fallback can be tested even when the tests themselves run
/// under a harness.
fn register_proc_with(
    me: u32,
    explicit_pid: Option<u32>,
    find_harness: impl Fn(u32) -> Option<(ProcId, Harness)>,
) -> Option<(ProcId, Harness)> {
    let usable = |p: &ProcId| p.pid > 1;
    if let Some(pid) = explicit_pid {
        let p = procinfo::proc_id(pid).filter(usable)?;
        if procinfo::ancestors(me).contains(&p)
            && let Some(found) = find_harness(me).filter(|(h, _)| usable(h))
        {
            return Some(found);
        }
        let harness = procinfo::comm(pid).map_or(Harness::Other, |c| procinfo::detect_harness(&c));
        // Guard against the PID being reused between the two reads.
        return procinfo::is_alive(&p).then_some((p, harness));
    }
    if let Some(found) = find_harness(me).filter(|(p, _)| usable(p)) {
        return Some(found);
    }
    procinfo::ancestors(me)
        .get(1)
        .copied()
        .filter(usable)
        .map(|p| (p, Harness::Other))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn identity_address_env_gives_address() {
        let me = std::process::id();
        assert_eq!(
            caller_from(false, env(&[(ENV_ADDRESS, "x")]), me),
            Caller::Address {
                address: "x".into()
            }
        );
        // It beats AGENTMAIL_HUMAN, but not an explicit --as human.
        let both = env(&[(ENV_ADDRESS, "x"), (ENV_HUMAN, "1")]);
        assert_eq!(
            caller_from(false, &both, me),
            Caller::Address {
                address: "x".into()
            }
        );
        assert_eq!(caller_from(true, &both, me), Caller::Human);
    }

    #[test]
    fn identity_human_env_or_flag_gives_human() {
        let me = std::process::id();
        assert_eq!(
            caller_from(false, env(&[(ENV_HUMAN, "1")]), me),
            Caller::Human
        );
        assert_eq!(caller_from(true, env(&[]), me), Caller::Human);
    }

    #[test]
    fn identity_neither_gives_own_ancestry() {
        let me = std::process::id();
        // Empty or non-`1` values count as unset.
        for vars in [
            env(&[]),
            env(&[(ENV_ADDRESS, ""), (ENV_HUMAN, "0")]),
            env(&[(ENV_HUMAN, "true")]),
        ] {
            match caller_from(false, vars, me) {
                Caller::Procs { procs } => {
                    assert!(!procs.is_empty());
                    assert_eq!(Some(procs[0].pid), procinfo::parent_pid(me));
                }
                other => panic!("expected Procs, got {other:?}"),
            }
        }
    }

    fn opencode(id: &str) -> SessionKey {
        SessionKey {
            harness: Harness::Opencode,
            id: id.into(),
        }
    }

    #[test]
    fn identity_session_from_env_table() {
        let kiro = |id: &str| SessionKey {
            harness: Harness::Kiro,
            id: id.into(),
        };
        type Vars<'a> = &'a [(&'a str, &'a str)];
        let cases: [(Vars, Option<SessionKey>); 6] = [
            (&[(ENV_OPENCODE_SESSION, "s1")], Some(opencode("s1"))),
            (&[(ENV_KIRO_SESSION, "k1")], Some(kiro("k1"))),
            (
                &[(ENV_OPENCODE_SESSION, "s1"), (ENV_KIRO_SESSION, "k1")],
                Some(opencode("s1")),
            ),
            // An empty value counts as unset, so the next source wins.
            (
                &[(ENV_OPENCODE_SESSION, ""), (ENV_KIRO_SESSION, "k1")],
                Some(kiro("k1")),
            ),
            (&[(ENV_OPENCODE_SESSION, ""), (ENV_KIRO_SESSION, "")], None),
            (&[], None),
        ];
        for (vars, want) in cases {
            assert_eq!(session_from_env(env(vars)), want, "{vars:?}");
        }
    }

    #[test]
    fn identity_cli_caller_prefers_session_over_procs() {
        let me = std::process::id();
        let session = env(&[(ENV_OPENCODE_SESSION, "s1")]);
        assert_eq!(
            caller_from(false, &session, me),
            Caller::Session {
                session: opencode("s1")
            }
        );
        // An explicit address, the human env and --as human all beat it.
        let addr = env(&[(ENV_OPENCODE_SESSION, "s1"), (ENV_ADDRESS, "x")]);
        assert_eq!(
            caller_from(false, &addr, me),
            Caller::Address {
                address: "x".into()
            }
        );
        let human = env(&[(ENV_OPENCODE_SESSION, "s1"), (ENV_HUMAN, "1")]);
        assert_eq!(caller_from(false, &human, me), Caller::Human);
        assert_eq!(caller_from(true, &session, me), Caller::Human);
    }

    #[test]
    fn identity_cli_caller_reads_the_real_environment() {
        let expected = caller_from(false, |k| std::env::var_os(k), std::process::id());
        assert_eq!(cli_caller(false), expected);
        assert_eq!(cli_caller(true), Caller::Human);
    }

    #[test]
    fn identity_register_proc_explicit_pid() {
        let me = std::process::id();
        let (p, harness) = register_proc(Some(me)).expect("own process");
        assert_eq!(p, procinfo::proc_id(me).unwrap());
        assert_eq!(harness, Harness::Other);
        assert_eq!(register_proc(Some(u32::MAX - 1)), None);
        assert_eq!(register_proc(Some(1)), None);
    }

    #[test]
    fn identity_register_proc_finds_harness_ancestor() {
        // A child running under a process named "claude" binds to it.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("claude");
        std::os::unix::fs::symlink("/bin/sh", &fake).unwrap();
        let mut child = std::process::Command::new(&fake)
            .arg("-c")
            .arg("sleep 5 & echo $!; wait")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        use std::io::BufRead;
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let grandchild: u32 = line.trim().parse().unwrap();
        let found = register_proc_for(grandchild, None);
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(grandchild.to_string())
            .status();
        let _ = child.kill();
        let _ = child.wait();
        let (p, harness) = found.expect("harness ancestor");
        assert_eq!(harness, Harness::ClaudeCode);
        assert_eq!(p.pid, child.id());
    }

    /// Spawn `sh -c 'sleep 5 & echo $!; wait'` and return the shell child
    /// and the PID of its `sleep` grandchild.
    fn spawn_grandchild() -> (std::process::Child, u32) {
        use std::io::BufRead;
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 5 & echo $!; wait")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        (child, line.trim().parse().unwrap())
    }

    fn reap(mut child: std::process::Child, grandchild: u32) {
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(grandchild.to_string())
            .status();
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn identity_register_proc_explicit_ancestor_prefers_harness() {
        // sleep -> sh -> test process. The sleep names its parent, the shell,
        // and a harness sits above it: the harness wins.
        let (child, grandchild) = spawn_grandchild();
        let me = procinfo::proc_id(std::process::id()).unwrap();
        let shell = child.id();
        let harness = |_| Some((me, Harness::Kiro));
        let via_parent = register_proc_with(grandchild, Some(shell), harness);
        // No harness above: the named parent itself.
        let no_harness = register_proc_with(grandchild, Some(shell), |_| None);
        // A pid that is not our ancestor binds exactly, harness or not.
        let not_ancestor = register_proc_with(me.pid, Some(grandchild), harness);
        let shell_id = procinfo::proc_id(shell);
        let sleep_id = procinfo::proc_id(grandchild);
        reap(child, grandchild);
        assert_eq!(via_parent, Some((me, Harness::Kiro)));
        assert_eq!(no_harness, shell_id.map(|p| (p, Harness::Other)));
        assert_eq!(not_ancestor, sleep_id.map(|p| (p, Harness::Other)));
    }

    #[test]
    fn identity_register_proc_falls_back_to_grandparent() {
        // With no harness ancestor, the sleep binds to its grandparent: the
        // test process itself (sleep -> sh -> test process).
        let (child, grandchild) = spawn_grandchild();
        let found = register_proc_with(grandchild, None, |_| None);
        let shell_saw_us = procinfo::parent_pid(child.id());
        reap(child, grandchild);
        let me = std::process::id();
        assert_eq!(shell_saw_us, Some(me));
        assert_eq!(
            found,
            Some((procinfo::proc_id(me).unwrap(), Harness::Other))
        );
    }

    #[test]
    fn identity_register_proc_rejects_harness_at_pid_1() {
        // A harness lookup that lands on init is ignored in favour of the
        // grandparent.
        let (child, grandchild) = spawn_grandchild();
        let init = procinfo::proc_id(1);
        let found =
            register_proc_with(grandchild, None, |_| init.map(|p| (p, Harness::ClaudeCode)));
        reap(child, grandchild);
        let me = std::process::id();
        assert_eq!(
            found,
            Some((procinfo::proc_id(me).unwrap(), Harness::Other))
        );
    }

    #[test]
    fn identity_register_proc_never_falls_back_to_pid_1() {
        // A process whose grandparent is init gets no binding.
        let me = std::process::id();
        let chain = procinfo::ancestors(me);
        assert_eq!(
            chain.last().map(|p| p.pid),
            Some(1),
            "ancestry reaches init"
        );
        // `x`'s grandparent is the last entry of the chain, i.e. PID 1.
        let x = match chain.len() {
            n if n >= 3 => chain[n - 3].pid,
            _ => me,
        };
        assert_eq!(procinfo::ancestors(x).get(1).map(|p| p.pid).unwrap_or(1), 1);
        assert_eq!(register_proc_with(x, None, |_| None), None);
    }
}
