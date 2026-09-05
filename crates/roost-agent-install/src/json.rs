//! An **order-preserving** JSON value, and a printer that reuses the
//! file's own layout.
//!
//! Roost edits files it did not write. `serde_json::Value` cannot be
//! used for that: its `Map` is a `BTreeMap`, so parsing and re-emitting
//! a user's `settings.json` would silently sort every object in it —
//! a diff across the whole file for a two-line addition.
//!
//! The obvious fix, `serde_json`'s `preserve_order` feature, is not
//! available to us: Cargo unifies features across the workspace, so
//! turning it on here would swap `Map` for an `IndexMap` in *every*
//! crate of the build — including `roost-ipc`'s wire output. It already
//! breaks a shipped byte contract:
//! `roost-cli`'s `claude_settings_document_matches_the_shipped_file`
//! pins a frozen `to_string_pretty` literal whose keys are in sorted
//! order, which only holds while the map is a `BTreeMap`. Changing the
//! JSON key order of the IPC wire to make an install crate tidier is
//! not a trade worth making, so the ordered representation lives here
//! and stops at this crate's edge.
//!
//! What the type therefore guarantees, and what it does not: values and
//! key order survive a parse/print round-trip exactly; *bytes* do not.
//! [`Style`] recovers the three conventions that account for nearly all
//! of the difference — the indent unit, the line ending, and the
//! trailing newline — and the writers only ever render a document that
//! actually changed, so an unchanged file is never rewritten at all.
//!
//! # Why the parser is ours too
//!
//! `serde_json`'s `deserialize_any` hands a number to the visitor as a
//! `u64`, an `i64` or an `f64`, and there is no fourth case. An integer
//! past `u64::MAX` therefore arrives as an `f64` and goes back out as
//! `1.8446744073709552e19` — the file now *says something else*, which
//! is the one thing this crate may never do. `arbitrary_precision` would
//! fix it and, like `preserve_order`, is a workspace-wide feature that
//! changes `serde_json::Number` everywhere including the IPC wire.
//!
//! So numbers are kept as the **token the file spelled them with**, and
//! the parser that produces them is the ~150 lines below. The two parts
//! that are genuinely hard are still `serde_json`'s: string *decoding*
//! (escapes, `\uXXXX`, surrogate pairs) is delegated to it, and so is
//! string *escaping* on the way out. Everything the parser does itself —
//! whitespace, structure, literals, and the number grammar — is
//! mechanical, and the round-trip is pinned by tests over every value
//! class.
//!
//! Invalid UTF-8 is a parse **failure**, never a substitution: a
//! `from_utf8_lossy` here would put U+FFFD where the user's byte was and
//! the next write would persist it.

/// A JSON value that remembers the order its object keys arrived in.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

/// A number, kept verbatim as the file spelled it.
///
/// Equality is over the token, so `1` and `1.0` are different numbers
/// here. That is deliberate: this type exists to answer "would writing
/// this change the file", and rewriting `1.0` as `1` changes the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Number(String);

impl Number {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Number {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<u64> for Number {
    fn from(value: u64) -> Number {
        Number(value.to_string())
    }
}

impl From<i64> for Number {
    fn from(value: i64) -> Number {
        Number(value.to_string())
    }
}

/// Why a document could not be read. Rendered into
/// [`crate::SkipReason::Unparseable`], so it is what the user sees when
/// Roost declines to touch a file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} (byte {offset})")]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl Json {
    /// An empty JSON object — what an absent file stands in for.
    pub fn object() -> Json {
        Json::Object(Vec::new())
    }

    pub fn parse(bytes: &[u8]) -> Result<Json, ParseError> {
        let text = std::str::from_utf8(bytes).map_err(|e| ParseError {
            message: "not valid UTF-8".to_string(),
            offset: e.valid_up_to(),
        })?;
        let mut parser = Parser {
            src: text,
            pos: 0,
            depth: 0,
        };
        parser.skip_whitespace();
        let value = parser.value()?;
        parser.skip_whitespace();
        if parser.pos != text.len() {
            return Err(parser.error("trailing characters after the document"));
        }
        Ok(value)
    }

    pub fn as_object(&self) -> Option<&Vec<(String, Json)>> {
        match self {
            Json::Object(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut Vec<(String, Json)>> {
        match self {
            Json::Object(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Json>> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        self.as_object()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        self.as_object_mut()?
            .iter_mut()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    /// The value at `key`, inserting `default` **at the end** when the
    /// key is absent. Existing keys keep their position, which is the
    /// whole point of this type.
    pub fn entry(&mut self, key: &str, default: Json) -> Option<&mut Json> {
        let entries = self.as_object_mut()?;
        if let Some(index) = entries.iter().position(|(k, _)| k == key) {
            return Some(&mut entries[index].1);
        }
        entries.push((key.to_string(), default));
        entries.last_mut().map(|(_, v)| v)
    }

    /// Append `(key, value)`, or replace the value of an existing key in
    /// place. Mirrors `IndexMap::insert`.
    pub fn insert(&mut self, key: &str, value: Json) {
        let Some(entries) = self.as_object_mut() else {
            return;
        };
        match entries.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => entries.push((key.to_string(), value)),
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Json> {
        let entries = self.as_object_mut()?;
        let index = entries.iter().position(|(k, _)| k == key)?;
        Some(entries.remove(index).1)
    }

    pub fn is_empty_object(&self) -> bool {
        matches!(self, Json::Object(entries) if entries.is_empty())
    }

    /// The document as text, in `style`.
    pub fn render(&self, style: &Style) -> String {
        let mut out = String::new();
        self.write(&mut out, style, 0);
        if style.trailing_newline {
            out.push_str(&style.newline);
        }
        out
    }

    fn write(&self, out: &mut String, style: &Style, depth: usize) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Number(n) => out.push_str(n.as_str()),
            // Delegated so the escaping is `serde_json`'s own, byte for
            // byte: short forms for the control characters it has them
            // for, `\u00XX` otherwise, `/` left alone, and non-ASCII
            // emitted raw.
            Json::String(s) => out.push_str(&serde_json::Value::String(s.clone()).to_string()),
            Json::Array(items) if items.is_empty() => out.push_str("[]"),
            Json::Array(items) => {
                out.push('[');
                out.push_str(&style.newline);
                for (i, item) in items.iter().enumerate() {
                    style.indent_to(out, depth + 1);
                    item.write(out, style, depth + 1);
                    if i + 1 < items.len() {
                        out.push(',');
                    }
                    out.push_str(&style.newline);
                }
                style.indent_to(out, depth);
                out.push(']');
            }
            Json::Object(entries) if entries.is_empty() => out.push_str("{}"),
            Json::Object(entries) => {
                out.push('{');
                out.push_str(&style.newline);
                for (i, (key, value)) in entries.iter().enumerate() {
                    style.indent_to(out, depth + 1);
                    out.push_str(&serde_json::Value::String(key.clone()).to_string());
                    out.push_str(": ");
                    value.write(out, style, depth + 1);
                    if i + 1 < entries.len() {
                        out.push(',');
                    }
                    out.push_str(&style.newline);
                }
                style.indent_to(out, depth);
                out.push('}');
            }
        }
    }
}

/// How deep a document may nest before it is refused. `serde_json`'s own
/// default, and the reason a hostile file cannot blow this recursive
/// descent off the stack.
const MAX_DEPTH: usize = 128;

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn bytes(&self) -> &'a [u8] {
        self.src.as_bytes()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes().get(self.pos).copied()
    }

    fn error(&self, message: &str) -> ParseError {
        ParseError {
            message: message.to_string(),
            offset: self.pos,
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, byte: u8, what: &str) -> Result<(), ParseError> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            return Ok(());
        }
        Err(self.error(what))
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, ParseError> {
        if self.src[self.pos..].starts_with(word) {
            self.pos += word.len();
            return Ok(value);
        }
        Err(self.error("expected a value"))
    }

    fn value(&mut self) -> Result<Json, ParseError> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::String(self.string()?)),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("expected a value")),
            None => Err(self.error("unexpected end of document")),
        }
    }

    fn nested<T>(
        &mut self,
        f: impl FnOnce(&mut Parser<'a>) -> Result<T, ParseError>,
    ) -> Result<T, ParseError> {
        if self.depth >= MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        self.depth += 1;
        let out = f(self);
        self.depth -= 1;
        out
    }

    fn object(&mut self) -> Result<Json, ParseError> {
        self.nested(|p| {
            p.pos += 1; // '{'
            let mut entries: Vec<(String, Json)> = Vec::new();
            p.skip_whitespace();
            if p.peek() == Some(b'}') {
                p.pos += 1;
                return Ok(Json::Object(entries));
            }
            loop {
                p.skip_whitespace();
                let key = p.string()?;
                p.skip_whitespace();
                p.expect(b':', "expected `:` after an object key")?;
                p.skip_whitespace();
                let value = p.value()?;
                // A duplicate key keeps its first position and takes the
                // last value — `IndexMap::insert`'s rule, and the one
                // every JSON reader in this stack agrees on, the agents'
                // own included.
                match entries.iter_mut().find(|(k, _)| *k == key) {
                    Some(slot) => slot.1 = value,
                    None => entries.push((key, value)),
                }
                p.skip_whitespace();
                match p.peek() {
                    Some(b',') => p.pos += 1,
                    Some(b'}') => {
                        p.pos += 1;
                        return Ok(Json::Object(entries));
                    }
                    _ => return Err(p.error("expected `,` or `}` in an object")),
                }
            }
        })
    }

    fn array(&mut self) -> Result<Json, ParseError> {
        self.nested(|p| {
            p.pos += 1; // '['
            let mut items = Vec::new();
            p.skip_whitespace();
            if p.peek() == Some(b']') {
                p.pos += 1;
                return Ok(Json::Array(items));
            }
            loop {
                p.skip_whitespace();
                items.push(p.value()?);
                p.skip_whitespace();
                match p.peek() {
                    Some(b',') => p.pos += 1,
                    Some(b']') => {
                        p.pos += 1;
                        return Ok(Json::Array(items));
                    }
                    _ => return Err(p.error("expected `,` or `]` in an array")),
                }
            }
        })
    }

    /// Find the string token's extent, then let `serde_json` decode it.
    ///
    /// The scan only has to know that a backslash protects the next
    /// byte; escapes, `\uXXXX` and surrogate pairs are exactly the part
    /// worth not reimplementing. UTF-8 continuation bytes are all
    /// `>= 0x80`, so scanning for ASCII `"` and `\` cannot land inside a
    /// multi-byte character.
    fn string(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        self.expect(b'"', "expected a string")?;
        loop {
            match self.peek() {
                None => return Err(self.error("unterminated string")),
                Some(b'\\') => {
                    self.pos += 1;
                    if self.peek().is_none() {
                        return Err(self.error("unterminated escape"));
                    }
                    self.pos += 1;
                }
                Some(b'"') => {
                    self.pos += 1;
                    break;
                }
                Some(c) if c < 0x20 => {
                    return Err(self.error("a raw control character in a string"))
                }
                Some(_) => self.pos += 1,
            }
        }
        serde_json::from_str::<String>(&self.src[start..self.pos]).map_err(|e| ParseError {
            message: e.to_string(),
            offset: start,
        })
    }

    /// The JSON number grammar, kept as text.
    ///
    /// Nothing here converts: the token is the value, so a number too
    /// big for any Rust integer, and a spelling like `1.50` or `1e+2`,
    /// come back out exactly as the user wrote them.
    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => self.digits(),
            _ => return Err(self.error("expected a digit")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected a digit after `.`"));
            }
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected a digit in the exponent"));
            }
            self.digits();
        }
        Ok(Json::Number(Number(self.src[start..self.pos].to_string())))
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
    }
}

/// The three layout conventions a JSON file carries that a re-serialize
/// would otherwise destroy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Style {
    pub indent: String,
    /// `"\n"` or `"\r\n"`. A file written on Windows, or by an editor
    /// configured that way, is not ours to convert: rewriting every line
    /// ending turns a two-line addition into a whole-file diff.
    pub newline: String,
    pub trailing_newline: bool,
}

impl Default for Style {
    /// What a file Roost creates looks like: two spaces, LF, and a final
    /// newline, matching what Claude, codex and cursor all write.
    fn default() -> Style {
        Style {
            indent: "  ".to_string(),
            newline: "\n".to_string(),
            trailing_newline: true,
        }
    }
}

impl Style {
    /// Read the conventions back off an existing file.
    ///
    /// The first indented line of a pretty-printed JSON document is at
    /// depth one, so its leading whitespace run *is* the indent unit.
    /// A file with nothing indented (compact, or a single scalar) keeps
    /// the default — there is nothing to preserve.
    pub fn detect(text: &str) -> Style {
        let indent = text
            .lines()
            .find_map(|line| {
                let run: String = line
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                (!run.is_empty() && run.len() < line.len()).then_some(run)
            })
            .unwrap_or_else(|| "  ".to_string());
        // The *first* line ending decides, so a file that is CRLF
        // throughout stays CRLF and a mixed one is normalised to
        // whichever it opened with.
        let newline = match text.find('\n') {
            Some(at) if at > 0 && text.as_bytes()[at - 1] == b'\r' => "\r\n",
            _ => "\n",
        };
        Style {
            indent,
            newline: newline.to_string(),
            trailing_newline: text.ends_with('\n'),
        }
    }

    fn indent_to(&self, out: &mut String, depth: usize) {
        for _ in 0..depth {
            out.push_str(&self.indent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reason this type exists at all. `serde_json::Value` sorts;
    /// this must not.
    #[test]
    fn object_key_order_survives_a_round_trip() {
        let text = r#"{"zebra":1,"apple":2,"mango":{"yak":3,"ant":4}}"#;
        let parsed = Json::parse(text.as_bytes()).unwrap();
        let keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, ["zebra", "apple", "mango"]);

        let inner: Vec<&str> = parsed
            .get("mango")
            .unwrap()
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(inner, ["yak", "ant"]);

        // And the same order comes back out of the printer.
        let rendered = parsed.render(&Style {
            indent: String::new(),
            newline: "\n".to_string(),
            trailing_newline: false,
        });
        assert!(rendered.find("\"zebra\"") < rendered.find("\"apple\""));
    }

    #[test]
    fn a_pretty_two_space_file_round_trips_byte_identically() {
        let text = "{\n  \"a\": [\n    1,\n    {\n      \"b\": \"x\"\n    }\n  ],\n  \"c\": {},\n  \"d\": []\n}\n";
        let parsed = Json::parse(text.as_bytes()).unwrap();
        assert_eq!(parsed.render(&Style::detect(text)), text);
    }

    #[test]
    fn a_four_space_file_keeps_its_indent() {
        let text = "{\n    \"a\": {\n        \"b\": 1\n    }\n}\n";
        assert_eq!(Style::detect(text).indent, "    ");
        let parsed = Json::parse(text.as_bytes()).unwrap();
        assert_eq!(parsed.render(&Style::detect(text)), text);
    }

    #[test]
    fn a_tab_indented_file_keeps_its_tabs() {
        let text = "{\n\t\"a\": {\n\t\t\"b\": 1\n\t}\n}";
        let style = Style::detect(text);
        assert_eq!(style.indent, "\t");
        assert!(!style.trailing_newline);
        let parsed = Json::parse(text.as_bytes()).unwrap();
        assert_eq!(parsed.render(&style), text);
    }

    /// Non-ASCII stays raw and the escapes are `serde_json`'s, because
    /// the printer hands strings straight to it.
    #[test]
    fn strings_are_escaped_the_way_serde_json_escapes_them() {
        let parsed = Json::parse(r#"{"kéy":"café / tab\there"}"#.as_bytes()).unwrap();
        let rendered = parsed.render(&Style {
            indent: String::new(),
            newline: "\n".to_string(),
            trailing_newline: false,
        });
        assert!(rendered.contains("\"kéy\""), "{rendered}");
        assert!(rendered.contains("café / tab\\there"), "{rendered}");
    }

    #[test]
    fn numbers_keep_their_json_spelling() {
        let parsed = Json::parse(br#"{"i":10,"f":1.5,"neg":-3,"big":9007199254740993}"#).unwrap();
        let rendered = parsed.render(&Style {
            indent: String::new(),
            newline: "\n".to_string(),
            trailing_newline: false,
        });
        for expected in ["10", "1.5", "-3", "9007199254740993"] {
            assert!(
                rendered.contains(expected),
                "{expected} missing: {rendered}"
            );
        }
    }

    /// A number the user wrote is a token, not a value: an integer past
    /// `u64` is the case where re-deriving it from an `f64` changes what
    /// the file *says*, and a token like `1.50` or `1.0e-7` is a
    /// spelling nobody asked us to normalise.
    #[test]
    fn a_number_keeps_the_spelling_the_user_wrote() {
        for token in [
            "18446744073709551617",
            "-18446744073709551617",
            "1.50",
            "1.0e-7",
            "1E+2",
            "0.30000000000000004",
            "-0",
            "1e999",
        ] {
            let src = format!("{{\"n\":{token}}}");
            let parsed = Json::parse(src.as_bytes()).unwrap();
            let rendered = parsed.render(&Style::default());
            assert!(
                rendered.contains(&format!("\"n\": {token}\n")),
                "{token} became {rendered}"
            );
            assert_eq!(Json::parse(rendered.as_bytes()).unwrap(), parsed);
        }
    }

    /// A file that does not decode as UTF-8 is not a JSON file. It must
    /// never come back with U+FFFD where the user's bytes were.
    #[test]
    fn invalid_utf8_is_a_parse_failure_never_a_substitution() {
        let mut bytes = br#"{"token":"abc"#.to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(br#"def"}"#);
        assert!(Json::parse(&bytes).is_err(), "invalid UTF-8 parsed");
    }

    /// CRLF is a convention a hand-edited file carries, and rewriting it
    /// to LF is a whole-file diff for a two-line addition.
    #[test]
    fn crlf_line_endings_are_preserved() {
        let text = "{\r\n  \"a\": [\r\n    1\r\n  ]\r\n}\r\n";
        let parsed = Json::parse(text.as_bytes()).unwrap();
        assert_eq!(parsed.render(&Style::detect(text)), text);
    }

    #[test]
    fn entry_appends_but_never_reorders() {
        let mut doc = Json::parse(br#"{"b":1,"a":2}"#).unwrap();
        doc.entry("c", Json::object());
        doc.entry("b", Json::Null);
        let keys: Vec<&str> = doc
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, ["b", "a", "c"]);
        assert_eq!(doc.get("b"), Some(&Json::Number(1u64.into())));
    }

    #[test]
    fn a_duplicate_key_keeps_the_first_position_and_the_last_value() {
        let doc = Json::parse(br#"{"a":1,"b":2,"a":3}"#).unwrap();
        let keys: Vec<&str> = doc
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, ["a", "b"]);
        assert_eq!(doc.get("a"), Some(&Json::Number(3u64.into())));
    }

    /// A hand-written `Deserialize` is the one place a user's value can
    /// quietly change shape, so every JSON value class gets walked —
    /// integers past `f64`'s exact range in both directions, exponent
    /// and negative-zero floats, both spellings of unicode, empty
    /// containers, and the non-object roots an agent config should
    /// never have but might.
    #[test]
    fn every_json_value_class_survives_a_round_trip() {
        let cases: &[&str] = &[
            r#"{"big":9007199254740993,"neg":-9007199254740993}"#,
            r#"{"u64max":18446744073709551615,"i64min":-9223372036854775808}"#,
            r#"{"float":1.0,"int":1,"exp":1e100,"small":1.0e-7,"negzero":-0.0}"#,
            r#"{"esc":"line\nbreak\ttab\\slash\"quote","solidus":"a\/b"}"#,
            r#"{"raw":"cafe 1 JP","escaped":"\u0041\u00e9\u0000"}"#,
            r#"{"empty_obj":{},"empty_arr":[],"null":null,"t":true,"f":false}"#,
            r#"{"nested":{"a":{"b":{"c":[1,[2,[3]]]}}}}"#,
            r#"[]"#,
            r#"{}"#,
            r#""a bare string""#,
            r#"42"#,
            r#"null"#,
        ];

        for src in cases {
            let parsed =
                Json::parse(src.as_bytes()).unwrap_or_else(|e| panic!("{src} did not parse: {e}"));
            let rendered = parsed.render(&Style::detect(src));
            let before: serde_json::Value = serde_json::from_str(src).unwrap();
            let after: serde_json::Value = serde_json::from_str(&rendered)
                .unwrap_or_else(|e| panic!("{src} rendered unparseable {rendered}: {e}"));
            assert_eq!(before, after, "{src} became {rendered}");
        }
    }

    /// CRLF and a BOM are both things a hand-edited config really
    /// carries. Either the file round-trips, or it is refused outright
    /// and skipped upstream — what it must never do is parse and come
    /// back with different content.
    #[test]
    fn crlf_and_a_bom_either_round_trip_or_are_refused() {
        for src in [
            "{\r\n  \"a\": 1\r\n}\r\n",
            "\u{feff}{\n  \"a\": 1\n}\n",
            "{\n  \"a\": 1\n}",
        ] {
            let Ok(parsed) = Json::parse(src.as_bytes()) else {
                continue;
            };
            let rendered = parsed.render(&Style::detect(src));
            let after: serde_json::Value = serde_json::from_str(&rendered)
                .unwrap_or_else(|e| panic!("{src:?} rendered unparseable {rendered:?}: {e}"));
            assert_eq!(
                after["a"],
                serde_json::json!(1),
                "{src:?} became {rendered:?}"
            );
        }
    }
}
