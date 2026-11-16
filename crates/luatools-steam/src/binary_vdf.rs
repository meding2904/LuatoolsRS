//! Valve's binary KeyValues codec, used inside `appinfo.vdf`.
//!
//! A binary VDF object is an ordered list of properties, not a map: real `appinfo.vdf`
//! files contain duplicate keys, and the blob is hashed, so both order and duplicates must
//! survive a read/write cycle. String values are kept as raw bytes because a handful of old
//! apps contain text that is not valid UTF-8.
//!
//! Supported value tags are the ones the C# implementation reads and writes: table,
//! string, `i32`, `f32`, pointer, color, `u64`, and `i64`. `WString` (0x05) is rejected.

use luatools_core::{Error, Result};

/// Maximum object nesting accepted by the parser.
const MAX_DEPTH: usize = 256;

/// One binary VDF value.
#[derive(Debug, Clone, PartialEq)]
pub enum BvdfValue {
    /// A nested object.
    Table(Bvdf),
    /// A raw, NUL-free string value. Kept as bytes so non-UTF-8 values round-trip exactly.
    String(Vec<u8>),
    /// A little-endian signed 32-bit integer.
    Int32(i32),
    /// A little-endian IEEE-754 32-bit float.
    Float(f32),
    /// A pointer-sized integer carried with its original type tag.
    Pointer(i32),
    /// A color integer carried with its original type tag.
    Color(i32),
    /// A little-endian unsigned 64-bit integer.
    UInt64(u64),
    /// A little-endian signed 64-bit integer.
    Int64(i64),
}

impl BvdfValue {
    /// Returns the nested object, when this is a table.
    #[must_use]
    pub const fn as_table(&self) -> Option<&Bvdf> {
        match self {
            Self::Table(table) => Some(table),
            _ => None,
        }
    }

    /// Returns the nested object mutably, when this is a table.
    pub fn as_table_mut(&mut self) -> Option<&mut Bvdf> {
        match self {
            Self::Table(table) => Some(table),
            _ => None,
        }
    }

    /// Renders the value for display.
    ///
    /// String bytes are decoded lossily, matching the C# `AsText` helper. Tables render as
    /// an empty string because their `ToString` was never used meaningfully.
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            Self::String(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            Self::Int32(value) | Self::Pointer(value) | Self::Color(value) => value.to_string(),
            Self::Float(value) => value.to_string(),
            Self::UInt64(value) => value.to_string(),
            Self::Int64(value) => value.to_string(),
            Self::Table(_) => String::new(),
        }
    }

    /// Returns the raw string bytes, when this is a string value.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::String(bytes) => Some(bytes),
            _ => None,
        }
    }
}

/// One named property in a binary VDF object.
#[derive(Debug, Clone, PartialEq)]
pub struct BvdfProperty {
    /// Key name. In format 29 this comes from the shared string table.
    pub name: String,
    /// Tagged value.
    pub value: BvdfValue,
}

impl BvdfProperty {
    /// Creates a property.
    #[must_use]
    pub fn new(name: impl Into<String>, value: BvdfValue) -> Self {
        Self {
            name: name.into(),
            value,
        }
    }

    /// Returns the nested object, when the value is a table.
    #[must_use]
    pub const fn as_table(&self) -> Option<&Bvdf> {
        self.value.as_table()
    }

    /// Returns the nested object mutably, when the value is a table.
    pub fn as_table_mut(&mut self) -> Option<&mut Bvdf> {
        self.value.as_table_mut()
    }

    /// Renders the value for display.
    #[must_use]
    pub fn as_text(&self) -> String {
        self.value.as_text()
    }
}

/// A binary VDF object: an ordered list of properties.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bvdf {
    items: Vec<BvdfProperty>,
}

impl Bvdf {
    /// Creates an empty object.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty object with room for `capacity` properties.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            items: Vec::with_capacity(capacity),
        }
    }

    /// Appends a property. Duplicate names are allowed and preserved.
    pub fn push(&mut self, name: impl Into<String>, value: BvdfValue) {
        self.items.push(BvdfProperty::new(name, value));
    }

    /// Appends a nested object property.
    pub fn push_table(&mut self, name: impl Into<String>, value: Bvdf) {
        self.push(name, BvdfValue::Table(value));
    }

    /// Appends a string property from UTF-8 text.
    pub fn push_text(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.push(name, BvdfValue::String(value.into().into_bytes()));
    }

    /// Returns the properties in file order.
    #[must_use]
    pub fn items(&self) -> &[BvdfProperty] {
        &self.items
    }

    /// Number of properties.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Returns true when the object has no properties.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Returns the first property with this name, compared ASCII case-insensitively.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&BvdfProperty> {
        self.items
            .iter()
            .find(|property| property.name.eq_ignore_ascii_case(name))
    }

    /// Returns the first property with this name mutably, compared ASCII
    /// case-insensitively.
    pub fn find_mut(&mut self, name: &str) -> Option<&mut BvdfProperty> {
        self.items
            .iter_mut()
            .find(|property| property.name.eq_ignore_ascii_case(name))
    }

    /// Returns the first child object with this name.
    #[must_use]
    pub fn get_table(&self, name: &str) -> Option<&Bvdf> {
        self.find(name)?.as_table()
    }

    /// Returns the first child object with this name mutably.
    pub fn get_table_mut(&mut self, name: &str) -> Option<&mut Bvdf> {
        self.find_mut(name)?.as_table_mut()
    }

    /// Returns the first value with this name rendered for display.
    #[must_use]
    pub fn get_text(&self, name: &str) -> Option<String> {
        self.find(name).map(BvdfProperty::as_text)
    }

    /// Removes every property with this name, returning how many were removed.
    pub fn remove(&mut self, name: &str) -> usize {
        let before = self.items.len();
        self.items
            .retain(|property| !property.name.eq_ignore_ascii_case(name));
        before - self.items.len()
    }
}

/// Parses one binary VDF object whose keys are inline NUL-terminated strings.
///
/// A missing terminator and end-of-input are treated as the end of the object, matching the
/// C# reader. Bytes after the root terminator are ignored.
///
/// # Errors
///
/// Returns a parse error for an unsupported type marker, a truncated primitive, or object
/// nesting deeper than 256 levels.
pub fn parse(bytes: &[u8]) -> Result<Bvdf> {
    parse_with_strings(bytes, &[], false)
}

/// Parses one binary VDF object using either inline keys or a shared string table.
///
/// `indexed_keys` must be true for `appinfo.vdf` format 29, where every key is a `u32`
/// index into `strings`.
///
/// # Errors
///
/// In addition to the errors from [`parse`], returns a parse error when an indexed key is
/// outside the supplied string table.
pub fn parse_with_strings(bytes: &[u8], strings: &[String], indexed_keys: bool) -> Result<Bvdf> {
    let mut reader = Reader {
        data: bytes,
        pos: 0,
    };
    read_table(&mut reader, strings, indexed_keys, 0)
}

/// Serializes an object with inline NUL-terminated keys.
///
/// # Errors
///
/// Returns an error when a key or string value contains an interior NUL byte, which the
/// format cannot represent.
pub fn encode(value: &Bvdf) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut interner = Interner::Inline;
    value.write_table(&mut output, &mut interner)?;
    Ok(output)
}

/// Serializes an object with keys replaced by `u32` indices from `intern`.
///
/// The callback may append new names to its string table; the returned index is written
/// into the blob. Existing indices stay valid as long as the table is only appended to.
///
/// # Errors
///
/// Returns an error when a key or string value contains an interior NUL byte.
pub fn encode_indexed<F>(value: &Bvdf, mut intern: F) -> Result<Vec<u8>>
where
    F: FnMut(&str) -> u32,
{
    let mut output = Vec::new();
    let mut interner = Interner::Indexed(&mut intern);
    value.write_table(&mut output, &mut interner)?;
    Ok(output)
}

/// Serializes an object with keys replaced by string-table indices from a fallible
/// interner.
///
/// This is [`encode_indexed`] for callers whose string table has a fixed maximum index
/// or whose new keys must be validated before they are appended.
///
/// # Errors
///
/// Returns an error when a key or string value contains an interior NUL byte, or when the
/// interner rejects a key.
pub fn encode_indexed_checked<F>(value: &Bvdf, mut intern: F) -> Result<Vec<u8>>
where
    F: FnMut(&str) -> Result<u32>,
{
    let mut output = Vec::new();
    let mut interner = Interner::IndexedChecked(&mut intern);
    value.write_table(&mut output, &mut interner)?;
    Ok(output)
}

enum Interner<'a> {
    Inline,
    Indexed(&'a mut dyn FnMut(&str) -> u32),
    IndexedChecked(&'a mut dyn FnMut(&str) -> Result<u32>),
}

impl Bvdf {
    fn write_table(&self, output: &mut Vec<u8>, interner: &mut Interner<'_>) -> Result<()> {
        for property in &self.items {
            let tag = match &property.value {
                BvdfValue::Table(_) => 0x00,
                BvdfValue::String(_) => 0x01,
                BvdfValue::Int32(_) => 0x02,
                BvdfValue::Float(_) => 0x03,
                BvdfValue::Pointer(_) => 0x04,
                BvdfValue::Color(_) => 0x06,
                BvdfValue::UInt64(_) => 0x07,
                BvdfValue::Int64(_) => 0x0A,
            };
            output.push(tag);
            write_key(output, interner, &property.name)?;

            match &property.value {
                BvdfValue::Table(table) => table.write_table(output, interner)?,
                BvdfValue::String(bytes) => write_cstring(output, bytes)?,
                BvdfValue::Int32(value) | BvdfValue::Pointer(value) | BvdfValue::Color(value) => {
                    output.extend_from_slice(&value.to_le_bytes());
                }
                BvdfValue::Float(value) => output.extend_from_slice(&value.to_le_bytes()),
                BvdfValue::UInt64(value) => output.extend_from_slice(&value.to_le_bytes()),
                BvdfValue::Int64(value) => output.extend_from_slice(&value.to_le_bytes()),
            }
        }
        output.push(0x08);
        Ok(())
    }
}

fn write_key(output: &mut Vec<u8>, interner: &mut Interner<'_>, name: &str) -> Result<()> {
    match interner {
        Interner::Inline => write_cstring(output, name.as_bytes())?,
        Interner::Indexed(callback) => {
            output.extend_from_slice(&callback(name).to_le_bytes());
        }
        Interner::IndexedChecked(callback) => {
            output.extend_from_slice(&callback(name)?.to_le_bytes());
        }
    }
    Ok(())
}

fn write_cstring(output: &mut Vec<u8>, value: &[u8]) -> Result<()> {
    if value.contains(&0) {
        return Err(Error::parse(
            "binary vdf",
            "string contains an interior NUL byte",
        ));
    }
    output.extend_from_slice(value);
    output.push(0);
    Ok(())
}

fn read_table(
    reader: &mut Reader<'_>,
    strings: &[String],
    indexed_keys: bool,
    depth: usize,
) -> Result<Bvdf> {
    if depth > MAX_DEPTH {
        return Err(reader.error("object nesting exceeds the supported depth"));
    }

    let mut table = Bvdf::new();
    loop {
        let Some(marker) = reader.read_u8() else {
            return Ok(table);
        };
        if marker == 0x08 || marker == 0x0B {
            return Ok(table);
        }

        let name = if indexed_keys {
            let index = reader.read_u32()?;
            strings
                .get(index as usize)
                .cloned()
                .ok_or_else(|| reader.error(&format!("key string index {index} is out of range")))?
        } else {
            String::from_utf8_lossy(&reader.read_cstring_bytes()).into_owned()
        };

        let value = match marker {
            0x00 => BvdfValue::Table(read_table(reader, strings, indexed_keys, depth + 1)?),
            0x01 => BvdfValue::String(reader.read_cstring_bytes()),
            0x02 => BvdfValue::Int32(reader.read_i32()?),
            0x03 => BvdfValue::Float(reader.read_f32()?),
            0x04 => BvdfValue::Pointer(reader.read_i32()?),
            0x06 => BvdfValue::Color(reader.read_i32()?),
            0x07 => BvdfValue::UInt64(reader.read_u64()?),
            0x0A => BvdfValue::Int64(reader.read_i64()?),
            other => {
                return Err(reader.error(&format!("unsupported value type 0x{other:02X}")));
            }
        };
        table.push(name, value);
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn read_u8(&mut self) -> Option<u8> {
        let byte = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(byte)
    }

    fn read_exact(&mut self, count: usize) -> Result<&[u8]> {
        let end = self
            .pos
            .checked_add(count)
            .ok_or_else(|| self.error("length overflow"))?;
        let slice = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| self.error("unexpected end of data"))?;
        self.pos = end;
        Ok(slice)
    }

    fn read_u32(&mut self) -> Result<u32> {
        let bytes = self.read_exact(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_i32(&mut self) -> Result<i32> {
        let bytes = self.read_exact(4)?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_f32(&mut self) -> Result<f32> {
        let bytes = self.read_exact(4)?;
        Ok(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_u64(&mut self) -> Result<u64> {
        let bytes = self.read_exact(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn read_i64(&mut self) -> Result<i64> {
        let bytes = self.read_exact(8)?;
        Ok(i64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn read_cstring_bytes(&mut self) -> Vec<u8> {
        let rest = self.data.get(self.pos..).unwrap_or_default();
        let end = rest
            .iter()
            .position(|byte| *byte == 0)
            .map_or(self.data.len(), |offset| self.pos + offset);
        let bytes = self.data.get(self.pos..end).unwrap_or_default().to_vec();
        self.pos = if end < self.data.len() { end + 1 } else { end };
        bytes
    }

    fn error(&self, message: &str) -> Error {
        Error::parse("binary vdf", format!("{message} at offset {}", self.pos))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample() -> Bvdf {
        let mut inner = Bvdf::new();
        inner.push("dup", BvdfValue::String(b"first".to_vec()));
        inner.push("dup", BvdfValue::String(b"second".to_vec()));
        inner.push(
            "legacy",
            BvdfValue::String(vec![0x53, 0x74, 0xFD, 0xFE, 0x72]),
        );

        let mut root = Bvdf::new();
        root.push_table("appinfo", inner);
        root.push("count", BvdfValue::Int32(-7));
        root.push("ratio", BvdfValue::Float(1.5));
        root.push("pointer", BvdfValue::Pointer(4_242));
        root.push("color", BvdfValue::Color(0x1122_3344));
        root.push("big", BvdfValue::UInt64(u64::MAX));
        root.push("signed", BvdfValue::Int64(-9_876_543_210));
        root
    }

    #[test]
    fn known_vectors_are_exact() {
        assert_eq!(encode(&Bvdf::new()).unwrap(), vec![0x08]);

        let mut one = Bvdf::new();
        one.push("a", BvdfValue::String(b"b".to_vec()));
        assert_eq!(
            encode(&one).unwrap(),
            vec![0x01, b'a', 0x00, b'b', 0x00, 0x08]
        );
    }

    #[test]
    fn round_trips_every_supported_value_and_preserves_duplicates() {
        let original = sample();
        let bytes = encode(&original).unwrap();
        let parsed = parse(&bytes).unwrap();
        assert_eq!(parsed, original);

        let appinfo = parsed.get_table("appinfo").unwrap();
        let duplicates: Vec<_> = appinfo
            .items()
            .iter()
            .filter(|property| property.name == "dup")
            .map(BvdfProperty::as_text)
            .collect();
        assert_eq!(duplicates, vec!["first", "second"]);
        assert_eq!(
            appinfo.find("legacy").unwrap().value.as_bytes(),
            Some([0x53, 0x74, 0xFD, 0xFE, 0x72].as_slice())
        );
        assert_eq!(parsed.get_text("count").as_deref(), Some("-7"));
    }

    #[test]
    fn indexed_keys_round_trip_through_a_string_table() {
        let original = sample();
        let mut strings: Vec<String> = Vec::new();
        let bytes = encode_indexed(&original, |name| {
            if let Some(index) = strings.iter().position(|candidate| candidate == name) {
                return index as u32;
            }
            strings.push(name.to_owned());
            (strings.len() - 1) as u32
        })
        .unwrap();

        assert_eq!(
            strings,
            vec![
                "appinfo", "dup", "legacy", "count", "ratio", "pointer", "color", "big", "signed"
            ]
        );
        let parsed = parse_with_strings(&bytes, &strings, true).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn rejects_unknown_markers_and_out_of_range_indices() {
        let error = parse(&[0x09, b'x', 0x00, 0x08]).unwrap_err().to_string();
        assert!(error.contains("unsupported value type 0x09"), "{error}");

        let error = parse_with_strings(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x08], &[], true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("out of range"), "{error}");
    }

    #[test]
    fn accepts_missing_terminators_like_the_csharp_reader() {
        let parsed = parse(&[0x01, b'k', 0x00, b'v', 0x00]).unwrap();
        assert_eq!(parsed.get_text("k").as_deref(), Some("v"));
    }

    #[test]
    fn rejects_interior_nul_in_strings() {
        let mut table = Bvdf::new();
        table.push("k", BvdfValue::String(b"a\0b".to_vec()));
        let error = encode(&table).unwrap_err().to_string();
        assert!(error.contains("interior NUL"), "{error}");
    }

    #[test]
    fn fallible_indexed_interner_propagates_its_error() {
        let mut table = Bvdf::new();
        table.push_text("rejected", "value");
        let error = encode_indexed_checked(&table, |_name| {
            Err(Error::InvalidInput("interner said no".to_owned()))
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("interner said no"), "{error}");
    }
}
