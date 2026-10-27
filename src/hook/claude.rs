//! Claude Code's hook output formats (see docs/harness-notes.md, "Claude
//! Code hooks").
//!
//! - `session-start`, `prompt`: `additionalContext` with who the agent is
//!   and its reminder.
//! - `post-tool`: urgent mail in full, once (it is marked surfaced), else
//!   the reminder; nothing when there is neither.
//! - `stop`: `decision: block` while questions to the agent are open,
//!   unless this stop already follows a hook continuation
//!   (`stop_hook_active`), so a model that cannot answer is not held.
//! - `wait` (an `asyncRewake` Stop hook): block until mail comes, then exit
//!   2 with the reminder on stderr, which wakes the idle session.

use std::ffi::OsString;

use serde_json::json;

use super::{Daemon, HookEvent, HookInput, HookOutput, WaitOutcome, text};

/// Claude Code's entry point for its interactive terminal UI, as set in
/// `CLAUDE_CODE_ENTRYPOINT`.
const INTERACTIVE_ENTRYPOINT: &str = "cli";

/// Claude Code's exit code for a hook result to act on (block, or wake).
const EXIT_BLOCK: u8 = 2;

/// Run `event` for Claude Code.
pub async fn run(
    d: &mut Daemon,
    event: HookEvent,
    input: &HookInput,
    env: &(dyn Fn(&str) -> Option<OsString> + Sync),
) -> HookOutput {
    match event {
        HookEvent::SessionStart => context(d, "SessionStart").await,
        HookEvent::Prompt => context(d, "UserPromptSubmit").await,
        HookEvent::PostTool => post_tool(d).await,
        HookEvent::Stop => stop(d, input).await,
        HookEvent::Wait => wait(d, input, env).await,
        HookEvent::PreTool | HookEvent::Status => HookOutput::default(),
    }
}

/// `additionalContext` output for `event_name`.
fn additional_context(event_name: &str, text: String) -> HookOutput {
    let v = json!({
        "hookSpecificOutput": {
            "hookEventName": event_name,
            "additionalContext": text,
        }
    });
    HookOutput {
        stdout: v.to_string(),
        ..HookOutput::default()
    }
}

async fn context(d: &mut Daemon, event_name: &str) -> HookOutput {
    let Some(status) = d.status(false).await else {
        return HookOutput::default();
    };
    let mut text = text::guidance_reminder(&status);
    if let Some(r) = text::reminder(&status) {
        text.push('\n');
        text.push_str(&r);
    }
    additional_context(event_name, text)
}

async fn post_tool(d: &mut Daemon) -> HookOutput {
    let Some(status) = d.status(true).await else {
        return HookOutput::default();
    };
    match text::urgent(&status.urgent).or_else(|| text::reminder(&status)) {
        Some(text) => additional_context("PostToolUse", text),
        None => HookOutput::default(),
    }
}

async fn stop(d: &mut Daemon, input: &HookInput) -> HookOutput {
    if input.stop_hook_active {
        return HookOutput::default();
    }
    let Some(status) = d.status(false).await else {
        return HookOutput::default();
    };
    match text::open_questions(&status) {
        Some(reason) => HookOutput {
            stdout: json!({ "decision": "block", "reason": reason }).to_string(),
            ..HookOutput::default()
        },
        None => HookOutput::default(),
    }
}

/// Wait for mail, then wake the session. Only in the interactive UI:
/// `claude -p` runs `asyncRewake` hooks synchronously, so waiting there
/// would hold every headless run open until mail came.
async fn wait(
    d: &mut Daemon,
    input: &HookInput,
    env: &(dyn Fn(&str) -> Option<OsString> + Sync),
) -> HookOutput {
    let entrypoint = env("CLAUDE_CODE_ENTRYPOINT");
    if entrypoint.is_some_and(|e| e != INTERACTIVE_ENTRYPOINT) {
        return HookOutput::default();
    }
    match d.wait(!input.stop_hook_active).await {
        WaitOutcome::Mail(status) => match text::reminder(&status) {
            Some(reminder) => HookOutput {
                stderr: reminder,
                exit: EXIT_BLOCK,
                ..HookOutput::default()
            },
            None => HookOutput::default(),
        },
        WaitOutcome::Gone | WaitOutcome::Skip => HookOutput::default(),
    }
}
