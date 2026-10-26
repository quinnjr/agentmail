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
/// harness ancestor at all there is no session either: a process detached
/// from its harness (a tmux server, `setsid` or `nohup` job started from an
/// opencode bash tool, reparented to init) still carries the variable, but
/// is no longer part of that session. A real opencode session always has
/// its `opencode` process above its shell and MCP children.
///
/// A value the harness process carries itself (`environ`, its environment
/// as it was started) does not count either: it was inherited from an
/// outer session (opencode started from another opencode's bash tool), and
/// the harness passes it on to everything it starts, not only its shell
/// commands. This is the rule [`launched_from_harness_shell`] applies.
pub fn session_for(
    var: impl Fn(&str) -> Option<OsString>,
    nearest: Option<(ProcId, Harness)>,
    environ: impl Fn(u32, &str) -> Option<OsString>,
) -> Option<SessionKey> {
    let (proc, h) = nearest?;
    let (name, harness) = SESSION_VARS.into_iter().find(|(_, vh)| *vh == h)?;
    let session = read_session(&var, name, harness)?;
    let inherited = environ(proc.pid, name).is_some_and(|v| v == session.id.as_str());
    (!inherited).then_some(session)
}

/// [`session_for`] this process: its real environment and harness ancestor.
pub fn env_session() -> Option<SessionKey> {
    session_in(
        |k| std::env::var_os(k),
        std::process::id(),
        procinfo::find_harness_ancestor,
        procinfo::env_var,
    )
}

/// [`session_for`] process `pid`, looking up its harness ancestor only when
/// a session variable is set.
fn session_in(
    var: impl Fn(&str) -> Option<OsString>,
    pid: u32,
    find_harness: impl Fn(u32) -> Option<(ProcId, Harness)>,
    environ: impl Fn(u32, &str) -> Option<OsString>,
) -> Option<SessionKey> {
    session_from_env(&var)?;
    session_for(var, find_harness(pid), environ)
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
        procinfo::env_var,
    )
}

/// [`cli_caller`] with the environment, own pid, harness-ancestor lookup
/// and other processes' environments injected.
fn caller_from(
    as_human: bool,
    var: impl Fn(&str) -> Option<OsString>,
    pid: u32,
    find_harness: impl Fn(u32) -> Option<(ProcId, Harness)>,
    environ: impl Fn(u32, &str) -> Option<OsString>,
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
        session_in(&var, pid, find_harness, environ).filter(|k| k.harness.shares_process())
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
///    its parent) and is not itself a known harness process, whose own
///    nearest harness ancestor is a kiro or opencode process: that harness
///    process. kiro V3 runs its MCP servers from a `node` helper (below
///    `kiro-cli-chat`), and binding the helper would record
///    [`Harness::Other`]; opencode may start its server through a wrapper
///    (`sh -c`), and a session-bound opencode agent's process is the
///    opencode process itself, whichever path registered it. The harness
///    must sit above `explicit_pid`: a pid that names a process above it
///    (say the terminal that launched it) is bound exactly. The MCP server
///    ([`mcp_bind`]) narrows this further: see there.
///
///    This narrows the plan's rule (prefer the harness ancestor for every
///    explicit pid) on purpose, for Claude Code: an unknown MCP client run
///    from a Claude Code shell must bind itself, not the Claude Code it
///    happens to run under, or it would take over the binding of the agent
///    Claude Code itself registered. The cost: a Claude Code MCP server
///    started through a wrapper that does not exec (`sh -c 'agentmail
///    mcp'`) binds the wrapper, so CLI calls and hooks from Claude Code's
///    shell (which find the agent through Claude Code's process) do not
///    find it. Claude Code configs should run `agentmail mcp` directly, or
///    `exec` it from a wrapper.
/// 2. Any other `explicit_pid` (`register --pid`, or a parent that is a
///    harness, has no kiro or opencode above it, or sits above it): exactly
///    that process, with the harness its command name suggests.
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
    register_proc_with(me, explicit_pid, procinfo::find_harness_ancestor, |_| true)
}

/// The process the MCP server's `register` binds to, given its `parent`:
/// [`register_proc`], except that rule 1 (an explicit pid redirected to the
/// kiro or opencode process above it) applies only when that harness
/// started the server itself, not when the server belongs to some other
/// MCP client run from the harness's shell
/// ([`launched_from_harness_shell`]). Such a client (another agent tool, an
/// MCP inspector) binds its own process, as under Claude Code: redirected,
/// it would bind kiro-cli and take over the agent kiro's own server
/// registered, or bind opencode and be refused as a sessionless call on
/// opencode's shared server.
pub fn mcp_bind(parent: Option<u32>) -> Option<(ProcId, Harness)> {
    let me = std::process::id();
    register_proc_with(
        me,
        parent,
        procinfo::find_harness_ancestor,
        started_by_harness(me),
    )
}

/// [`mcp_bind`]'s rule-1 filter for process `me`: the harness found may
/// take the binding only when it started `me` itself.
fn started_by_harness(me: u32) -> impl Fn(&(ProcId, Harness)) -> bool {
    move |(p, h)| !launched_from_harness_shell(me, p, *h, procinfo::env_var)
}

/// Whether process `me` was started from a shell command of `harness_proc`
/// (a `harness` process above `me`), rather than by the harness itself.
///
/// kiro and opencode set their session variable (`KIRO_SESSION_ID`,
/// `OPENCODE_SESSION_ID`) for the commands their shell tool runs, but not
/// for their MCP servers (see docs/harness-notes.md). So a process between
/// `me` (inclusive) and the harness whose environment, as it was started,
/// carries that variable came from the harness's shell. An MCP client that
/// passes its server a cleaned environment does not hide this: the client
/// process itself still carries the variable. A value the harness process
/// carries itself does not count: it was inherited from an outer session
/// (kiro run from a kiro shell), and the harness passes it on to everything
/// it starts.
fn launched_from_harness_shell(
    me: u32,
    harness_proc: &ProcId,
    harness: Harness,
    environ: impl Fn(u32, &str) -> Option<OsString>,
) -> bool {
    let Some((name, _)) = SESSION_VARS.into_iter().find(|(_, h)| *h == harness) else {
        return false;
    };
    let inherited = environ(harness_proc.pid, name);
    std::iter::once(me)
        .chain(procinfo::ancestors(me).into_iter().map(|p| p.pid))
        .take_while(|pid| *pid != harness_proc.pid)
        .filter_map(|pid| environ(pid, name).filter(|v| !v.is_empty()))
        .any(|v| Some(&v) != inherited.as_ref())
}

/// [`register_proc_for`] with the harness-ancestor lookup injected, so the
/// grandparent fallback can be tested even when the tests themselves run
/// under a harness, and with `redirect` deciding whether rule 1 may hand an
/// explicit pid to the kiro or opencode process found above it.
fn register_proc_with(
    me: u32,
    explicit_pid: Option<u32>,
    find_harness: impl Fn(u32) -> Option<(ProcId, Harness)>,
    redirect: impl Fn(&(ProcId, Harness)) -> bool,
) -> Option<(ProcId, Harness)> {
    binding_with(me, explicit_pid, find_harness, redirect, |p| p.pid > 1)
}

/// The harness of the process [`register_proc`] picks, PID 1 included.
/// A harness at PID 1 (opencode as a container's entry point) cannot be
/// bound to, but whether its process is shared between sessions must
/// still be known, so a sessionless `register` under it is refused.
pub fn register_harness(explicit_pid: Option<u32>) -> Harness {
    binding_with(
        std::process::id(),
        explicit_pid,
        procinfo::find_harness_ancestor,
        |_| true,
        |_| true,
    )
    .map_or(Harness::Other, |(_, h)| h)
}

/// The harness of the process [`mcp_bind`] picks, PID 1 included (see
/// [`register_harness`]).
pub fn mcp_harness(parent: Option<u32>) -> Harness {
    let me = std::process::id();
    binding_with(
        me,
        parent,
        procinfo::find_harness_ancestor,
        started_by_harness(me),
        |_| true,
    )
    .map_or(Harness::Other, |(_, h)| h)
}

/// [`register_proc_with`] with `usable` deciding which processes may be
/// picked.
fn binding_with(
    me: u32,
    explicit_pid: Option<u32>,
    find_harness: impl Fn(u32) -> Option<(ProcId, Harness)>,
    redirect: impl Fn(&(ProcId, Harness)) -> bool,
    usable: impl Fn(&ProcId) -> bool,
) -> Option<(ProcId, Harness)> {
    if let Some(pid) = explicit_pid {
        let p = procinfo::proc_id(pid).filter(&usable)?;
        let harness = procinfo::comm(pid).map_or(Harness::Other, |c| procinfo::detect_harness(&c));
        if harness == Harness::Other
            && procinfo::ancestors(me).contains(&p)
            && let Some(found) = find_harness(pid).filter(|(h, k)| {
                usable(h) && (*k == Harness::Kiro || k.shares_process()) && redirect(&(*h, *k))
            })
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
        .filter(&usable)
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

    /// [`register_proc_with`] as the CLI runs it: rule 1 always redirects.
    fn redirecting(
        me: u32,
        explicit_pid: Option<u32>,
        find_harness: impl Fn(u32) -> Option<(ProcId, Harness)>,
    ) -> Option<(ProcId, Harness)> {
        register_proc_with(me, explicit_pid, find_harness, |_| true)
    }

    /// Other processes' environments, all empty.
    fn no_environ(_: u32, _: &str) -> Option<OsString> {
        None
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
            caller_from(false, env(&[(ENV_ADDRESS, "x")]), me, none, no_environ),
            Caller::Address {
                address: "x".into()
            }
        );
        // It beats AGENTMAIL_HUMAN, but not an explicit --as human.
        let both = env(&[(ENV_ADDRESS, "x"), (ENV_HUMAN, "1")]);
        assert_eq!(
            caller_from(false, &both, me, none, no_environ),
            Caller::Address {
                address: "x".into()
            }
        );
        assert_eq!(
            caller_from(true, &both, me, none, no_environ),
            Caller::Human
        );
    }

    #[test]
    fn identity_human_env_or_flag_gives_human() {
        let me = std::process::id();
        assert_eq!(
            caller_from(false, env(&[(ENV_HUMAN, "1")]), me, none, no_environ),
            Caller::Human
        );
        assert_eq!(
            caller_from(true, env(&[]), me, none, no_environ),
            Caller::Human
        );
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
            match caller_from(false, vars, me, none, no_environ) {
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
        let opencode_harness = under(Harness::Opencode);
        let session = env(&[(ENV_OPENCODE_SESSION, "s1")]);
        assert_eq!(
            caller_from(false, &session, me, &opencode_harness, no_environ),
            Caller::Session {
                session: opencode("s1")
            }
        );
        // An explicit address, the human env and --as human all beat it.
        let addr = env(&[(ENV_OPENCODE_SESSION, "s1"), (ENV_ADDRESS, "x")]);
        assert_eq!(
            caller_from(false, &addr, me, &opencode_harness, no_environ),
            Caller::Address {
                address: "x".into()
            }
        );
        let human = env(&[(ENV_OPENCODE_SESSION, "s1"), (ENV_HUMAN, "1")]);
        assert_eq!(
            caller_from(false, &human, me, &opencode_harness, no_environ),
            Caller::Human
        );
        assert_eq!(
            caller_from(true, &session, me, &opencode_harness, no_environ),
            Caller::Human
        );
    }

    #[test]
    fn identity_cli_caller_ignores_session_without_harness_ancestor() {
        // A tmux server or `setsid` job started from an opencode bash tool
        // keeps OPENCODE_SESSION_ID after it is reparented away from
        // opencode; its shells are not that session.
        let me = std::process::id();
        let detached = env(&[(ENV_OPENCODE_SESSION, "s1")]);
        match caller_from(false, &detached, me, none, no_environ) {
            Caller::Procs { procs } => assert_eq!(procs, procinfo::ancestors(me)),
            other => panic!("expected Procs, got {other:?}"),
        }
        assert_eq!(session_in(&detached, me, none, no_environ), None);
    }

    #[test]
    fn identity_cli_caller_session_under_its_own_harness() {
        let me = std::process::id();
        let session = env(&[(ENV_OPENCODE_SESSION, "s1")]);
        assert_eq!(
            caller_from(false, &session, me, under(Harness::Opencode), no_environ),
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
        match caller_from(
            false,
            &inherited,
            me,
            under(Harness::ClaudeCode),
            no_environ,
        ) {
            Caller::Procs { procs } => assert_eq!(procs, procinfo::ancestors(me)),
            other => panic!("expected Procs, got {other:?}"),
        }
        // kiro started from opencode: the kiro session is the one that
        // counts, and kiro resolves by process.
        let both = env(&[(ENV_OPENCODE_SESSION, "outer"), (ENV_KIRO_SESSION, "k1")]);
        assert!(matches!(
            caller_from(false, &both, me, under(Harness::Kiro), no_environ),
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
            caller_from(false, &kiro, me, under(Harness::Kiro), no_environ),
            Caller::Procs { .. }
        ));
        assert!(matches!(
            caller_from(false, &kiro, me, none, no_environ),
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
        let p = procinfo::proc_id(std::process::id()).unwrap();
        let at = |h: Harness| Some((p, h));
        // No harness ancestor: the variables are inherited, not ours.
        assert_eq!(session_for(&both, None, no_environ), None);
        assert_eq!(session_for(&both, at(Harness::Other), no_environ), None);
        assert_eq!(
            session_for(&both, at(Harness::Opencode), no_environ),
            Some(opencode("o1"))
        );
        assert_eq!(
            session_for(&both, at(Harness::Kiro), no_environ),
            Some(kiro("k1"))
        );
        assert_eq!(
            session_for(&both, at(Harness::ClaudeCode), no_environ),
            None
        );
        let outer = env(&[(ENV_OPENCODE_SESSION, "o1")]);
        assert_eq!(session_for(&outer, at(Harness::Kiro), no_environ), None);
        // The lookup runs only when a session variable is set.
        let me = std::process::id();
        let panics = |_: u32| -> Option<(ProcId, Harness)> { panic!("looked up") };
        assert_eq!(session_in(env(&[]), me, panics, no_environ), None);
        assert_eq!(
            session_in(&outer, me, under(Harness::ClaudeCode), no_environ),
            None
        );
    }

    #[test]
    fn identity_session_for_ignores_a_value_the_harness_carries() {
        // opencode B started from opencode A's bash tool carries A's
        // OPENCODE_SESSION_ID and passes it on to everything it starts
        // (its plugin's Bun.spawn, an LSP): that is not one of B's
        // sessions. B's own shell commands carry a different value.
        let p = procinfo::proc_id(std::process::id()).unwrap();
        let carries = |value: &'static str| {
            move |pid: u32, name: &str| -> Option<OsString> {
                (pid == p.pid && name == ENV_OPENCODE_SESSION).then(|| value.into())
            }
        };
        let outer = env(&[(ENV_OPENCODE_SESSION, "outer")]);
        let inner = env(&[(ENV_OPENCODE_SESSION, "inner")]);
        let at = Some((p, Harness::Opencode));
        assert_eq!(session_for(&outer, at, carries("outer")), None);
        assert_eq!(
            session_for(&inner, at, carries("outer")),
            Some(opencode("inner"))
        );
        // Only the harness process's own value counts: another process
        // carrying it changes nothing.
        let elsewhere =
            |pid: u32, _: &str| -> Option<OsString> { (pid != p.pid).then(|| "outer".into()) };
        assert_eq!(session_for(&outer, at, elsewhere), Some(opencode("outer")));
        // The CLI caller follows it.
        let me = std::process::id();
        let opencode_harness = |_: u32| at;
        match caller_from(false, &outer, me, opencode_harness, carries("outer")) {
            Caller::Procs { procs } => assert_eq!(procs, procinfo::ancestors(me)),
            other => panic!("expected Procs, got {other:?}"),
        }
        assert_eq!(
            caller_from(false, &inner, me, opencode_harness, carries("outer")),
            Caller::Session {
                session: opencode("inner")
            }
        );
    }

    #[test]
    fn identity_cli_caller_reads_the_real_environment() {
        let expected = caller_from(
            false,
            |k| std::env::var_os(k),
            std::process::id(),
            procinfo::find_harness_ancestor,
            procinfo::env_var,
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
        let via_parent = redirecting(grandchild, Some(shell), harness);
        // No harness above: the named parent itself.
        let no_harness = redirecting(grandchild, Some(shell), |_| None);
        // A pid that is not our ancestor binds exactly, harness or not.
        let not_ancestor = redirecting(me.pid, Some(grandchild), harness);
        let shell_id = procinfo::proc_id(shell);
        let sleep_id = procinfo::proc_id(grandchild);
        reap(child, grandchild);
        assert_eq!(via_parent, Some((me, Harness::Kiro)));
        assert_eq!(no_harness, shell_id.map(|p| (p, Harness::Other)));
        assert_eq!(not_ancestor, sleep_id.map(|p| (p, Harness::Other)));
    }

    #[test]
    fn identity_register_proc_explicit_pid_above_kiro_binds_itself() {
        // sleep -> sh -> test process, with the shell standing in for
        // kiro-cli. The sleep names the test process, which sits above the
        // kiro: it binds exactly that process, not the kiro below it.
        let (child, grandchild) = spawn_grandchild();
        let shell = child.id();
        let kiro_above = |x: u32| {
            procinfo::ancestors(x)
                .into_iter()
                .find(|p| p.pid == shell)
                .map(|p| (p, Harness::Kiro))
        };
        let me = procinfo::proc_id(std::process::id()).unwrap();
        let above = redirecting(grandchild, Some(me.pid), kiro_above);
        // The lookup really does see the kiro from the sleep.
        let seen = kiro_above(grandchild).map(|(p, _)| p.pid);
        reap(child, grandchild);
        assert_eq!(seen, Some(shell));
        assert_eq!(above, Some((me, Harness::Other)));
    }

    #[test]
    fn identity_register_proc_explicit_ancestor_ignores_claude_code() {
        // An unknown MCP client run from a Claude Code shell: its server's
        // parent (the shell here) sits levels below Claude Code, and still
        // binds itself.
        let (child, grandchild) = spawn_grandchild();
        let shell = child.id();
        let found = redirecting(grandchild, Some(shell), under(Harness::ClaudeCode));
        let shell_id = procinfo::proc_id(shell);
        reap(child, grandchild);
        assert_eq!(found, shell_id.map(|p| (p, Harness::Other)));
    }

    #[test]
    fn identity_register_proc_explicit_ancestor_binds_opencode() {
        // opencode running its MCP server through a wrapper: the wrapper
        // (the shell here) is not bound, the opencode above it is.
        let (child, grandchild) = spawn_grandchild();
        let me = procinfo::proc_id(std::process::id()).unwrap();
        let found = redirecting(grandchild, Some(child.id()), under(Harness::Opencode));
        reap(child, grandchild);
        assert_eq!(found, Some((me, Harness::Opencode)));
    }

    #[test]
    fn identity_launched_from_harness_shell_reads_the_chain() {
        // sleep -> sh -> test process, the test process standing in for the
        // harness. `vars` gives each pid's environment.
        let (child, grandchild) = spawn_grandchild();
        let shell = child.id();
        let me = procinfo::proc_id(std::process::id()).unwrap();
        let check = |harness: Harness, vars: &[(u32, &str, &str)]| {
            let vars: Vec<(u32, String, OsString)> = vars
                .iter()
                .map(|(p, k, v)| (*p, k.to_string(), OsString::from(v)))
                .collect();
            launched_from_harness_shell(grandchild, &me, harness, |pid, name| {
                vars.iter()
                    .find(|(p, k, _)| *p == pid && k == name)
                    .map(|(_, _, v)| v.clone())
            })
        };
        let kiro = Harness::Kiro;
        // (harness, each pid's variables, launched from its shell?)
        type Case<'a> = (Harness, Vec<(u32, &'a str, &'a str)>, bool);
        let cases: Vec<Case> = vec![
            // Nothing set: the harness started the chain.
            (kiro, vec![], false),
            // The harness's shell, or the server itself, carries it.
            (kiro, vec![(shell, ENV_KIRO_SESSION, "k1")], true),
            (kiro, vec![(grandchild, ENV_KIRO_SESSION, "k1")], true),
            // Empty does not count.
            (kiro, vec![(shell, ENV_KIRO_SESSION, "")], false),
            // Only the variable of the harness found counts.
            (kiro, vec![(shell, ENV_OPENCODE_SESSION, "o1")], false),
            (
                Harness::Opencode,
                vec![(shell, ENV_OPENCODE_SESSION, "o1")],
                true,
            ),
            // Inherited by the harness itself (kiro run from a kiro shell).
            (
                kiro,
                vec![
                    (me.pid, ENV_KIRO_SESSION, "outer"),
                    (shell, ENV_KIRO_SESSION, "outer"),
                ],
                false,
            ),
            (
                kiro,
                vec![
                    (me.pid, ENV_KIRO_SESSION, "outer"),
                    (shell, ENV_KIRO_SESSION, "inner"),
                ],
                true,
            ),
            // A harness with no session variable never counts.
            (
                Harness::ClaudeCode,
                vec![(shell, ENV_KIRO_SESSION, "k1")],
                false,
            ),
        ];
        let got: Vec<bool> = cases.iter().map(|(h, v, _)| check(*h, v)).collect();
        reap(child, grandchild);
        let want: Vec<bool> = cases.iter().map(|(_, _, w)| *w).collect();
        assert_eq!(got, want);
    }

    /// Spawn `sh -c 'sleep 5 & echo $!; wait'` with `vars` set and return
    /// the shell child and the PID of its `sleep` grandchild.
    fn spawn_grandchild_with(vars: &[(&str, &str)]) -> (std::process::Child, u32) {
        use std::io::BufRead;
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 5 & echo $!; wait")
            .envs(vars.iter().copied())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        (child, line.trim().parse().unwrap())
    }

    #[test]
    fn identity_mcp_bind_skips_harness_redirect_from_its_shell() {
        // kiro-cli (the test process) -> sh with KIRO_SESSION_ID set (kiro's
        // shell tool running another MCP client) -> that client's server
        // (the sleep). The server binds the client's process, not kiro-cli,
        // so it cannot take over the agent kiro's own server registered.
        let me = procinfo::proc_id(std::process::id()).unwrap();
        let value = format!("agentmail-test-{}", me.pid);
        for (harness, var) in [
            (Harness::Kiro, ENV_KIRO_SESSION),
            (Harness::Opencode, ENV_OPENCODE_SESSION),
        ] {
            let (child, sleep) = spawn_grandchild_with(&[(var, &value)]);
            let shell = procinfo::proc_id(child.id());
            let from_shell = register_proc_with(
                sleep,
                Some(child.id()),
                under(harness),
                started_by_harness(sleep),
            );
            // The CLI's `register --pid $$` in that shell still redirects.
            let cli = redirecting(sleep, Some(child.id()), under(harness));
            reap(child, sleep);
            assert_eq!(
                from_shell,
                shell.map(|p| (p, Harness::Other)),
                "{harness:?}"
            );
            assert_eq!(cli, Some((me, harness)), "{harness:?}");

            // Started by the harness itself (no variable): redirected.
            let (child, sleep) = spawn_grandchild_with(&[]);
            let by_harness = register_proc_with(
                sleep,
                Some(child.id()),
                under(harness),
                started_by_harness(sleep),
            );
            reap(child, sleep);
            // Any value the test process itself carries is inherited by
            // the whole chain, and so ignored.
            assert_eq!(by_harness, Some((me, harness)), "{harness:?}");
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
        let found = redirecting(grandchild, Some(child.id()), under(Harness::Kiro));
        let parent = procinfo::proc_id(child.id());
        reap(child, grandchild);
        assert_eq!(found, parent.map(|p| (p, Harness::Opencode)));
    }

    #[test]
    fn identity_register_proc_falls_back_to_grandparent() {
        // With no harness ancestor, the sleep binds to its grandparent: the
        // test process itself (sleep -> sh -> test process).
        let (child, grandchild) = spawn_grandchild();
        let found = redirecting(grandchild, None, |_| None);
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
        let found = redirecting(grandchild, None, |_| init.map(|p| (p, Harness::ClaudeCode)));
        reap(child, grandchild);
        let me = std::process::id();
        assert_eq!(
            found,
            Some((procinfo::proc_id(me).unwrap(), Harness::Other))
        );
    }

    #[test]
    fn identity_harness_lookup_sees_a_harness_at_pid_1() {
        // opencode at PID 1 (a container's entry point) cannot be bound to,
        // but its harness is still known, directly or through a wrapper
        // shell below it, so its shared process is recognized.
        let (child, grandchild) = spawn_grandchild();
        let init = procinfo::proc_id(1);
        let at_init = |_| init.map(|p| (p, Harness::Opencode));
        let all = |_: &ProcId| true;
        let found = binding_with(grandchild, None, at_init, |_| true, all);
        let via_shell = binding_with(grandchild, Some(child.id()), at_init, |_| true, all);
        let bound = redirecting(grandchild, None, at_init);
        reap(child, grandchild);
        assert_eq!(found, init.map(|p| (p, Harness::Opencode)));
        assert_eq!(via_shell, init.map(|p| (p, Harness::Opencode)));
        assert_ne!(bound.map(|(p, _)| p.pid), Some(1));
        assert_ne!(bound.map(|(_, h)| h), Some(Harness::Opencode));
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
        assert_eq!(redirecting(x, None, |_| None), None);
    }
}
