//! The words hooks put in front of an agent, shared by every harness.

use super::HookStatus;
use crate::model::Message;

/// Longest urgent body shown in full, in bytes; longer ones are cut and
/// point at `read`.
pub const MAX_URGENT_BODY: usize = 4096;

/// The unread count and open questions, e.g.
/// `📬 agentmail — you are backend-k3x9q2mz: 2 unread (1 question from
/// frontend-p7v2c8wd: "POST /orders body?"). Check your inbox before
/// continuing.` `None` when there is no unread mail and no open question.
pub fn reminder(status: &HookStatus) -> Option<String> {
    if status.unread == 0 && status.open_questions.is_empty() {
        return None;
    }
    let questions = match status.open_questions.as_slice() {
        [] => String::new(),
        [q] => format!(" (1 question from {}: \"{}\")", q.from, q.subject),
        [q, rest @ ..] => format!(
            " ({} questions, the oldest from {}: \"{}\")",
            rest.len() + 1,
            q.from,
            q.subject
        ),
    };
    Some(format!(
        "📬 agentmail — you are {}: {} unread{questions}. Check your inbox before continuing.",
        status.address, status.unread
    ))
}

/// Urgent mail in full, each body capped at [`MAX_URGENT_BODY`] bytes.
/// `None` when there is none.
pub fn urgent(msgs: &[Message]) -> Option<String> {
    if msgs.is_empty() {
        return None;
    }
    let mut out = String::from(
        "🚨 agentmail — urgent mail. It stays unread in your inbox; act on it before continuing.",
    );
    for m in msgs {
        out.push_str(&format!(
            "\n\nFrom {}, subject \"{}\" (id {}):\n{}",
            m.from,
            m.subject,
            m.id,
            capped(&m.body, &m.id)
        ));
    }
    Some(out)
}

/// `body`, cut at a character boundary to at most [`MAX_URGENT_BODY`]
/// bytes, with a pointer to the full message when cut.
fn capped(body: &str, id: &str) -> String {
    if body.len() <= MAX_URGENT_BODY {
        return body.to_string();
    }
    let mut end = MAX_URGENT_BODY;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… (truncated; read {id})", &body[..end])
}

/// The open questions addressed to the agent, oldest first, to answer
/// before stopping. `None` when there are none.
pub fn open_questions(status: &HookStatus) -> Option<String> {
    if status.open_questions.is_empty() {
        return None;
    }
    let mut out = format!(
        "agentmail — you are {} and these questions to you are still open:",
        status.address
    );
    for q in &status.open_questions {
        out.push_str(&format!("\n- {} from {}: \"{}\"", q.id, q.from, q.subject));
    }
    out.push_str("\nRead and answer them (`answer`) before you stop.");
    Some(out)
}

/// One line: who the agent is and how to work with the mail.
pub fn guidance_reminder(status: &HookStatus) -> String {
    format!(
        "agentmail: you are {}. Check your inbox between steps; answer questions addressed to you first.",
        status.address
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::OpenQuestion;
    use crate::model::Kind;

    fn status(unread: usize, questions: &[(&str, &str)]) -> HookStatus {
        HookStatus {
            address: "backend-k3x9q2mz".into(),
            name: "backend".into(),
            wake: true,
            unread,
            unread_ids: (0..unread).map(|i| format!("M{i}")).collect(),
            open_questions: questions
                .iter()
                .enumerate()
                .map(|(i, (from, subject))| OpenQuestion {
                    id: format!("Q{i}"),
                    from: from.to_string(),
                    subject: subject.to_string(),
                })
                .collect(),
            urgent: Vec::new(),
        }
    }

    fn msg(body: &str) -> Message {
        Message {
            id: "01ID".into(),
            thread_id: "01ID".into(),
            in_reply_to: None,
            from: "human".into(),
            to: vec!["backend-k3x9q2mz".into()],
            subject: "stop".into(),
            body: body.into(),
            kind: Kind::Note,
            urgent: true,
            refs: Vec::new(),
            created_at: 0,
            from_human: true,
        }
    }

    #[test]
    fn hook_text_reminder_matches_the_plan() {
        assert_eq!(reminder(&status(0, &[])), None);
        assert_eq!(
            reminder(&status(2, &[("frontend-p7v2c8wd", "POST /orders body?")])).unwrap(),
            "📬 agentmail — you are backend-k3x9q2mz: 2 unread (1 question from \
             frontend-p7v2c8wd: \"POST /orders body?\"). Check your inbox before continuing."
        );
        assert_eq!(
            reminder(&status(1, &[])).unwrap(),
            "📬 agentmail — you are backend-k3x9q2mz: 1 unread. Check your inbox before continuing."
        );
        let many = reminder(&status(0, &[("a-1", "first"), ("b-2", "second")])).unwrap();
        assert!(
            many.contains("0 unread (2 questions, the oldest from a-1: \"first\")"),
            "{many}"
        );
    }

    #[test]
    fn hook_text_urgent_caps_bodies_at_a_char_boundary() {
        assert_eq!(urgent(&[]), None);
        let short = urgent(&[msg("now")]).unwrap();
        assert!(
            short.ends_with("From human, subject \"stop\" (id 01ID):\nnow"),
            "{short}"
        );
        // 3-byte characters: 4096 is not a boundary.
        let long = urgent(&[msg(&"€".repeat(2000))]).unwrap();
        let body = long.split_once("(id 01ID):\n").unwrap().1;
        let kept = body.strip_suffix("… (truncated; read 01ID)").unwrap();
        assert_eq!(kept.len(), 4095);
        assert_eq!(kept, "€".repeat(1365));
        // Exactly at the cap: kept whole.
        let exact = "a".repeat(MAX_URGENT_BODY);
        assert!(urgent(&[msg(&exact)]).unwrap().ends_with(&exact));
    }

    #[test]
    fn hook_text_open_questions_lists_each() {
        assert_eq!(open_questions(&status(3, &[])), None);
        let text = open_questions(&status(0, &[("a-1", "first"), ("b-2", "second")])).unwrap();
        assert!(text.contains("\n- Q0 from a-1: \"first\""), "{text}");
        assert!(text.contains("\n- Q1 from b-2: \"second\""), "{text}");
    }

    #[test]
    fn hook_text_guidance_is_one_line() {
        let g = guidance_reminder(&status(0, &[]));
        assert!(g.contains("backend-k3x9q2mz"));
        assert_eq!(g.lines().count(), 1);
    }
}
