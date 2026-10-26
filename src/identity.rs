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
/// The kiro-cli root conversation id, set for kiro's shell tool commands
/// and hooks (not its MCP servers).
pub const ENV_KIRO_SESSION: &str = "KIRO_SESSION_ID";

/// The session variables, in the order [`session_from_env`] reads them.
const SESSION_VARS: [(&str, Harness); 2] = [
    (ENV_OPENCODE_SESSION, Harness::Opencode),
    (ENV_KIRO_SESSION, Harness::Kiro),
];

/// The session of `harness` named by variable `name`, if set and non-empty.
fn read_session(
    var: impl Fn(&str) -> Option<OsString>,
    name: &str,
    harness: Harness,
) -> Option<SessionKey> {
    let id = var(name).filter(|v| !v.is_empty())?;
    Some(SessionKey {
        harness,
        id: id.to_string_lossy().into_owned(),
    })
}

/// The harness session this process runs in, from the environment the
/// harness sets: `OPENCODE_SESSION_ID`, then `KIRO_SESSION_ID`. The first
/// non-empty one wins.
pub fn session_from_env(var: impl Fn(&str) -> Option<OsString>) -> Option<SessionKey> {
    SESSION_VARS
        .into_iter()
        .find_map(|(name, harness)| read_session(&var, name, harness))
}

/// [`session_from_env`], checked against `nearest`, the harness of this
/// process's nearest harness ancestor ([`procinfo::find_harness_ancestor`]).
///
/// Session variables are inherited: a harness started from another
/// harness's shell (`claude` run from an opencode bash tool) passes the
/// outer `OPENCODE_SESSION_ID` on to its own commands. Only the variable of
/// the harness this process actually runs under counts, so with a harness
/// ancestor that sets none (Claude Code) there is no session. With no
/// harness ancestor at all the environment is trusted as is.
pub fn session_for(
    var: impl Fn(&str) -> Option<OsString>,
    nearest: Option<Harness>,
) -> Option<SessionKey> {
    match nearest {
        None | Some(Harness::Other) => session_from_env(var),
        Some(h) => SESSION_VARS
            .into_iter()
            .find(|(_, vh)| *vh == h)
            .and_then(|(name, harness)| read_session(&var, name, harness)),
    }
}

/// [`session_for`] this process: its real environment and harness ancestor.
pub fn env_session() -> Option<SessionKey> {
    session_in(
        |k| std::env::var_os(k),
        std::process::id(),
        procinfo::find_harness_ancestor,
    )
}

/// [`session_for`] process `pid`, looking up its harness ancestor only when
/// a session variable is set.
fn session_in(
    var: impl Fn(&str) -> Option<OsString>,
    pid: u32,
    find_harness: impl Fn(u32) -> Option<(ProcId, Harness)>,
) -> Option<SessionKey> {
    session_from_env(&var)?;
    session_for(var, find_harness(pid).map(|(_, h)| h))
}

/// The caller for a CLI invocation:
///
/// 1. `--as human` (`as_human`): [`Caller::Human`]. An explicit flag wins
///    over the environment.
/// 2. `AGENTMAIL_ADDRESS` set and non-empty: [`Caller::Address`].
/// 3. `AGENTMAIL_HUMAN=1`: [`Caller::Human`].
/// 4. A session from the environment ([`session_for`] the nearest harness
///    ancestor) of a harness that runs several sessions in one process
///    (opencode): [`Caller::Session`]. Shell commands of different opencode
///    sessions share one process tree, so the session must win over
///    ancestry. A kiro session is not used here: kiro's MCP server gets no
///    session id and registers by process alone, so its shell commands must
///    resolve by process too.
/// 5. Otherwise this process's ancestry, nearest first
///    ([`Caller::Procs`]); the daemon picks the agent bound to one of them
///    and refuses with `NoIdentity` when none is. It never falls back to the
///    human.
pub fn cli_caller(as_human: bool) -> Caller {
    caller_from(
        as_human,
        |k| std::env::var_os(k),
        std::process::id(),
        procinfo::find_harness_ancestor,
    )
}

/// [`cli_caller`] with the environment, own pid and harness-ancestor lookup
/// injected.
fn caller_from(
    as_human: bool,
    var: impl Fn(&str) -> Option<OsString>,
    pid: u32,
    find_harness: impl Fn(u32) -> Option<(ProcId, Harness)>,
) -> Caller {
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
    if let Some(session) =
        session_in(&var, pid, find_harness).filter(|k| k.harness.shares_process())
    {
        return Caller::Session { session };
    }
    Caller::Procs {
        procs: procinfo::ancestors(pid),
    }
}

/// The process a registration binds to, with its harness:
///
/// 1. `explicit_pid` that is one of our own ancestors (the MCP server passes
///    its parent) and is not itself a known harness process, under a kiro
///    process: that kiro process. kiro V3 runs its MCP servers from a `node`
///    helper, and binding the helper would record [`Harness::Other`]. Only
///    kiro gets this: an unknown MCP client run from a Claude Code or
///    opencode shell must bind itself, not the harness it happens to run
///    under, or two agents would share one process binding.
/// 2. Any other `explicit_pid` (`register --pid`, or a parent that is a
///    harness or has no kiro above it): exactly that process, with the
///    harness its command name suggests.
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
        let harness = procinfo::comm(pid).map_or(Harness::Other, |c| procinfo::detect_harness(&c));
        if harness == Harness::Other
            && procinfo::ancestors(me).contains(&p)
            && let Some(found) = find_harness(me).filter(|(h, k)| usable(h) && *k == Harness::Kiro)
        {
            return Some(found);
        }
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

    /// A harness-ancestor lookup that finds none.
    fn none(_: u32) -> Option<(ProcId, Harness)> {
        None
    }

    /// A harness-ancestor lookup that finds `h` (on our own process).
    fn under(h: Harness) -> impl Fn(u32) -> Option<(ProcId, Harness)> {
        let me = procinfo::proc_id(std::process::id()).unwrap();
        move |_| Some((me, h))
    }

    #[test]
    fn identity_address_env_gives_address() {
        let me = std::process::id();
        assert_eq!(
            caller_from(false, env(&[(ENV_ADDRESS, "x")]), me, none),
            Caller::Address {
                address: "x".into()
            }
        );
        // It beats AGENTMAIL_HUMAN, but not an explicit --as human.
        let both = env(&[(ENV_ADDRESS, "x"), (ENV_HUMAN, "1")]);
        assert_eq!(
            caller_from(false, &both, me, none),
            Caller::Address {
                address: "x".into()
            }
        );
        assert_eq!(caller_from(true, &both, me, none), Caller::Human);
    }

    #[test]
    fn identity_human_env_or_flag_gives_human() {
        let me = std::process::id();
        assert_eq!(
            caller_from(false, env(&[(ENV_HUMAN, "1")]), me, none),
            Caller::Human
        );
        assert_eq!(caller_from(true, env(&[]), me, none), Caller::Human);
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
            match caller_from(false, vars, me, none) {
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
            caller_from(false, &session, me, none),
            Caller::Session {
                session: opencode("s1")
            }
        );
        // An explicit address, the human env and --as human all beat it.
        let addr = env(&[(ENV_OPENCODE_SESSION, "s1"), (ENV_ADDRESS, "x")]);
        assert_eq!(
            caller_from(false, &addr, me, none),
            Caller::Address {
                address: "x".into()
            }
        );
        let human = env(&[(ENV_OPENCODE_SESSION, "s1"), (ENV_HUMAN, "1")]);
        assert_eq!(caller_from(false, &human, me, none), Caller::Human);
        assert_eq!(caller_from(true, &session, me, none), Caller::Human);
    }

    #[test]
    fn identity_cli_caller_session_under_its_own_harness() {
        let me = std::process::id();
        let session = env(&[(ENV_OPENCODE_SESSION, "s1")]);
        assert_eq!(
            caller_from(false, &session, me, under(Harness::Opencode)),
            Caller::Session {
                session: opencode("s1")
            }
        );
    }

    #[test]
    fn identity_cli_caller_ignores_inherited_session() {
        // `claude` started from an opencode bash tool inherits
        // OPENCODE_SESSION_ID; its own commands are not that session.
        let me = std::process::id();
        let inherited = env(&[(ENV_OPENCODE_SESSION, "outer")]);
        match caller_from(false, &inherited, me, under(Harness::ClaudeCode)) {
            Caller::Procs { procs } => assert_eq!(procs, procinfo::ancestors(me)),
            other => panic!("expected Procs, got {other:?}"),
        }
        // kiro started from opencode: the kiro session is the one that
        // counts, and kiro resolves by process.
        let both = env(&[(ENV_OPENCODE_SESSION, "outer"), (ENV_KIRO_SESSION, "k1")]);
        assert!(matches!(
            caller_from(false, &both, me, under(Harness::Kiro)),
            Caller::Procs { .. }
        ));
    }

    #[test]
    fn identity_cli_caller_kiro_session_resolves_by_process() {
        // kiro's MCP server registers by process alone (it gets no session
        // id), so the CLI in kiro's shell must resolve by process too.
        let me = std::process::id();
        let kiro = env(&[(ENV_KIRO_SESSION, "k1")]);
        assert!(matches!(
            caller_from(false, &kiro, me, under(Harness::Kiro)),
            Caller::Procs { .. }
        ));
        assert!(matches!(
            caller_from(false, &kiro, me, none),
            Caller::Procs { .. }
        ));
    }

    #[test]
    fn identity_session_for_checks_the_harness_ancestor() {
        let kiro = |id: &str| SessionKey {
            harness: Harness::Kiro,
            id: id.into(),
        };
        let both = env(&[(ENV_OPENCODE_SESSION, "o1"), (ENV_KIRO_SESSION, "k1")]);
        assert_eq!(session_for(&both, None), Some(opencode("o1")));
        assert_eq!(
            session_for(&both, Some(Harness::Other)),
            Some(opencode("o1"))
        );
        assert_eq!(
            session_for(&both, Some(Harness::Opencode)),
            Some(opencode("o1"))
        );
        assert_eq!(session_for(&both, Some(Harness::Kiro)), Some(kiro("k1")));
        assert_eq!(session_for(&both, Some(Harness::ClaudeCode)), None);
        let outer = env(&[(ENV_OPENCODE_SESSION, "o1")]);
        assert_eq!(session_for(&outer, Some(Harness::Kiro)), None);
        // The lookup runs only when a session variable is set.
        let me = std::process::id();
        let panics = |_: u32| -> Option<(ProcId, Harness)> { panic!("looked up") };
        assert_eq!(session_in(env(&[]), me, panics), None);
        assert_eq!(session_in(&outer, me, under(Harness::ClaudeCode)), None);
    }

    #[test]
    fn identity_cli_caller_reads_the_real_environment() {
        let expected = caller_from(
            false,
            |k| std::env::var_os(k),
            std::process::id(),
            procinfo::find_harness_ancestor,
        );
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
    fn identity_register_proc_explicit_ancestor_ignores_other_harnesses() {
        // An unknown MCP client run from a Claude Code or opencode shell:
        // its server's parent (the shell here) sits levels below that
        // harness, and still binds itself.
        let (child, grandchild) = spawn_grandchild();
        let shell = child.id();
        let found: Vec<_> = [Harness::ClaudeCode, Harness::Opencode]
            .into_iter()
            .map(|h| register_proc_with(grandchild, Some(shell), under(h)))
            .collect();
        let shell_id = procinfo::proc_id(shell);
        reap(child, grandchild);
        for f in found {
            assert_eq!(f, shell_id.map(|p| (p, Harness::Other)));
        }
    }

    #[test]
    fn identity_register_proc_explicit_harness_parent_binds_itself() {
        // A parent that is itself a harness process binds itself, even with
        // a kiro further up.
        use std::io::BufRead;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("opencode");
        std::os::unix::fs::symlink("/bin/sh", &fake).unwrap();
        let mut child = std::process::Command::new(&fake)
            .arg("-c")
            .arg("sleep 5 & echo $!; wait")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let grandchild: u32 = line.trim().parse().unwrap();
        let found = register_proc_with(grandchild, Some(child.id()), under(Harness::Kiro));
        let parent = procinfo::proc_id(child.id());
        reap(child, grandchild);
        assert_eq!(found, parent.map(|p| (p, Harness::Opencode)));
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
