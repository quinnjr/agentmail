//! Who the CLI or MCP server is acting as, and which process a registration
//! binds to.

use std::ffi::OsString;

use crate::model::{Harness, ProcId};
use crate::procinfo;
use crate::protocol::Caller;

/// Names an agent address to act as, overriding the process binding.
pub const ENV_ADDRESS: &str = "AGENTMAIL_ADDRESS";
/// Set to `1` to act as the human.
pub const ENV_HUMAN: &str = "AGENTMAIL_HUMAN";

/// The caller for a CLI invocation:
///
/// 1. `--as human` (`as_human`): [`Caller::Human`]. An explicit flag wins
///    over the environment.
/// 2. `AGENTMAIL_ADDRESS` set and non-empty: [`Caller::Address`].
/// 3. `AGENTMAIL_HUMAN=1`: [`Caller::Human`].
/// 4. Otherwise this process's ancestry, nearest first
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
    Caller::Procs {
        procs: procinfo::ancestors(pid),
    }
}

/// The process a registration binds to, with its harness:
///
/// 1. `explicit_pid` (e.g. the MCP server passes its parent, the harness):
///    that process, with the harness its command name suggests.
/// 2. Otherwise the nearest ancestor that is a known harness process.
/// 3. Otherwise the grandparent, i.e. the parent of the shell running the
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
