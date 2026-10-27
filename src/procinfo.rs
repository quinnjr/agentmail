//! Process identity, liveness and ancestry.

use crate::model::{Harness, ProcId};

/// Upper bound on the ancestry walk, as a guard against a malformed or cyclic chain.
const MAX_ANCESTRY_DEPTH: usize = 1024;

/// The [`ProcId`] of a live process: its PID plus its start time
/// (`/proc/<pid>/stat` field 22, in clock ticks since boot).
pub fn proc_id(pid: u32) -> Option<ProcId> {
    let (_, start_time) = read_stat(pid)?;
    Some(ProcId { pid, start_time })
}

/// Whether the exact process `p` is still running. A different process that
/// reused the PID has a different start time and so does not count.
pub fn is_alive(p: &ProcId) -> bool {
    proc_id(p.pid) == Some(*p)
}

/// The parent PID of `pid` (`/proc/<pid>/stat` field 4).
pub fn parent_pid(pid: u32) -> Option<u32> {
    read_stat(pid).map(|(ppid, _)| ppid)
}

/// The ancestors of `pid`, nearest (the parent) first. Excludes `pid` itself
/// and ends at PID 1, or earlier if a parent is outside our PID namespace or
/// exits during the walk (including when its PID is reused).
pub fn ancestors(pid: u32) -> Vec<ProcId> {
    walk_ancestors(pid, read_stat)
}

/// The ancestry walk behind [`ancestors`], with the `/proc/<pid>/stat` reader
/// injected so the walk can be tested against a fake process table.
fn walk_ancestors(pid: u32, read: impl Fn(u32) -> Option<(u32, u64)>) -> Vec<ProcId> {
    let mut chain = Vec::new();
    let Some((mut ppid, mut child_start)) = read(pid) else {
        return chain;
    };
    while ppid > 0 && chain.len() < MAX_ANCESTRY_DEPTH {
        let Some((next_ppid, start_time)) = read(ppid) else {
            break;
        };
        // PID-reuse guard: a real parent cannot start after its child, so a
        // later start time means the parent exited and its PID was reused.
        if start_time > child_start {
            break;
        }
        chain.push(ProcId {
            pid: ppid,
            start_time,
        });
        child_start = start_time;
        ppid = next_ppid;
    }
    chain
}

/// The command name of `pid` (`/proc/<pid>/comm`), without the trailing newline.
pub fn comm(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read(format!("/proc/{pid}/comm")).ok()?;
        let s = String::from_utf8_lossy(&raw);
        Some(s.trim_end_matches('\n').to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// The command-line arguments of `pid` (`/proc/<pid>/cmdline`), the
/// program first. `None` when unreadable.
pub fn args(pid: u32) -> Option<Vec<std::ffi::OsString>> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let raw = raw.strip_suffix(&[0]).unwrap_or(&raw);
        Some(
            raw.split(|b| *b == 0)
                .map(|a| std::ffi::OsStr::from_bytes(a).to_os_string())
                .collect(),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Environment variable `name` of process `pid`, as the process was started
/// (`/proc/<pid>/environ`). `None` when unset or unreadable.
pub fn env_var(pid: u32, name: &str) -> Option<std::ffi::OsString> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        raw.split(|b| *b == 0).find_map(|entry| {
            let value = entry.strip_prefix(name.as_bytes())?.strip_prefix(b"=")?;
            Some(std::ffi::OsStr::from_bytes(value).to_os_string())
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, name);
        None
    }
}

/// Map a process command name to the harness it belongs to.
pub fn detect_harness(comm: &str) -> Harness {
    match comm {
        "claude" => Harness::ClaudeCode,
        "opencode" => Harness::Opencode,
        "kiro-cli" | "kiro" => Harness::Kiro,
        _ => Harness::Other,
    }
}

/// The nearest ancestor of `pid` that is a known harness process.
pub fn find_harness_ancestor(pid: u32) -> Option<(ProcId, Harness)> {
    ancestors(pid).into_iter().find_map(|p| {
        let harness = detect_harness(&comm(p.pid)?);
        // Guard against the PID being reused between the walk and the comm read.
        (harness != Harness::Other && is_alive(&p)).then_some((p, harness))
    })
}

/// Read `/proc/<pid>/stat` and return `(ppid, start_time)`.
fn read_stat(pid: u32) -> Option<(u32, u64)> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        parse_stat(&String::from_utf8_lossy(&raw))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Parse a `/proc/<pid>/stat` line into `(ppid, start_time)`.
///
/// Field 2 (comm) is wrapped in parentheses and may itself contain spaces and
/// parentheses, so parsing starts after the *last* `)`. After it come field 3
/// (state), field 4 (ppid), ... field 22 (starttime).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_stat(stat: &str) -> Option<(u32, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_ascii_whitespace();
    let ppid = fields.nth(1)?.parse().ok()?;
    let start_time = fields.nth(17)?.parse().ok()?;
    Some((ppid, start_time))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn procinfo_parse_stat_handles_parens_and_spaces_in_comm() {
        // Literal form: "123 (we (ird) x) S 45 0 0 ... 0 987654 1 2 3",
        // i.e. fields 5..=21 are "0" and field 22 (starttime) is 987654.
        let mut fields = vec!["S", "45"];
        // fields 5..=21 then starttime (22)
        fields.extend(std::iter::repeat_n("0", 17));
        fields.push("987654");
        fields.extend(["1", "2", "3"]);
        let line = format!("123 (we (ird) x) {}", fields.join(" "));
        assert_eq!(parse_stat(&line), Some((45, 987654)));
    }

    #[test]
    fn procinfo_parse_stat_rejects_garbage() {
        assert_eq!(parse_stat("no paren here"), None);
        assert_eq!(parse_stat("1 (x) S"), None);
        assert_eq!(parse_stat("1 (x) S notanumber"), None);
    }

    #[test]
    fn procinfo_parse_stat_literal_line() {
        let line = "123 (we (ird) x) S 45 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 987654 1 2 3";
        assert_eq!(parse_stat(line), Some((45, 987654)));
    }

    #[test]
    fn procinfo_walk_ancestors_follows_chain_to_init() {
        let table = std::collections::HashMap::from([
            (30, (20, 300)),
            (20, (10, 200)),
            (10, (1, 100)),
            (1, (0, 1)),
        ]);
        let chain = walk_ancestors(30, |p| table.get(&p).copied());
        let pids: Vec<u32> = chain.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![20, 10, 1]);
        assert_eq!(chain[0].start_time, 200);
    }

    #[test]
    fn procinfo_walk_ancestors_stops_at_reused_parent_pid() {
        // PID 10 was reused by a process that started after its "child" 20.
        let table = std::collections::HashMap::from([
            (30, (20, 300)),
            (20, (10, 200)),
            (10, (1, 250)),
            (1, (0, 1)),
        ]);
        let chain = walk_ancestors(30, |p| table.get(&p).copied());
        let pids: Vec<u32> = chain.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![20]);
    }

    #[test]
    fn procinfo_walk_ancestors_stops_at_missing_parent() {
        let table = std::collections::HashMap::from([(30, (20, 300))]);
        assert!(walk_ancestors(30, |p| table.get(&p).copied()).is_empty());
        assert!(walk_ancestors(99, |p| table.get(&p).copied()).is_empty());
    }

    #[test]
    fn procinfo_detect_harness_table() {
        assert_eq!(detect_harness("claude"), Harness::ClaudeCode);
        assert_eq!(detect_harness("opencode"), Harness::Opencode);
        assert_eq!(detect_harness("kiro-cli"), Harness::Kiro);
        assert_eq!(detect_harness("kiro"), Harness::Kiro);
        assert_eq!(detect_harness("bash"), Harness::Other);
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use super::super::*;
        use std::process::Command;

        #[test]
        fn procinfo_self_is_alive() {
            let me = proc_id(std::process::id()).expect("own proc id");
            assert_eq!(me.pid, std::process::id());
            assert!(is_alive(&me));
        }

        #[test]
        fn procinfo_dead_child_is_not_alive() {
            let mut child = Command::new("sleep").arg("30").spawn().unwrap();
            let p = proc_id(child.id()).expect("child proc id");
            assert!(is_alive(&p));
            child.kill().unwrap();
            child.wait().unwrap();
            assert!(!is_alive(&p));
        }

        #[test]
        fn procinfo_pid_reuse_guard() {
            let real = proc_id(std::process::id()).unwrap();
            let fake = ProcId {
                pid: real.pid,
                start_time: real.start_time + 1,
            };
            assert!(!is_alive(&fake));
        }

        #[test]
        fn procinfo_ancestors_start_with_parent() {
            let me = std::process::id();
            let chain = ancestors(me);
            assert!(!chain.is_empty());
            assert_eq!(Some(chain[0].pid), parent_pid(me));
            assert!(chain.iter().all(|p| p.pid != me));
        }

        #[test]
        fn procinfo_env_var_reads_a_child_environment() {
            // The shell prints a line once it runs: until it has exec'd, the
            // forked child still shows the test process's environment.
            use std::io::BufRead;
            let mut child = Command::new("/bin/sh")
                .arg("-c")
                .arg("echo ready; sleep 5")
                .env("AGENTMAIL_TEST_ENV", "v=1")
                .env("AGENTMAIL_TEST_ENV_EMPTY", "")
                .env_remove("AGENTMAIL_TEST_ENV_UNSET")
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            let pid = child.id();
            let set = env_var(pid, "AGENTMAIL_TEST_ENV");
            let empty = env_var(pid, "AGENTMAIL_TEST_ENV_EMPTY");
            let prefix = env_var(pid, "AGENTMAIL_TEST");
            let unset = env_var(pid, "AGENTMAIL_TEST_ENV_UNSET");
            let _ = child.kill();
            let _ = child.wait();
            assert_eq!(set, Some("v=1".into()));
            assert_eq!(empty, Some("".into()));
            assert_eq!(prefix, None);
            assert_eq!(unset, None);
            assert_eq!(env_var(u32::MAX, "PATH"), None);
        }

        #[test]
        fn procinfo_comm_of_self() {
            let c = comm(std::process::id()).expect("own comm");
            assert!(!c.is_empty());
            assert!(!c.ends_with('\n'));
        }

        #[test]
        fn procinfo_missing_pid_is_none() {
            // PIDs never exceed 2^22 on Linux.
            assert_eq!(proc_id(u32::MAX - 1), None);
            assert_eq!(parent_pid(u32::MAX - 1), None);
            assert_eq!(comm(u32::MAX - 1), None);
            assert!(ancestors(u32::MAX - 1).is_empty());
        }

        /// Kills the fake harness and its backgrounded grandchild on drop, so
        /// neither leaks even when the test panics part-way through.
        struct Cleanup {
            child: std::process::Child,
            grandchild: Option<u32>,
        }

        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Some(pid) = self.grandchild {
                    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
                }
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }

        #[test]
        fn procinfo_harness_ancestor_of_claude_like_parent() {
            // A child running under a process named "claude" finds it.
            let dir = tempfile::tempdir().unwrap();
            let fake = dir.path().join("claude");
            std::os::unix::fs::symlink("/bin/sh", &fake).unwrap();
            let mut guard = Cleanup {
                child: Command::new(&fake)
                    .arg("-c")
                    .arg("sleep 5 & echo $!; wait")
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap(),
                grandchild: None,
            };
            let harness_pid = guard.child.id();
            let mut line = String::new();
            use std::io::BufRead;
            std::io::BufReader::new(guard.child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            let grandchild: u32 = line.trim().parse().unwrap();
            guard.grandchild = Some(grandchild);
            let found = find_harness_ancestor(grandchild);
            drop(guard);
            let (p, h) = found.expect("harness ancestor");
            assert_eq!(h, Harness::ClaudeCode);
            assert_eq!(p.pid, harness_pid);
        }
    }
}
