//! Valve's text KeyValues ("VDF") reader and writer.
//!
//! Steam uses this format for `libraryfolders.vdf`, `appmanifest_*.acf`, and dozens of
//! configuration files. The grammar is small: an object is a sequence of key/value pairs,
//! keys and values are usually double quoted, and a value may be a nested object.
//!
//! The parser accepts the extensions Steam actually ships:
//!
//! - `//` line comments,
//! - backslash escapes (`\\`, `\"`, `\n`, `\r`, `\t`),
//! - bare (unquoted) keys and values as a fallback,
//! - conditional tags such as `[$WIN32]` (parsed and included unconditionally).
//!
//! Keys are matched case-insensitively by the lookup helpers, mirroring Steam and the C#
//! implementation this crate ports.

use std::path::Path;

use indexmap::IndexMap;
use luatools_core::{Error, Result};

/// Maximum nesting depth accepted by the parser.
const MAX_DEPTH: usize = 256;

/// A single value in a text VDF object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VdfValue {
    /// A scalar value. Steam stores scalars as text even when they represent numbers.
    String(String),
    /// A nested object.
    Object(Vdf),
}

impl VdfValue {
    /// Returns the value as a string slice, when it is a scalar.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            Self::Object(_) => None,
        }
    }

    /// Returns the nested object, when this value is one.
    #[must_use]
    pub const fn as_object(&self) -> Option<&Vdf> {
        match self {
            Self::Object(value) => Some(value),
            Self::String(_) => None,
        }
    }

    /// Parses the scalar as an unsigned integer, trimming surrounding whitespace.
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        self.as_str()?.trim().parse().ok()
    }

    /// Parses the scalar as a signed integer, trimming surrounding whitespace.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        self.as_str()?.trim().parse().ok()
    }
}

/// A text VDF object: an ordered map of keys to scalar or nested values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vdf {
    entries: IndexMap<String, VdfValue>,
}

impl Vdf {
    /// Creates an empty object.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty object with room for `capacity` entries.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: IndexMap::with_capacity(capacity),
        }
    }

    /// Inserts or replaces an entry, returning the previous value if the key existed.
    ///
    /// Replacement preserves the original position in the object, matching
    /// `IndexMap` insertion semantics.
    pub fn insert(&mut self, key: impl Into<String>, value: VdfValue) -> Option<VdfValue> {
        self.entries.insert(key.into(), value)
    }

    /// Inserts a scalar entry.
    pub fn insert_str(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.insert(key, VdfValue::String(value.into()));
    }

    /// Inserts a nested object entry.
    pub fn insert_table(&mut self, key: impl Into<String>, value: Vdf) {
        self.insert(key, VdfValue::Object(value));
    }

    /// Returns a mutable entry by key, compared ASCII case-insensitively.
    pub fn get_key_mut(&mut self, key: &str) -> Option<&mut VdfValue> {
        self.entries
            .iter_mut()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    }

    /// Returns the nested object at `path`, creating missing levels as needed.
    ///
    /// Existing keys are matched case-insensitively and keep their original spelling, so
    /// editing a real Steam file does not rewrite unrelated key casing.
    pub fn obtain_table(&mut self, path: &str) -> &mut Vdf {
        let mut current = self;
        for key in path.split('.').filter(|segment| !segment.is_empty()) {
            let existing = current
                .entries
                .keys()
                .find(|candidate| candidate.eq_ignore_ascii_case(key))
                .cloned();
            let key = existing.unwrap_or_else(|| key.to_owned());
            let value = current
                .entries
                .entry(key)
                .or_insert_with(|| VdfValue::Object(Vdf::new()));
            if !matches!(value, VdfValue::Object(_)) {
                *value = VdfValue::Object(Vdf::new());
            }
            current = match value {
                VdfValue::Object(table) => table,
                VdfValue::String(_) => unreachable!("value was just replaced with a table"),
            };
        }
        current
    }

    /// Removes an entry by key, returning the previous value.
    pub fn remove_key(&mut self, key: &str) -> Option<VdfValue> {
        let existing = self
            .entries
            .keys()
            .find(|candidate| candidate.eq_ignore_ascii_case(key))
            .cloned();
        existing.and_then(|key| self.entries.shift_remove(&key))
    }

    /// Returns an entry by exact key, compared ASCII case-insensitively.
    #[must_use]
    pub fn get_key(&self, key: &str) -> Option<&VdfValue> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    }

    /// Returns an entry by a dotted path such as `libraryfolders.0.path`.
    ///
    /// Every segment is compared ASCII case-insensitively. Numeric segments address
    /// Steam's numbered object keys, not array indexes.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&VdfValue> {
        if path.is_empty() {
            return None;
        }

        let mut current = self;
        let mut segments = path.split('.').peekable();
        while let Some(segment) = segments.next() {
            let value = current.get_key(segment)?;
            if segments.peek().is_some() {
                current = value.as_object()?;
            } else {
                return Some(value);
            }
        }
        None
    }

    /// Returns a nested object by dotted path.
    #[must_use]
    pub fn table(&self, path: &str) -> Option<&Vdf> {
        self.get(path)?.as_object()
    }

    /// Returns a scalar by dotted path.
    #[must_use]
    pub fn as_str(&self, path: &str) -> Option<&str> {
        self.get(path)?.as_str()
    }

    /// Parses a scalar at the given dotted path as an unsigned integer.
    #[must_use]
    pub fn as_u64(&self, path: &str) -> Option<u64> {
        self.get(path)?.as_u64()
    }

    /// Parses a scalar at the given dotted path as a signed integer.
    #[must_use]
    pub fn as_i64(&self, path: &str) -> Option<i64> {
        self.get(path)?.as_i64()
    }

    /// Parses a scalar at the given dotted path as a boolean.
    ///
    /// `1`, `true`, `yes`, and `on` are true; `0`, `false`, `no`, and `off` are false.
    /// Comparison is ASCII case-insensitive.
    #[must_use]
    pub fn as_bool(&self, path: &str) -> Option<bool> {
        let value = self.as_str(path)?.trim();
        if value.eq_ignore_ascii_case("1")
            || value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("yes")
            || value.eq_ignore_ascii_case("on")
        {
            Some(true)
        } else if value.eq_ignore_ascii_case("0")
            || value.eq_ignore_ascii_case("false")
            || value.eq_ignore_ascii_case("no")
            || value.eq_ignore_ascii_case("off")
        {
            Some(false)
        } else {
            None
        }
    }

    /// Iterates the entries in file order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &VdfValue)> + '_ {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }

    /// Number of entries in the object.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true when the object has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Parses one text VDF object from a string.
///
/// A UTF-8 byte-order mark at the start of the input is ignored.
///
/// # Errors
///
/// Returns [`luatools_core::Error::Parse`] with the offending line when the input has an
/// unterminated string or object, a missing value, or an unexpected brace.
pub fn parse(text: &str) -> Result<Vdf> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut parser = Parser { text, pos: 0 };
    parser.parse_table(0, false)
}

/// Reads and parses a text VDF file.
///
/// # Errors
///
/// Returns an I/O error when the file cannot be read, and a parse error when it is not
/// valid UTF-8 or not valid VDF.
pub fn parse_file(path: &Path) -> Result<Vdf> {
    let bytes = std::fs::read(path)
        .map_err(|error| Error::io(format!("reading VDF file {}", path.display()), error))?;
    let text = std::str::from_utf8(&bytes).map_err(|error| {
        Error::parse(
            "vdf",
            format!(
                "{} is not valid UTF-8 at byte {}",
                path.display(),
                error.valid_up_to()
            ),
        )
    })?;
    parse(text)
}

/// Renders an object in Valve's tab-indented style.
///
/// Scalars are written as `"key"\t\t"value"`; nested objects use braces on their own lines
/// and one tab of indentation per depth. The output ends with a newline.
#[must_use]
pub fn to_string_pretty(value: &Vdf) -> String {
    let mut output = String::new();
    write_table(value, 0, &mut output);
    output
}

fn write_table(table: &Vdf, indent: usize, output: &mut String) {
    for (key, value) in table.entries() {
        write_indent(indent, output);
        write_quoted(key, output);
        match value {
            VdfValue::String(text) => {
                output.push('\t');
                output.push('\t');
                write_quoted(text, output);
                output.push('\n');
            }
            VdfValue::Object(nested) => {
                output.push('\n');
                write_indent(indent, output);
                output.push_str("{\n");
                write_table(nested, indent + 1, output);
                write_indent(indent, output);
                output.push_str("}\n");
            }
        }
    }
}

fn write_indent(indent: usize, output: &mut String) {
    for _ in 0..indent {
        output.push('\t');
    }
}

fn write_quoted(text: &str, output: &mut String) {
    output.push('"');
    for ch in text.chars() {
        match ch {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            other => output.push(other),
        }
    }
    output.push('"');
}

struct Parser<'a> {
    text: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn parse_table(&mut self, depth: usize, nested: bool) -> Result<Vdf> {
        if depth > MAX_DEPTH {
            return Err(self.error("object nesting exceeds the supported depth"));
        }

        let mut table = Vdf::new();
        loop {
            self.skip_trivia();
            let Some(ch) = self.peek() else {
                if nested {
                    return Err(self.error("unterminated object"));
                }
                return Ok(table);
            };

            if ch == '}' {
                if nested {
                    self.pos += 1;
                    return Ok(table);
                }
                return Err(self.error("unexpected '}'"));
            }
            if ch == '[' {
                self.skip_conditional()?;
                continue;
            }
            if ch == '{' {
                return Err(self.error("unexpected '{'"));
            }

            let key = self.parse_key()?;
            self.skip_trivia();

            match self.peek() {
                Some('{') => {
                    self.pos += 1;
                    let child = self.parse_table(depth + 1, true)?;
                    table.insert(key, VdfValue::Object(child));
                }
                Some('}') | None => {
                    return Err(self.error("key has no value"));
                }
                Some(_) => {
                    let value = self.parse_scalar()?;
                    table.insert(key, VdfValue::String(value));
                }
            }
        }
    }

    fn skip_conditional(&mut self) -> Result<()> {
        let rest = self.text.get(self.pos..).unwrap_or_default();
        let Some(end) = rest.find(']') else {
            return Err(self.error("unterminated conditional tag"));
        };
        self.pos += end + 1;
        Ok(())
    }

    fn parse_key(&mut self) -> Result<String> {
        match self.peek() {
            Some('"') => self.parse_quoted(),
            Some(_) => self.parse_bare(),
            None => Err(self.error("expected a key")),
        }
    }

    fn parse_scalar(&mut self) -> Result<String> {
        match self.peek() {
            Some('"') => self.parse_quoted(),
            Some(_) => self.parse_bare(),
            None => Err(self.error("expected a value")),
        }
    }

    fn parse_quoted(&mut self) -> Result<String> {
        self.pos += 1;
        let mut value = String::new();
        loop {
            let Some(ch) = self.peek() else {
                return Err(self.error("unterminated quoted string"));
            };
            match ch {
                '"' => {
                    self.pos += 1;
                    return Ok(value);
                }
                '\\' => {
                    self.pos += 1;
                    let Some(escaped) = self.peek() else {
                        return Err(self.error("trailing backslash in quoted string"));
                    };
                    self.pos += escaped.len_utf8();
                    match escaped {
                        '\\' => value.push('\\'),
                        '"' => value.push('"'),
                        'n' => value.push('\n'),
                        'r' => value.push('\r'),
                        't' => value.push('\t'),
                        other => {
                            // Keep unknown escapes intact; the C# reader only unescapes `\\`.
                            value.push('\\');
                            value.push(other);
                        }
                    }
                }
                other => {
                    self.pos += other.len_utf8();
                    value.push(other);
                }
            }
        }
    }

    fn parse_bare(&mut self) -> Result<String> {
        let start = self.pos;
        while let Some(ch) = self.peek() {
            if ch.is_ascii_whitespace() || ch == '{' || ch == '}' || ch == '"' {
                break;
            }
            if self
                .text
                .get(self.pos..)
                .is_some_and(|rest| rest.starts_with("//"))
            {
                break;
            }
            self.pos += ch.len_utf8();
        }

        let token = self.text.get(start..self.pos).unwrap_or_default();
        if token.is_empty() {
            return Err(self.error("expected a token"));
        }
        Ok(token.to_owned())
    }

    fn skip_trivia(&mut self) {
        loop {
            let rest = self.text.get(self.pos..).unwrap_or_default();
            let trimmed = rest.trim_start_matches(|ch: char| ch.is_ascii_whitespace());
            self.pos += rest.len() - trimmed.len();

            if self
                .text
                .get(self.pos..)
                .is_some_and(|rest| rest.starts_with("//"))
            {
                let rest = self.text.get(self.pos..).unwrap_or_default();
                match rest.find('\n') {
                    Some(offset) => self.pos += offset + 1,
                    None => self.pos = self.text.len(),
                }
                continue;
            }
            return;
        }
    }

    fn peek(&self) -> Option<char> {
        self.text.get(self.pos..)?.chars().next()
    }

    fn error(&self, message: &str) -> Error {
        let consumed = self.text.get(..self.pos).unwrap_or_default();
        let line = consumed.bytes().filter(|byte| *byte == b'\n').count() + 1;
        Error::parse("vdf", format!("{message} at line {line}"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
// Steam library list
"libraryfolders"
{
    "0"
    {
        "path"        "C:\\Program Files (x86)\\Steam"
        "label"       ""
        "apps"
        {
            "730"     "123456"
        }
    }
    "1"
    {
        "path"        "/home/player/.steam/steam"
        "contentid"   "9007199254740993"
    }
}
"#;

    #[test]
    fn parses_a_known_valve_vector() {
        let vdf = parse(SAMPLE).unwrap();
        assert_eq!(vdf.len(), 1);
        assert_eq!(
            vdf.as_str("libraryfolders.0.path"),
            Some("C:\\Program Files (x86)\\Steam")
        );
        assert_eq!(
            vdf.as_str("LibraryFolders.1.Path"),
            Some("/home/player/.steam/steam")
        );
        assert_eq!(
            vdf.as_u64("libraryfolders.1.contentid"),
            Some(9_007_199_254_740_993)
        );
        assert_eq!(vdf.as_u64("libraryfolders.0.apps.730"), Some(123_456));
        assert_eq!(vdf.as_str("libraryfolders.0.label"), Some(""));
        assert!(vdf.as_str("libraryfolders.9.path").is_none());
    }

    #[test]
    fn pretty_output_is_stable_and_round_trips() {
        let vdf = parse(
            r#"
"AppState"
{
  "appid" "730"
  "name" "A \"quoted\" name\nwith escapes"
  "nested" { "value" "42" }
}
"#,
        )
        .unwrap();
        let rendered = to_string_pretty(&vdf);
        assert_eq!(
            rendered,
            "\"AppState\"\n{\n\t\"appid\"\t\t\"730\"\n\t\"name\"\t\t\"A \\\"quoted\\\" name\\nwith escapes\"\n\t\"nested\"\n\t{\n\t\t\"value\"\t\t\"42\"\n\t}\n}\n"
        );

        let reparsed = parse(&rendered).unwrap();
        assert_eq!(
            reparsed.as_str("AppState.name"),
            Some("A \"quoted\" name\nwith escapes")
        );
        assert_eq!(reparsed.as_u64("AppState.nested.value"), Some(42));
    }

    #[test]
    fn accepts_bom_comments_and_bare_tokens() {
        let vdf = parse("\u{feff}// heading\nkey value // trailing\n").unwrap();
        assert_eq!(vdf.as_str("key"), Some("value"));
    }

    #[test]
    fn reports_unterminated_objects_and_strings() {
        let error = parse("\"a\" { \"b\" \"c\"").unwrap_err().to_string();
        assert!(error.contains("unterminated object"), "{error}");

        let error = parse("\"a\" \"unterminated").unwrap_err().to_string();
        assert!(error.contains("unterminated quoted string"), "{error}");
    }

    #[test]
    fn parse_file_strips_a_utf8_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("libraryfolders.vdf");
        std::fs::write(
            &path,
            b"\xEF\xBB\xBF\"libraryfolders\" { \"0\" { \"path\" \"/steam\" } }",
        )
        .unwrap();

        let vdf = parse_file(&path).unwrap();
        assert_eq!(vdf.as_str("libraryfolders.0.path"), Some("/steam"));
    }
}
