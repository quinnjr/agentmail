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
//!   `{"mail":true,"text":<reminder>}`; `{"mail":false,"known":[<ids>]}`
//!   when the timeout (default [`DEFAULT_WAIT`]) passed first, so the
//!   plugin waits again, passing `known` back (below);
//!   `{"busy":true}` when the session is not idle (open questions continue
//!   the turn, though never on a wait passed `known` or `after_wake`, or a
//!   later wait took over); `{"skip":true}` when the agent
//!   does not want waking (`wake` off) or the waiter lock is unusable; and
//!   `{"gone":true}` when the agent signed off or is gone, the session has
//!   no agent, the daemon is down or went away, opencode exited, or the
//!   hook runs as the human. Only
//!   `{"mail":false}` asks the plugin to wait again at once; it stops on
//!   the others until the session is idle again.
//!
//!   The payload on stdin is `{"stop_hook_active": <bool>}`, optionally
//!   with `"after_wake": true` or `"known": [<ids>]`. Plain
//!   (`stop_hook_active` `false`, after a human's turn): unread mail
//!   already there wakes the session at once, once (not again for mail a
//!   wake was already for). `stop_hook_active` `true`, as Claude Code's
//!   Stop payload says after a continuation: only mail that arrives from
//!   now on counts (the plugin's waiter uses it after a wake it could not
//!   queue). `after_wake` (the turn that just ended was started by a wake,
//!   or failed or was interrupted): mail the last wake was for does not
//!   count, all other unread mail does, so mail that came during the turn
//!   wakes the session. `known` (the ids the timed-out wait before this
//!   one did not count as news, or the unread mail when the plugin queued
//!   open questions as a turn): exactly that mail does not count
//!   ([`Daemon::wait_from`]), so mail delivered between the two waits, or
//!   during the questions' turn, is news rather than counted as already
//!   there by the new wait's first look. With `after_wake` or `known`, open
//!   questions never end the wait as `{"busy":true}`.
//! - `signoff`: sign the session's agent off, when opencode deletes the
//!   session: `{"signed_off":true,"address":<address>}`, or `{}` when the
//!   session has no agent or the daemon is down. Like every hook it never
//!   starts the daemon, and it acts only as the session's agent (no
//!   process-ancestry fallback, unlike the CLI's `signoff`).
//!
//! Every other event prints nothing: the plugin runs only these three.

use std::collections::HashSet;
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
            let mut known: Option<HashSet<String>> = input
                .known
                .as_ref()
                .map(|ids| ids.iter().cloned().collect());
            let wait = d.wait_from(!input.stop_hook_active, None, &mut known, input.after_wake);
            match tokio::time::timeout(limit, wait).await {
                Ok(outcome) => printed(&wait_json(outcome)),
                Err(_) => printed(&timed_out_json(known)),
            }
        }
        HookEvent::Signoff => match d.signoff().await {
            Some(view) => printed(&json!({
                "signed_off": true,
                "address": view["address"],
            })),
            None => unavailable(event),
        },
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
        HookEvent::Status | HookEvent::Signoff => printed(&json!({})),
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

/// `wait`'s JSON when the timeout passed first: `known` (sorted) is what
/// the next wait gets back; left out when the wait never looked (and had
/// no `known` passed in), so the next wait looks for itself.
fn timed_out_json(known: Option<HashSet<String>>) -> Value {
    match known {
        Some(known) => {
            let mut ids: Vec<String> = known.into_iter().collect();
            ids.sort();
            json!({ "mail": false, "known": ids })
        }
        None => json!({ "mail": false }),
    }
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
        assert_eq!(unavailable(HookEvent::Signoff).stdout, "{}");
        assert_eq!(unavailable(HookEvent::Wait).stdout, r#"{"gone":true}"#);
        assert_eq!(unavailable(HookEvent::PostTool), HookOutput::default());
    }

    #[test]
    fn timed_out_json_sorts_known() {
        assert_eq!(timed_out_json(None), json!({ "mail": false }));
        let known: HashSet<String> = ["m2".to_string(), "m1".to_string()].into();
        assert_eq!(
            timed_out_json(Some(known)),
            json!({ "mail": false, "known": ["m1", "m2"] })
        );
        assert_eq!(
            timed_out_json(Some(HashSet::new())),
            json!({ "mail": false, "known": [] })
        );
    }

    #[test]
    fn wait_json_per_outcome() {
        assert_eq!(wait_json(WaitOutcome::Busy), json!({ "busy": true }));
        assert_eq!(wait_json(WaitOutcome::Skip), json!({ "skip": true }));
        assert_eq!(wait_json(WaitOutcome::Gone), json!({ "gone": true }));
    }
}
