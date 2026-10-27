//! opencode's hook output: JSON for the agentmail plugin
//! (`plugin/agentmail.js`), which opencode loads in its shared process and
//! which runs these hooks for one session at a time (`--session`). See
//! docs/harness-notes.md, "opencode plugin".
//!
//! - `status [--surface-urgent]`: the session agent's [`HookStatus`] as
//!   JSON, plus a `text` object with the words to show, rendered as for
//!   every other harness ([`text`]): `guidance`, and `reminder`, `urgent`
//!   and `questions` (each `null` when there is nothing to say). With
//!   `--surface-urgent` the urgent mail reported is marked surfaced, so it
//!   is pushed once. `{}` when the session has no agent or the daemon is
//!   down.
//! - `wait [--timeout SECS]`: block while the idle session has no mail to
//!   wake for ([`Daemon::wait`]), then print one line:
//!   `{"mail":true,"text":<reminder>}`; `{"mail":false}` when the timeout
//!   (default [`DEFAULT_WAIT`]) passed first, so the plugin waits again;
//!   `{"busy":true}` when the session is not idle (open questions continue
//!   the turn, or a later wait took over); `{"skip":true}` when the agent
//!   does not want waking (`wake` off) or the waiter lock is unusable; and
//!   `{"gone":true}` when the agent signed off or is gone, the session has
//!   no agent, the daemon is down or went away, or opencode exited. Only
//!   `{"mail":false}` asks the plugin to wait again at once; it stops on
//!   the others until the session is idle again.
//!
//!   The payload on stdin is `{"stop_hook_active": <bool>}`: `true` when
//!   the turn that just ended was one the plugin started itself (a wake, or
//!   its open questions), as Claude Code's Stop payload says. Then only
//!   mail that arrives from now on counts; otherwise unread mail already
//!   there wakes the session at once, once.
//!
//! Every other event prints nothing: the plugin runs only these two.

use std::time::Duration;

use serde_json::{Value, json};

use super::{Daemon, HookArgs, HookEvent, HookInput, HookOutput, HookStatus, WaitOutcome, text};

/// How long `wait` waits without `--timeout`.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(300);

/// Run `event` for the opencode plugin.
pub async fn run(
    d: &mut Daemon,
    event: HookEvent,
    args: &HookArgs<'_>,
    input: &HookInput,
) -> HookOutput {
    match event {
        HookEvent::Status => match d.status(args.surface_urgent).await {
            Some(status) => printed(&status_json(&status)),
            None => unavailable(event),
        },
        HookEvent::Wait => {
            let limit = args.timeout.unwrap_or(DEFAULT_WAIT);
            let wait = d.wait(!input.stop_hook_active, None);
            match tokio::time::timeout(limit, wait).await {
                Ok(outcome) => printed(&wait_json(outcome)),
                Err(_) => printed(&json!({ "mail": false })),
            }
        }
        HookEvent::SessionStart
        | HookEvent::Prompt
        | HookEvent::PostTool
        | HookEvent::PreTool
        | HookEvent::Stop => HookOutput::default(),
    }
}

/// The output when the hook has no agent to act as (no session, no agent
/// for it, the daemon down, or the human): `{}` for `status`,
/// `{"gone":true}` for `wait`, nothing otherwise.
pub fn unavailable(event: HookEvent) -> HookOutput {
    match event {
        HookEvent::Status => printed(&json!({})),
        HookEvent::Wait => printed(&json!({ "gone": true })),
        _ => HookOutput::default(),
    }
}

/// `status`'s JSON: the status, with the words to show under `text`.
pub fn status_json(status: &HookStatus) -> Value {
    let mut v = serde_json::to_value(status).unwrap_or_else(|_| json!({}));
    v["text"] = json!({
        "guidance": text::guidance_reminder(status),
        "reminder": text::reminder(status),
        "urgent": text::urgent(&status.urgent),
        "questions": text::open_questions(status),
    });
    v
}

/// `wait`'s JSON for how the wait ended.
fn wait_json(outcome: WaitOutcome) -> Value {
    match outcome {
        WaitOutcome::Mail(status) => json!({ "mail": true, "text": text::reminder(&status) }),
        WaitOutcome::Busy => json!({ "busy": true }),
        WaitOutcome::Skip => json!({ "skip": true }),
        WaitOutcome::Gone => json!({ "gone": true }),
    }
}

fn printed(v: &Value) -> HookOutput {
    HookOutput {
        stdout: v.to_string(),
        ..HookOutput::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_per_event() {
        assert_eq!(unavailable(HookEvent::Status).stdout, "{}");
        assert_eq!(unavailable(HookEvent::Wait).stdout, r#"{"gone":true}"#);
        assert_eq!(unavailable(HookEvent::PostTool), HookOutput::default());
    }

    #[test]
    fn wait_json_per_outcome() {
        assert_eq!(wait_json(WaitOutcome::Busy), json!({ "busy": true }));
        assert_eq!(wait_json(WaitOutcome::Skip), json!({ "skip": true }));
        assert_eq!(wait_json(WaitOutcome::Gone), json!({ "gone": true }));
    }
}
