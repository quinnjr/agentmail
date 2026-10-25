//! Human-readable output for the CLI. `--json` bypasses all of this and
//! prints the daemon's data as is.

use std::fmt::Write as _;

use crate::model::{AgentView, Kind, Message, Presence};
use crate::protocol::Event;
use crate::store::ThreadSummary;

/// Longest working-on summary shown in a table cell, in characters.
const SUMMARY_CELL: usize = 60;
/// Widest role cell in the agents table, in characters.
const ROLE_CELL: usize = 24;

/// How long ago `then_ms` was, at `now_ms`, in the largest whole unit:
/// `12s`, `14m`, `3h`, `2d`. Future times count as `0s`.
pub fn age(now_ms: i64, then_ms: i64) -> String {
    let secs = now_ms.saturating_sub(then_ms).max(0) / 1000;
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Unix milliseconds as a UTC timestamp, `2026-10-25 14:03:07Z`.
pub fn utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// The last 8 characters of a message id, as the inbox shows it.
pub fn short_id(id: &str) -> &str {
    let start = id.len().saturating_sub(8);
    id.get(start..).unwrap_or(id)
}

/// `text` cut to `max` characters, with `…` marking a cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Characters that reorder or hide text on a terminal: bidirectional
/// embeddings, overrides and isolates, and directional marks.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// `text` made safe for one line of a table or header: line breaks and tabs
/// become spaces, and other control characters (ESC among them, so no
/// terminal escape sequence gets through) and bidirectional controls are
/// shown escaped, like `\u{1b}`. Message bodies never go through this.
pub fn cell(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' | '\r' | '\t' => out.push(' '),
            c if c.is_control() || is_bidi_control(c) => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Every item through `cell`, joined with `, `.
fn cells(items: &[String]) -> String {
    items.iter().map(|i| cell(i)).collect::<Vec<_>>().join(", ")
}

/// One line per row, columns padded to their widest cell. Trailing spaces
/// are trimmed.
fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|i| {
            rows.iter()
                .filter_map(|r| r.get(i))
                .map(|c| c.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            line.push_str(cell);
            if i + 1 < row.len() {
                let pad = widths[i] - cell.chars().count() + 2;
                line.extend(std::iter::repeat_n(' ', pad));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn kind_marker(kind: Kind) -> &'static str {
    match kind {
        Kind::Question => "[Q]",
        Kind::Answer => "[A]",
        Kind::Note => "   ",
    }
}

/// The sender as shown in headers: `human [from human]` for the human.
fn sender(m: &Message) -> String {
    if m.from_human {
        format!("{} [from human]", cell(&m.from))
    } else {
        cell(&m.from)
    }
}

/// One inbox line: kind marker, `!` if urgent, short id, sender (`*` for a
/// contact), subject and age.
pub fn inbox_line(m: &Message, from_contact: bool, now_ms: i64) -> Vec<String> {
    let from = if from_contact {
        format!("*{}", sender(m))
    } else {
        sender(m)
    };
    vec![
        kind_marker(m.kind).to_string(),
        if m.urgent { "!" } else { " " }.to_string(),
        cell(short_id(&m.id)),
        from,
        cell(&m.subject),
        age(now_ms, m.created_at),
    ]
}

/// The inbox: one line per message, or a note when it is empty.
pub fn inbox(entries: &[(Message, bool)], now_ms: i64) -> String {
    if entries.is_empty() {
        return "no mail\n".to_string();
    }
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|(m, c)| inbox_line(m, *c, now_ms))
        .collect();
    table(&rows)
}

/// Messages found by `search`: full ids, since they need not be in the
/// caller's mailbox.
pub fn message_list(msgs: &[Message], now_ms: i64) -> String {
    if msgs.is_empty() {
        return "no messages\n".to_string();
    }
    let rows: Vec<Vec<String>> = msgs
        .iter()
        .map(|m| {
            vec![
                kind_marker(m.kind).to_string(),
                if m.urgent { "!" } else { " " }.to_string(),
                cell(&m.id),
                sender(m),
                cell(&m.subject),
                age(now_ms, m.created_at),
            ]
        })
        .collect();
    table(&rows)
}

/// One message: a header line (`── from → to · time · kind`), the id and
/// subject, any refs, a blank line, then the body exactly as stored.
pub fn message(m: &Message) -> String {
    let mut out = String::new();
    let kind = match m.kind {
        Kind::Note => "note",
        Kind::Question => "question",
        Kind::Answer => "answer",
    };
    let _ = write!(
        out,
        "── {} → {} · {} · {kind}",
        sender(m),
        cells(&m.to),
        utc(m.created_at)
    );
    if m.urgent {
        out.push_str(" · urgent");
    }
    out.push('\n');
    let _ = writeln!(out, "   {}  {}", cell(&m.id), cell(&m.subject));
    if !m.refs.is_empty() {
        let _ = writeln!(out, "   refs: {}", cells(&m.refs));
    }
    out.push('\n');
    out.push_str(&m.body);
    if !m.body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// A whole thread, messages separated by a blank line.
pub fn thread(msgs: &[Message]) -> String {
    msgs.iter().map(message).collect::<Vec<_>>().join("\n")
}

/// "idle 14m" for an active agent, "seen 2h ago" for a gone one.
fn last_seen(v: &AgentView, now_ms: i64) -> String {
    let ago = age(now_ms, v.agent.last_seen);
    match v.presence {
        Presence::Active => format!("idle {ago}"),
        Presence::Gone => format!("seen {ago} ago"),
    }
}

fn presence(p: Presence) -> &'static str {
    match p {
        Presence::Active => "active",
        Presence::Gone => "gone",
    }
}

fn harness(v: &AgentView) -> String {
    serde_json::to_value(v.agent.harness)
        .ok()
        .and_then(|h| h.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The agent directory: address (`*` marks a contact), role, presence,
/// harness, repo (or cwd), working-on summary and last-seen.
pub fn agents(views: &[AgentView], now_ms: i64) -> String {
    if views.is_empty() {
        return "no agents\n".to_string();
    }
    let mut rows = vec![
        [
            "  ADDRESS",
            "ROLE",
            "PRESENCE",
            "HARNESS",
            "REPO",
            "WORKING ON",
            "LAST SEEN",
        ]
        .map(String::from)
        .to_vec(),
    ];
    for v in views {
        let a = &v.agent;
        let mark = if v.is_contact { "* " } else { "  " };
        let place = a.repo.as_ref().unwrap_or(&a.cwd);
        rows.push(vec![
            format!("{mark}{}", cell(&a.address)),
            clip(&cell(a.role.as_deref().unwrap_or("-")), ROLE_CELL),
            presence(v.presence).to_string(),
            harness(v),
            cell(&place.display().to_string()),
            clip(&cell(a.working_on.as_deref().unwrap_or("-")), SUMMARY_CELL),
            last_seen(v, now_ms),
        ]);
    }
    table(&rows)
}

/// One agent in full, for `whoami`, `status` and `register`.
pub fn agent(v: &AgentView, now_ms: i64) -> String {
    let a = &v.agent;
    let mut out = String::new();
    let _ = writeln!(out, "{}", cell(&a.address));
    if let Some(role) = &a.role {
        let _ = writeln!(out, "  role:       {}", cell(role));
    }
    let _ = writeln!(
        out,
        "  working on: {}",
        cell(a.working_on.as_deref().unwrap_or("-"))
    );
    let _ = writeln!(
        out,
        "  presence:   {} ({})",
        presence(v.presence),
        last_seen(v, now_ms)
    );
    let _ = writeln!(out, "  harness:    {}", harness(v));
    if let Some(repo) = &a.repo {
        let _ = writeln!(out, "  repo:       {}", cell(&repo.display().to_string()));
    }
    let _ = writeln!(out, "  cwd:        {}", cell(&a.cwd.display().to_string()));
    out
}

/// Thread listing: id, subject, message count, participants, open
/// questions and age of the newest message.
pub fn threads(list: &[ThreadSummary], now_ms: i64) -> String {
    if list.is_empty() {
        return "no threads\n".to_string();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|t| {
            vec![
                cell(&t.thread_id),
                cell(&t.subject),
                format!("{} msg", t.messages),
                cells(&t.participants),
                if t.open_questions > 0 {
                    format!("{} open [Q]", t.open_questions)
                } else {
                    String::new()
                },
                age(now_ms, t.last_at),
            ]
        })
        .collect();
    table(&rows)
}

/// One line for a `watch` event.
pub fn event(e: &Event, now_ms: i64) -> String {
    match e {
        Event::Delivered { to, message: m } => {
            let mut parts = vec![
                utc(now_ms),
                "mail     ".to_string(),
                sender(m),
                "→".to_string(),
                cell(to),
            ];
            let kind = kind_marker(m.kind).trim();
            if !kind.is_empty() {
                parts.push(kind.to_string());
            }
            if m.urgent {
                parts.push("!".to_string());
            }
            parts.push(format!("\"{}\"", cell(&m.subject)));
            parts.join(" ")
        }
        Event::Presence { agent: v } => format!(
            "{} presence  {} {} · {}",
            utc(now_ms),
            cell(&v.agent.address),
            if v.agent.signed_off {
                "signed off"
            } else {
                presence(v.presence)
            },
            cell(v.agent.working_on.as_deref().unwrap_or("-"))
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Agent, Harness};
    use std::path::PathBuf;

    fn msg(kind: Kind, urgent: bool) -> Message {
        Message {
            id: "01JABCDEFGHJKMNPQRSTVWXYZ0".into(),
            thread_id: "01JABCDEFGHJKMNPQRSTVWXYZ0".into(),
            in_reply_to: None,
            from: "backend-abcd1234".into(),
            to: vec!["frontend-wxyz9876".into()],
            subject: "Schema".into(),
            body: "body".into(),
            kind,
            urgent,
            refs: vec![],
            created_at: 0,
            from_human: false,
        }
    }

    fn view(contact: bool, presence: Presence) -> AgentView {
        AgentView {
            agent: Agent {
                address: "backend-abcd1234".into(),
                name: "backend".into(),
                role: None,
                working_on: Some("orders API".into()),
                harness: Harness::ClaudeCode,
                cwd: PathBuf::from("/w/shop"),
                repo: Some(PathBuf::from("/w/shop")),
                proc_id: None,
                wake: true,
                registered_at: 0,
                last_seen: 0,
                signed_off: false,
            },
            presence,
            is_contact: contact,
        }
    }

    #[test]
    fn render_age_picks_the_largest_unit() {
        assert_eq!(age(59_999, 0), "59s");
        assert_eq!(age(60_000, 0), "1m");
        assert_eq!(age(14 * 60_000 + 5, 0), "14m");
        assert_eq!(age(3 * 3_600_000, 0), "3h");
        assert_eq!(age(2 * 86_400_000 + 1, 0), "2d");
        assert_eq!(age(0, 5_000), "0s");
    }

    #[test]
    fn render_utc_formats_known_instants() {
        assert_eq!(utc(0), "1970-01-01 00:00:00Z");
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00:00Z");
        assert_eq!(utc(1_792_936_987_000), "2026-10-25 14:03:07Z");
        assert_eq!(utc(-1000), "1969-12-31 23:59:59Z");
    }

    #[test]
    fn render_short_id_is_last_8() {
        assert_eq!(short_id("01JABCDEFGHJKMNPQRSTVWXYZ0"), "STVWXYZ0");
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn render_inbox_line_markers() {
        let q = inbox_line(&msg(Kind::Question, true), true, 60_000);
        assert_eq!(q[0], "[Q]");
        assert_eq!(q[1], "!");
        assert_eq!(q[2], "STVWXYZ0");
        assert_eq!(q[3], "*backend-abcd1234");
        assert_eq!(q[4], "Schema");
        assert_eq!(q[5], "1m");
        let a = inbox_line(&msg(Kind::Answer, false), false, 0);
        assert_eq!((a[0].as_str(), a[1].as_str()), ("[A]", " "));
        let n = inbox_line(&msg(Kind::Note, false), false, 0);
        assert_eq!(n[0], "   ");
        let mut h = msg(Kind::Note, false);
        h.from = "human".into();
        h.from_human = true;
        assert_eq!(inbox_line(&h, false, 0)[3], "human [from human]");
    }

    #[test]
    fn render_message_keeps_body_bytes() {
        let mut m = msg(Kind::Answer, true);
        m.body = "```\ncode\n```\r\n| a |\nnaïve".into();
        m.refs = vec!["docs/api.md".into()];
        let out = message(&m);
        assert!(out.starts_with(
            "── backend-abcd1234 → frontend-wxyz9876 · 1970-01-01 00:00:00Z · answer · urgent\n"
        ));
        assert!(out.contains("refs: docs/api.md"));
        assert!(
            out.ends_with("\n\n```\ncode\n```\r\n| a |\nnaïve\n"),
            "{out:?}"
        );
        m.body = "ends\n".into();
        assert!(message(&m).ends_with("\n\nends\n"));
    }

    #[test]
    fn render_agents_table_marks_contacts() {
        let mut with_role = view(false, Presence::Gone);
        with_role.agent.role = Some(format!("API owner {}", "x".repeat(80)));
        let out = agents(&[view(true, Presence::Active), with_role], 14 * 60_000);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("  ADDRESS"));
        assert!(lines[0].contains("ROLE"), "{out}");
        // No role shows as `-`; a long one is clipped like WORKING ON.
        assert!(lines[1].contains(" - "), "{out}");
        assert!(lines[2].contains("API owner"), "{out}");
        assert!(
            lines[2].contains('…') && !lines[2].contains(&"x".repeat(80)),
            "{out}"
        );
        assert!(lines[1].starts_with("* backend-abcd1234"), "{out}");
        assert!(lines[1].contains("active") && lines[1].contains("idle 14m"));
        assert!(lines[1].contains("claude-code") && lines[1].contains("orders API"));
        assert!(lines[2].starts_with("  backend-abcd1234"), "{out}");
        assert!(lines[2].contains("gone") && lines[2].contains("seen 14m ago"));
        assert_eq!(agents(&[], 0), "no agents\n");
    }

    #[test]
    fn render_clip_marks_cuts() {
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcd", 3), "ab…");
        assert_eq!(clip("ééééé", 4), "ééé…");
    }

    #[test]
    fn render_cells_stay_on_one_line_without_escapes() {
        let nasty = "a\nb\x1b[31m\r\tc\u{202e}d";
        let check = |out: &str, rows: usize| {
            assert_eq!(out.lines().count(), rows, "{out:?}");
            assert!(!out.contains('\x1b'), "{out:?}");
            assert!(!out.contains('\r') && !out.contains('\t'), "{out:?}");
            assert!(!out.contains('\u{202e}'), "{out:?}");
        };
        let mut m = msg(Kind::Note, false);
        m.subject = nasty.into();
        m.refs = vec![nasty.into()];
        m.to = vec![nasty.into()];
        check(&inbox(&[(m.clone(), false)], 0), 1);
        check(&message_list(&[m.clone()], 0), 1);
        // The header lines of a message are sanitized; the body is not.
        m.body = "body\x1b".into();
        let out = message(&m);
        let header = out.split("\n\n").next().unwrap();
        check(header, 3);
        assert!(out.ends_with("\n\nbody\x1b\n"), "{out:?}");
        let mut v = view(false, Presence::Active);
        v.agent.role = Some(nasty.into());
        v.agent.working_on = Some(nasty.into());
        v.agent.repo = Some(PathBuf::from(nasty));
        v.agent.cwd = PathBuf::from(nasty);
        check(&agents(std::slice::from_ref(&v), 0), 2);
        check(&agent(&v, 0), 7);
        check(&event(&Event::Presence { agent: v.clone() }, 0), 1);
        check(
            &event(
                &Event::Delivered {
                    to: nasty.into(),
                    message: m.clone(),
                },
                0,
            ),
            1,
        );
        let t = ThreadSummary {
            thread_id: "T".into(),
            subject: nasty.into(),
            messages: 1,
            participants: vec![nasty.into()],
            open_questions: 0,
            last_at: 0,
        };
        check(&threads(&[t], 0), 1);
        assert_eq!(cell("plain naïve ✓"), "plain naïve ✓");
        assert_eq!(cell("a\nb\tc"), "a b c");
        assert_eq!(
            cell("x\x1b[31m\u{7f}\u{202e}"),
            "x\\u{1b}[31m\\u{7f}\\u{202e}"
        );
    }

    #[test]
    fn render_event_lines() {
        let line = event(
            &Event::Delivered {
                to: "frontend-wxyz9876".into(),
                message: msg(Kind::Question, true),
            },
            0,
        );
        assert_eq!(
            line,
            "1970-01-01 00:00:00Z mail      backend-abcd1234 → frontend-wxyz9876 [Q] ! \"Schema\""
        );
        let line = event(
            &Event::Delivered {
                to: "frontend-wxyz9876".into(),
                message: msg(Kind::Note, false),
            },
            0,
        );
        assert_eq!(
            line,
            "1970-01-01 00:00:00Z mail      backend-abcd1234 → frontend-wxyz9876 \"Schema\""
        );
        let line = event(
            &Event::Presence {
                agent: view(false, Presence::Active),
            },
            0,
        );
        assert_eq!(
            line,
            "1970-01-01 00:00:00Z presence  backend-abcd1234 active · orders API"
        );
    }
}
