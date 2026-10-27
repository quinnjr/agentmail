//! Format-preserving JSON output for config files people edit by hand.
//!
//! The installers change a config as a [`Value`] and [`render`] writes the
//! new value back over the original text, keeping every part of the text
//! whose value did not change byte for byte: its layout, key order, number
//! spellings and escapes. Only the containers that changed are rebuilt, and
//! those reuse the original separators and indentation; new entries are
//! pretty-printed with the file's own indent unit.

use serde_json::Value;

/// Parse a config file's text. Blank text is an empty object.
pub fn parse(text: &str) -> serde_json::Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_str(text)
}

/// `text` with its JSONC comments (`// ...` to the end of the line, and
/// `/* ... */`) blanked out with spaces, line breaks kept, or `None` when
/// it has none. Comment markers inside strings are not comments. An
/// unterminated `/*` blanks the rest of the text.
pub fn strip_comments(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut found = false;
    let mut chars = text.chars().peekable();
    let blank = |out: &mut String, c: char| out.push(if c == '\n' || c == '\r' { c } else { ' ' });
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push(c);
                while let Some(c) = chars.next() {
                    out.push(c);
                    match c {
                        '\\' => out.extend(chars.next()),
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => {
                found = true;
                blank(&mut out, c);
                while let Some(&c) = chars.peek() {
                    if c == '\n' || c == '\r' {
                        break;
                    }
                    blank(&mut out, c);
                    chars.next();
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                found = true;
                blank(&mut out, c);
                blank(&mut out, chars.next().expect("peeked"));
                let mut prev = ' ';
                for c in chars.by_ref() {
                    blank(&mut out, c);
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
            }
            c => out.push(c),
        }
    }
    found.then_some(out)
}

/// `new` written over `orig` (the text it was parsed from, or `None` for a
/// new file), changing only what differs. Always ends with a newline when
/// the original did, or when the file is new. New lines use the original's
/// line ending (`\r\n` when it uses that, else `\n`).
pub fn render(orig: Option<&str>, new: &Value) -> String {
    let Some(orig) = orig.filter(|t| !t.trim().is_empty()) else {
        return format!("{}\n", pretty(new, "  ", ""));
    };
    let unit = indent_unit(orig);
    let nl = if orig.contains("\r\n") { "\r\n" } else { "\n" };
    let fallback = || {
        let end = if orig.ends_with('\n') { nl } else { "" };
        format!("{}{end}", with_newline(pretty(new, &unit, ""), nl))
    };
    let Some(root) = Spans::parse(orig) else {
        return fallback();
    };
    let r = Renderer {
        text: orig,
        unit: unit.clone(),
        nl,
    };
    let mut out = String::with_capacity(orig.len() + 1024);
    out.push_str(&orig[..root.start]);
    r.node(new, &root, &mut out);
    out.push_str(&orig[root.end..]);
    // The output must say exactly `new`; anything else is a bug here, and
    // a plain re-serialisation is still correct.
    match serde_json::from_str::<Value>(&out) {
        Ok(v) if v == *new => out,
        _ => fallback(),
    }
}

/// `v` pretty-printed with `unit` per level, every line after the first
/// prefixed with `indent` (the indentation of the line it starts on).
pub fn pretty(v: &Value, unit: &str, indent: &str) -> String {
    use serde::Serialize;
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(unit.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    v.serialize(&mut ser)
        .expect("serialising a Value cannot fail");
    let s = String::from_utf8(buf).expect("serde_json writes UTF-8");
    if indent.is_empty() {
        s
    } else {
        s.replace('\n', &format!("\n{indent}"))
    }
}

/// `s` (with `\n` line endings) with `nl` line endings instead.
fn with_newline(s: String, nl: &str) -> String {
    if nl == "\n" { s } else { s.replace('\n', nl) }
}

/// The file's indent unit: the leading whitespace of its first indented
/// line, else two spaces.
fn indent_unit(text: &str) -> String {
    text.lines()
        .skip(1)
        .map(|l| &l[..l.len() - l.trim_start_matches([' ', '\t']).len()])
        .find(|w| !w.is_empty())
        .map(|w| {
            if w.starts_with('\t') {
                "\t".to_string()
            } else {
                w.to_string()
            }
        })
        .unwrap_or_else(|| "  ".to_string())
}

/// A JSON value's place in the text.
#[derive(Debug)]
struct Node {
    start: usize,
    end: usize,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    Object(Vec<Member>),
    Array(Vec<Node>),
    Scalar,
}

#[derive(Debug)]
struct Member {
    key: String,
    key_start: usize,
    key_end: usize,
    value: Node,
}

/// A lenient parser for text serde_json already accepted: it only records
/// where each value lies.
struct Spans<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
}

impl<'a> Spans<'a> {
    fn parse(text: &'a str) -> Option<Node> {
        let mut p = Spans {
            s: text.as_bytes(),
            text,
            i: 0,
        };
        let root = p.value()?;
        p.ws();
        (p.i == p.s.len()).then_some(root)
    }

    fn ws(&mut self) {
        while self
            .s
            .get(self.i)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.i += 1;
        }
    }

    fn value(&mut self) -> Option<Node> {
        self.ws();
        let start = self.i;
        let kind = match *self.s.get(self.i)? {
            b'{' => self.object()?,
            b'[' => self.array()?,
            b'"' => {
                self.string()?;
                Kind::Scalar
            }
            _ => {
                while self.s.get(self.i).is_some_and(|b| {
                    !matches!(b, b',' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r')
                }) {
                    self.i += 1;
                }
                if self.i == start {
                    return None;
                }
                Kind::Scalar
            }
        };
        Some(Node {
            start,
            end: self.i,
            kind,
        })
    }

    fn string(&mut self) -> Option<()> {
        self.i += 1;
        loop {
            match *self.s.get(self.i)? {
                b'\\' => self.i += 2,
                b'"' => {
                    self.i += 1;
                    return Some(());
                }
                _ => self.i += 1,
            }
        }
    }

    fn expect(&mut self, b: u8) -> Option<()> {
        self.ws();
        (self.s.get(self.i) == Some(&b)).then(|| self.i += 1)
    }

    fn object(&mut self) -> Option<Kind> {
        self.i += 1;
        let mut members = Vec::new();
        self.ws();
        if self.s.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Some(Kind::Object(members));
        }
        loop {
            self.ws();
            let key_start = self.i;
            if self.s.get(self.i) != Some(&b'"') {
                return None;
            }
            self.string()?;
            let key_end = self.i;
            let key = serde_json::from_str(self.text.get(key_start..key_end)?).ok()?;
            self.expect(b':')?;
            let value = self.value()?;
            members.push(Member {
                key,
                key_start,
                key_end,
                value,
            });
            self.ws();
            match *self.s.get(self.i)? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    return Some(Kind::Object(members));
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self) -> Option<Kind> {
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.s.get(self.i) == Some(&b']') {
            self.i += 1;
            return Some(Kind::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.ws();
            match *self.s.get(self.i)? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Some(Kind::Array(items));
                }
                _ => return None,
            }
        }
    }
}

struct Renderer<'a> {
    text: &'a str,
    unit: String,
    /// The line ending of new lines.
    nl: &'static str,
}

/// One entry of a rebuilt container: its text and, when it was in the
/// original container, its index there.
struct Item {
    text: String,
    orig: Option<usize>,
}

impl Renderer<'_> {
    fn slice(&self, start: usize, end: usize) -> &str {
        &self.text[start..end]
    }

    /// `v` pretty-printed at `indent`, with the file's line ending.
    fn pretty(&self, v: &Value, indent: &str) -> String {
        with_newline(pretty(v, &self.unit, indent), self.nl)
    }

    fn value_of(&self, n: &Node) -> Option<Value> {
        serde_json::from_str(self.slice(n.start, n.end)).ok()
    }

    /// The leading whitespace of the line holding `pos`.
    fn line_indent(&self, pos: usize) -> &str {
        let line_start = self.text[..pos].rfind('\n').map_or(0, |i| i + 1);
        let line = &self.text[line_start..pos];
        &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
    }

    fn node(&self, new: &Value, n: &Node, out: &mut String) {
        if self.value_of(n).as_ref() == Some(new) {
            out.push_str(self.slice(n.start, n.end));
            return;
        }
        match (new, &n.kind) {
            (Value::Object(map), Kind::Object(members)) => self.object(map, n, members, out),
            (Value::Array(items), Kind::Array(nodes)) => self.array(items, n, nodes, out),
            _ => out.push_str(&self.pretty(new, self.line_indent(n.start))),
        }
    }

    /// How entries of container `n` are laid out: the text before the
    /// first, the text after the last, the separator to use between two
    /// entries that were not neighbours, and the indentation of a new
    /// entry's line (`None` when the container sits on one line).
    fn layout(
        &self,
        n: &Node,
        spans: &[(usize, usize)],
    ) -> (String, String, String, Option<String>) {
        let Some(first) = spans.first() else {
            let base = self.line_indent(n.start);
            let inner = format!("{base}{}", self.unit);
            let nl = self.nl;
            return (
                format!("{nl}{inner}"),
                format!("{nl}{base}"),
                format!(",{nl}{inner}"),
                Some(inner),
            );
        };
        let last = spans.last().expect("spans is not empty");
        let lead = self.slice(n.start + 1, first.0).to_string();
        let tail = self.slice(last.1, n.end - 1).to_string();
        let sep = match spans.get(1) {
            Some(second) => self.slice(first.1, second.0).to_string(),
            None => format!(",{lead}"),
        };
        let indent = lead.rfind('\n').map(|i| lead[i + 1..].to_string());
        (lead, tail, sep, indent)
    }

    fn container(
        &self,
        open: char,
        close: char,
        n: &Node,
        spans: &[(usize, usize)],
        items: Vec<Item>,
        out: &mut String,
    ) {
        if items.is_empty() {
            out.push(open);
            out.push(close);
            return;
        }
        let (lead, tail, sep, _) = self.layout(n, spans);
        out.push(open);
        out.push_str(&lead);
        let mut prev: Option<Option<usize>> = None;
        for item in items {
            if let Some(p) = prev {
                // The separator that followed the previous entry, if it
                // had one; else the container's usual one.
                match p {
                    Some(k) if k + 1 < spans.len() => {
                        out.push_str(self.slice(spans[k].1, spans[k + 1].0))
                    }
                    _ => out.push_str(&sep),
                }
            }
            out.push_str(&item.text);
            prev = Some(item.orig);
        }
        out.push_str(&tail);
        out.push(close);
    }

    /// A new entry's text: pretty-printed at `indent`, or compact when the
    /// container sits on one line.
    fn fresh(&self, v: &Value, indent: &Option<String>) -> String {
        match indent {
            Some(i) => self.pretty(v, i),
            None => v.to_string(),
        }
    }

    fn object(
        &self,
        map: &serde_json::Map<String, Value>,
        n: &Node,
        members: &[Member],
        out: &mut String,
    ) {
        let spans: Vec<(usize, usize)> =
            members.iter().map(|m| (m.key_start, m.value.end)).collect();
        let (_, _, _, indent) = self.layout(n, &spans);
        let colon = members
            .first()
            .map_or(": ", |m| self.slice(m.key_end, m.value.start));
        let items = map
            .iter()
            .map(|(k, v)| match members.iter().rposition(|m| m.key == *k) {
                Some(i) => {
                    let m = &members[i];
                    let mut text = self.slice(m.key_start, m.value.start).to_string();
                    self.node(v, &m.value, &mut text);
                    Item {
                        text,
                        orig: Some(i),
                    }
                }
                None => Item {
                    text: format!(
                        "{}{colon}{}",
                        Value::String(k.clone()),
                        self.fresh(v, &indent)
                    ),
                    orig: None,
                },
            })
            .collect();
        self.container('{', '}', n, &spans, items, out);
    }

    fn array(&self, new: &[Value], n: &Node, nodes: &[Node], out: &mut String) {
        let spans: Vec<(usize, usize)> = nodes.iter().map(|m| (m.start, m.end)).collect();
        let (_, _, _, indent) = self.layout(n, &spans);
        let vals: Vec<Option<Value>> = nodes.iter().map(|m| self.value_of(m)).collect();
        let mut p = 0;
        let mut items = Vec::with_capacity(new.len());
        for (i, e) in new.iter().enumerate() {
            // The same value further on: what lies between was removed.
            if let Some(q) = (p..nodes.len()).find(|&q| vals[q].as_ref() == Some(e)) {
                items.push(Item {
                    text: self.slice(nodes[q].start, nodes[q].end).to_string(),
                    orig: Some(q),
                });
                p = q + 1;
                continue;
            }
            // Else the next original entry of the same kind, changed in
            // place, unless it is still to come unchanged.
            let same_kind = |v: &Value| {
                matches!(
                    (v, &nodes[p].kind),
                    (Value::Object(_), Kind::Object(_)) | (Value::Array(_), Kind::Array(_))
                )
            };
            if p < nodes.len()
                && same_kind(e)
                && !new[i + 1..]
                    .iter()
                    .any(|later| vals[p].as_ref() == Some(later))
            {
                let mut text = String::new();
                self.node(e, &nodes[p], &mut text);
                items.push(Item {
                    text,
                    orig: Some(p),
                });
                p += 1;
                continue;
            }
            items.push(Item {
                text: self.fresh(e, &indent),
                orig: None,
            });
        }
        self.container('[', ']', n, &spans, items, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edit(orig: &str, f: impl FnOnce(&mut Value)) -> String {
        let mut v = parse(orig).unwrap();
        f(&mut v);
        render(Some(orig), &v)
    }

    #[test]
    fn unchanged_is_identical() {
        let t = "{ \"a\" :1.50,\n\t\"b\": [1,2 , 3]}\n";
        assert_eq!(render(Some(t), &parse(t).unwrap()), t);
    }

    #[test]
    fn add_member_uses_file_indent() {
        let t = "{\n    \"a\": 1\n}\n";
        let out = edit(t, |v| v["b"] = json!({"c": [1]}));
        assert_eq!(
            out,
            "{\n    \"a\": 1,\n    \"b\": {\n        \"c\": [\n            1\n        ]\n    }\n}\n"
        );
    }

    #[test]
    fn remove_first_middle_last() {
        let t = "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}";
        assert_eq!(
            edit(t, |v| {
                v.as_object_mut().unwrap().shift_remove("a");
            }),
            "{\n  \"b\": 2,\n  \"c\": 3\n}"
        );
        assert_eq!(
            edit(t, |v| {
                v.as_object_mut().unwrap().shift_remove("b");
            }),
            "{\n  \"a\": 1,\n  \"c\": 3\n}"
        );
        assert_eq!(
            edit(t, |v| {
                v.as_object_mut().unwrap().shift_remove("c");
            }),
            "{\n  \"a\": 1,\n  \"b\": 2\n}"
        );
        assert_eq!(edit(t, |v| *v = json!({})), "{}");
    }

    #[test]
    fn compact_container_gets_compact_entries() {
        let t = "{\"a\": [1, 2]}";
        assert_eq!(
            edit(t, |v| v["a"].as_array_mut().unwrap().push(json!({"x": 1}))),
            "{\"a\": [1, 2, {\"x\":1}]}"
        );
    }

    #[test]
    fn array_element_changed_in_place() {
        let t = "[\n  {\"k\": [1, 2], \"z\": 0.10},\n  {\"m\": 1}\n]\n";
        let out = edit(t, |v| {
            v[0]["k"].as_array_mut().unwrap().pop();
        });
        assert_eq!(out, "[\n  {\"k\": [1], \"z\": 0.10},\n  {\"m\": 1}\n]\n");
    }

    #[test]
    fn scalar_replaced_and_root_change() {
        assert_eq!(
            edit("{\"a\": 1}", |v| v["a"] = json!("x")),
            "{\"a\": \"x\"}"
        );
        assert_eq!(edit("[1]", |v| *v = json!({"a": 1})), "{\n  \"a\": 1\n}");
    }

    #[test]
    fn crlf_lines_stay_crlf() {
        let t = "{\r\n  \"a\": 1,\r\n  \"e\": {}\r\n}\r\n";
        let out = edit(t, |v| {
            v["b"] = json!({"c": [1]});
            v["e"]["x"] = json!(2);
        });
        assert_eq!(
            out,
            "{\r\n  \"a\": 1,\r\n  \"e\": {\r\n    \"x\": 2\r\n  },\r\n  \"b\": {\r\n    \"c\": [\r\n      1\r\n    ]\r\n  }\r\n}\r\n"
        );
        assert_eq!(render(Some(&out), &parse(t).unwrap()), t);
        // A root replaced whole too.
        assert_eq!(
            edit("[1]\r\n", |v| *v = json!({"a": 1})),
            "{\r\n  \"a\": 1\r\n}\r\n"
        );
    }

    #[test]
    fn new_file_is_pretty() {
        assert_eq!(
            render(None, &json!({"a": [1]})),
            "{\n  \"a\": [\n    1\n  ]\n}\n"
        );
        assert_eq!(render(Some("  \n"), &json!({})), "{}\n");
    }

    #[test]
    fn comments_are_blanked() {
        assert_eq!(strip_comments("{\"a\": \"// no /* no */\"}"), None);
        assert_eq!(strip_comments("{\"a\\\"//\": 1}"), None);
        assert_eq!(
            strip_comments("{ // c\n \"a\": /* x\n y */ 1 }").as_deref(),
            Some("{     \n \"a\":     \n      1 }")
        );
        assert_eq!(strip_comments("/**/{}").as_deref(), Some("    {}"));
        assert_eq!(strip_comments("{} /* open").as_deref(), Some("{}        "));
        let v = parse(&strip_comments("{\r\n// c\r\n\"a\": 1}").unwrap()).unwrap();
        assert_eq!(v, json!({"a": 1}));
    }
}
