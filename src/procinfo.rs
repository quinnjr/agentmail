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
/// exits during the walk.
pub fn ancestors(pid: u32) -> Vec<ProcId> {
    let mut chain = Vec::new();
    let mut cur = pid;
    while cur > 1 && chain.len() < MAX_ANCESTRY_DEPTH {
        let Some(ppid) = parent_pid(cur) else { break };
        if ppid == 0 {
            break;
        }
        let Some(parent) = proc_id(ppid) else { break };
        chain.push(parent);
        cur = ppid;
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

        #[test]
        fn procinfo_harness_ancestor_of_claude_like_parent() {
            // A child running under a process named "claude" finds it.
            let dir = tempfile::tempdir().unwrap();
            let fake = dir.path().join("claude");
            std::os::unix::fs::symlink("/bin/sh", &fake).unwrap();
            let mut out = Command::new(&fake)
                .arg("-c")
                .arg("sleep 5 & echo $!; wait")
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            use std::io::BufRead;
            std::io::BufReader::new(out.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            let grandchild: u32 = line.trim().parse().unwrap();
            let found = find_harness_ancestor(grandchild);
            let _ = out.kill();
            let _ = out.wait();
            let (p, h) = found.expect("harness ancestor");
            assert_eq!(h, Harness::ClaudeCode);
            assert_eq!(p.pid, out.id());
        }
    }
}
