//! kiro-cli V3's hook output: exit codes and plain stdio (see
//! docs/harness-notes.md, "kiro-cli V3 hooks").
//!
//! kiro reads a command hook's result from its exit code: stdout of a
//! `SessionStart` or `UserPromptSubmit` hook that exits 0 is added to the
//! agent's context; exit 2 from `PreToolUse` blocks that one tool call
//! and shows the model stderr in its place; exit 1 from `Stop` continues
//! the turn with stderr as the instruction. Anything else is ignored, and
//! so is `PostToolUse` output, so agentmail installs no PostToolUse hook.
//!
//! - `session-start`, `prompt`: who the agent is and its reminder, on
//!   stdout.
//! - `pre-tool`: urgent mail in full, once (it is marked surfaced), by
//!   blocking the tool call it arrived before; silent otherwise.
//! - All three mark the session busy, which ends a `stop --wake` still
//!   waiting from an earlier turn (kiro does not stop a Stop hook when the
//!   human presses Esc).
//! - `stop`: open questions to the agent continue the turn (exit 1). kiro
//!   runs Stop hooks once per human prompt: the continued turn ends without
//!   another Stop, so a model that cannot answer is never held. Otherwise,
//!   with `--wake` (`install --kiro-wake`) and the agent's `wake` on, it
//!   waits while the agent is idle ([`Daemon::wait`]) and continues the
//!   turn with the reminder (exit 1) when mail comes. While it waits kiro
//!   shows the agent as working and holds what the human types until the
//!   hook ends or the human presses Esc, hence opt-in.
//! - `post-tool`, `wait`: nothing (`stop` does the waiting).

use std::path::Path;

use super::{Daemon, HookArgs, HookEvent, HookOutput, WaitOutcome, text};
use crate::procinfo;

/// kiro's exit code for a `PreToolUse` hook that blocks the tool call.
const EXIT_BLOCK_TOOL: u8 = 2;

/// kiro's exit code for a `Stop` hook that continues the turn.
const EXIT_CONTINUE: u8 = 1;

/// kiro-cli's options before a subcommand that take a value as the next
/// argument (`kiro-cli --help-all`, 2.32.0), so that value is not taken
/// for the subcommand.
const VALUE_OPTIONS: [&str; 3] = ["--agent", "--resume-id", "--repo"];

/// Whether a `kiro-cli` or `kiro-cli-chat` process started with `args`
/// (after the program) runs with nobody typing into it:
/// - `--no-interactive` anywhere: `chat --no-interactive` ends only after
///   its Stop hooks, so a waiting one would hold it open until mail came
///   (seen live);
/// - `--output-format stream-json` (or `=stream-json`) anywhere, which
///   `kiro-cli chat --help` says implies `--no-interactive`;
/// - the subcommand `acp`, where an editor drives the engine (not checked
///   live; a wait there could hold its turn open indefinitely, so it is not
///   risked). Only the subcommand counts: the first argument that is not
///   an option or an option's value, so a prompt or an agent named `acp`
///   in an interactive chat does not.
fn headless_args(args: &[&str]) -> bool {
    let anywhere = args.iter().enumerate().any(|(i, a)| {
        *a == "--no-interactive"
            || *a == "--output-format=stream-json"
            || (*a == "--output-format" && args.get(i + 1) == Some(&"stream-json"))
    });
    let mut rest = args.iter();
    let subcommand = loop {
        match rest.next() {
            Some(a) if VALUE_OPTIONS.contains(a) => {
                rest.next();
            }
            Some(a) if a.starts_with('-') => {}
            other => break other,
        }
    };
    anywhere || subcommand == Some(&"acp")
}

/// Whether the hook runs in a kiro run nobody types into: one of the
/// processes it runs under, up to `kiro-cli`, is `kiro-cli` or
/// `kiro-cli-chat` started with [`headless_args`]. Other processes are not
/// looked at, so a hook command or shell with such an argument changes
/// nothing.
fn headless(d: &Daemon) -> bool {
    d.procs.iter().any(|p| {
        let Some(args) = procinfo::args(p.pid) else {
            return false;
        };
        let is_kiro = args.first().is_some_and(|prog| {
            Path::new(prog)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "kiro-cli" || n == "kiro-cli-chat")
        });
        // An argument that is not UTF-8 is none of the words looked for.
        let args: Vec<&str> = args[1..].iter().map(|a| a.to_str().unwrap_or("")).collect();
        is_kiro && headless_args(&args)
    })
}

/// Run `event` for kiro-cli V3.
pub async fn run(d: &mut Daemon, event: HookEvent, args: &HookArgs<'_>) -> HookOutput {
    match event {
        HookEvent::SessionStart | HookEvent::Prompt => context(d).await,
        HookEvent::PreTool => pre_tool(d).await,
        HookEvent::Stop => stop(d, args.wake).await,
        HookEvent::PostTool | HookEvent::Wait | HookEvent::Status | HookEvent::Signoff => {
            HookOutput::default()
        }
    }
}

async fn context(d: &mut Daemon) -> HookOutput {
    let Some(status) = d.status(false).await else {
        return HookOutput::default();
    };
    d.mark_busy(&status);
    let mut text = text::guidance_reminder(&status);
    if let Some(r) = text::reminder(&status) {
        text.push('\n');
        text.push_str(&r);
    }
    HookOutput {
        stdout: text,
        ..HookOutput::default()
    }
}

async fn pre_tool(d: &mut Daemon) -> HookOutput {
    let Some(status) = d.status(true).await else {
        return HookOutput::default();
    };
    d.mark_busy(&status);
    match text::urgent(&status.urgent) {
        Some(urgent) => HookOutput {
            stderr: format!(
                "{urgent}\n\nagentmail held this tool call so you see the urgent mail \
                 first; it did not run. Run it again if it is still right."
            ),
            exit: EXIT_BLOCK_TOOL,
            ..HookOutput::default()
        },
        None => HookOutput::default(),
    }
}

async fn stop(d: &mut Daemon, wake: bool) -> HookOutput {
    let Some(status) = d.status(false).await else {
        return HookOutput::default();
    };
    if let Some(questions) = text::open_questions(&status) {
        return HookOutput {
            stderr: questions,
            exit: EXIT_CONTINUE,
            ..HookOutput::default()
        };
    }
    if !wake || !status.wake || headless(d) {
        return HookOutput::default();
    }
    // kiro gives a Stop hook no key: the wait decides by itself, and with
    // no open questions (just checked) unread mail already there wakes the
    // agent once.
    match d.wait(true, None).await {
        WaitOutcome::Mail(status) => match text::reminder(&status) {
            Some(reminder) => HookOutput {
                stderr: reminder,
                exit: EXIT_CONTINUE,
                ..HookOutput::default()
            },
            None => HookOutput::default(),
        },
        WaitOutcome::Gone | WaitOutcome::Busy | WaitOutcome::Skip => HookOutput::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::headless_args;

    #[test]
    fn headless_args_finds_runs_nobody_types_into() {
        for args in [
            &["chat", "--v3", "--no-interactive", "hi"][..],
            &["--no-interactive", "chat"],
            &["chat", "--v3", "--output-format", "stream-json", "hi"],
            &["chat", "--output-format=stream-json"],
            &["acp"],
            &["acp", "--agent-engine", "v3"],
            &["-v", "--v3", "acp"],
            &["--agent", "dev", "acp"],
        ] {
            assert!(headless_args(args), "{args:?}");
        }
    }

    #[test]
    fn headless_args_leaves_interactive_chats() {
        for args in [
            &[][..],
            &["chat", "--v3"],
            &["--v3"],
            // A prompt or an agent named `acp` is not the subcommand.
            &["chat", "--v3", "acp"],
            &["chat", "--agent", "acp"],
            &["--agent", "acp"],
            &["--agent", "acp", "chat"],
            &["chat", "--output-format", "text"],
            &["chat", "--output-format=text", "stream-json"],
        ] {
            assert!(!headless_args(args), "{args:?}");
        }
    }
}
