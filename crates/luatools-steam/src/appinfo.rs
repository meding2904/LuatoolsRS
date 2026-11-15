//! `appcache/appinfo.vdf` reading.
//!
//! The real file is hundreds of megabytes with roughly 177k app records. This reader keeps
//! only an `appid -> offset` index plus format-29's shared key string table, and parses one
//! app record on demand. Parsed records are memoised so [`AppInfoCache::get`] can return a
//! reference.
//!
//! File layout (v27/v28/v29):
//!
//! ```text
//! magic u32 | universe u32 | [v29] string-table offset i64
//! repeated: appid i32 | size i32 | metadata | binary-VDF blob
//! terminator: appid 0
//! [v29] string table: count u32, then NUL-terminated key bytes
//! ```
//!
//! `size` counts metadata + blob, not the 8-byte record header. Metadata is 60 bytes on
//! v28+ and 40 bytes on v27, which has no binary SHA-1 field.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use luatools_core::{AppId, Error, Result};

use crate::binary_vdf::{self, Bvdf};
use crate::launch_options::{LaunchOption, read_launch_options};

/// appinfo format 27 magic number.
pub const MAGIC_27: u32 = 0x0756_4427;
/// appinfo format 28 magic number.
pub const MAGIC_28: u32 = 0x0756_4428;
/// appinfo format 29 magic number.
pub const MAGIC_29: u32 = 0x0756_4429;

const META_LENGTH_V27: usize = 40;
const META_LENGTH_V28: usize = 60;
const CHANGE_NUMBER_OFFSET: usize = 36;
const SHA1_BINARY_OFFSET: usize = 40;
const MAX_RECORD_BYTES: u64 = 512 * 1024 * 1024;
const MAX_STRING_TABLE_ENTRIES: u32 = 4_000_000;

/// One parsed app record.
#[derive(Debug, Clone, PartialEq)]
pub struct AppInfo {
    /// Steam app id.
    pub app_id: AppId,
    /// appinfo change number from the record metadata.
    pub change_number: u32,
    /// Binary SHA-1 of the blob from v28+ metadata; all zeroes for v27, which has no field.
    pub sha1: [u8; 20],
    /// Root binary-VDF object, normally wrapping an `appinfo` child.
    pub data: Bvdf,
}

impl AppInfo {
    /// Returns the `appinfo` child table, which is where real records keep their content.
    #[must_use]
    pub fn body(&self) -> Option<&Bvdf> {
        self.data.get_table("appinfo")
    }

    /// Builds the flat entry used by the UI and library views.
    #[must_use]
    pub fn entry(&self) -> SteamAppEntry {
        SteamAppEntry::from_body(self.app_id, self.body().unwrap_or(&self.data))
    }
}

/// Steam's coarse application type from the `type` field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum AppType {
    /// A normal game.
    Game,
    /// A non-game application (software, soundtrack tool, ...).
    Application,
    /// A development or SDK tool.
    Tool,
    /// A demo.
    Demo,
    /// Downloadable content.
    Dlc,
    /// A soundtrack or music app.
    Music,
    /// A video app.
    Video,
    /// A series or bundle container.
    Series,
    /// Steam hardware.
    Hardware,
    /// A beta branch app.
    Beta,
    /// A mod or episode.
    Mod,
    /// An unrecognised type.
    #[default]
    Unknown,
}

impl AppType {
    /// Parses Steam's lowercase type tag, case-insensitively.
    #[must_use]
    pub fn from_tag(tag: &str) -> Self {
        match tag.trim().to_ascii_lowercase().as_str() {
            "game" => Self::Game,
            "application" => Self::Application,
            "tool" => Self::Tool,
            "demo" => Self::Demo,
            "dlc" => Self::Dlc,
            "music" => Self::Music,
            "video" => Self::Video,
            "series" => Self::Series,
            "hardware" => Self::Hardware,
            "beta" => Self::Beta,
            "mod" | "episode" => Self::Mod,
            _ => Self::Unknown,
        }
    }

    /// Returns the canonical Steam tag, or an empty string for [`AppType::Unknown`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Game => "game",
            Self::Application => "application",
            Self::Tool => "tool",
            Self::Demo => "demo",
            Self::Dlc => "dlc",
            Self::Music => "music",
            Self::Video => "video",
            Self::Series => "series",
            Self::Hardware => "hardware",
            Self::Beta => "beta",
            Self::Mod => "mod",
            Self::Unknown => "",
        }
    }
}

/// The fields of an app's appinfo body that library and detail views need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamAppEntry {
    /// Steam app id.
    pub app_id: AppId,
    /// Display name.
    pub name: String,
    /// Coarse application type.
    pub app_type: AppType,
    /// Play-button launch entries in file order.
    pub launch: Vec<LaunchOption>,
}

impl SteamAppEntry {
    /// Extracts the UI-facing fields from an appinfo body.
    #[must_use]
    pub fn from_body(app_id: AppId, body: &Bvdf) -> Self {
        let name = body.get_text("name").unwrap_or_default();
        let app_type = AppType::from_tag(body.get_text("type").as_deref().unwrap_or_default());
        Self {
            app_id,
            name,
            app_type,
            launch: read_launch_options(body),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct IndexEntry {
    offset: u64,
    size: usize,
    change_number: u32,
    sha1: [u8; 20],
}

/// Reader/index over one `appinfo.vdf` file.
#[derive(Debug)]
pub struct AppInfoCache {
    path: PathBuf,
    file: BufReader<File>,
    magic: u32,
    universe: u32,
    meta_length: usize,
    indexed_keys: bool,
    strings: Vec<String>,
    index: BTreeMap<AppId, IndexEntry>,
    parsed: BTreeMap<AppId, AppInfo>,
}

impl AppInfoCache {
    /// Opens an appinfo file and builds its record index.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the file cannot be opened or read, and a parse error when
    /// the magic is not 0x07564427/28/29, the string table is invalid, or a record header
    /// is truncated or inconsistent.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|error| {
            Error::io(format!("opening appinfo cache {}", path.display()), error)
        })?;
        let file_length = file
            .metadata()
            .map_err(|error| {
                Error::io(
                    format!("reading appinfo cache metadata {}", path.display()),
                    error,
                )
            })?
            .len();
        let mut reader = BufReader::new(file);

        let magic = read_u32(&mut reader, "appinfo magic")?;
        let universe = read_u32(&mut reader, "appinfo universe")?;
        if magic != MAGIC_27 && magic != MAGIC_28 && magic != MAGIC_29 {
            return Err(Error::parse(
                "appinfo",
                format!("unknown appinfo version 0x{magic:08X}"),
            ));
        }

        let indexed_keys = magic == MAGIC_29;
        let meta_length = if magic >= MAGIC_28 {
            META_LENGTH_V28
        } else {
            META_LENGTH_V27
        };

        let mut strings = Vec::new();
        if indexed_keys {
            let table_offset = read_i64(&mut reader, "appinfo string-table offset")?;
            let table_offset = u64::try_from(table_offset)
                .map_err(|_| Error::parse("appinfo", "negative appinfo string-table offset"))?;
            let resume = reader
                .stream_position()
                .map_err(|error| Error::io("reading appinfo string-table position", error))?;
            read_string_table(&mut reader, table_offset, file_length, &mut strings)?;
            reader
                .seek(SeekFrom::Start(resume))
                .map_err(|error| Error::io("seeking back to appinfo records", error))?;
        }

        let apps_start = reader
            .stream_position()
            .map_err(|error| Error::io("reading appinfo record start", error))?;
        let index = build_index(&mut reader, file_length, meta_length, apps_start)?;

        Ok(Self {
            path: path.to_path_buf(),
            file: reader,
            magic,
            universe,
            meta_length,
            indexed_keys,
            strings,
            index,
            parsed: BTreeMap::new(),
        })
    }

    /// Path this cache was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file's format magic number.
    #[must_use]
    pub const fn magic_number(&self) -> u32 {
        self.magic
    }

    /// The file's universe field.
    #[must_use]
    pub const fn universe(&self) -> u32 {
        self.universe
    }

    /// Metadata length for this format: 60 bytes on v28+, 40 on v27.
    #[must_use]
    pub const fn meta_length(&self) -> usize {
        self.meta_length
    }

    /// True when replayed keys are string-table indices (format 29).
    #[must_use]
    pub const fn indexed_keys(&self) -> bool {
        self.indexed_keys
    }

    /// Number of apps in the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// True when the file contains no app records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Iterates the indexed app ids in ascending order.
    pub fn iter_app_ids(&self) -> impl Iterator<Item = AppId> + '_ {
        self.index.keys().copied()
    }

    /// Parses one app record, memoising the result.
    ///
    /// Returns `None` when the app is not in the file.
    ///
    /// # Errors
    ///
    /// Returns an error when the record's blob is unreadable or malformed, or when an
    /// indexed key refers outside the shared string table.
    pub fn get(&mut self, app: AppId) -> Result<Option<&AppInfo>> {
        if !self.parsed.contains_key(&app) {
            let Some(entry) = self.index.get(&app).copied() else {
                return Ok(None);
            };
            let parsed = self.read_app(app, entry)?;
            self.parsed.insert(app, parsed);
        }
        Ok(self.parsed.get(&app))
    }

    fn read_app(&mut self, app: AppId, entry: IndexEntry) -> Result<AppInfo> {
        let blob_length = entry.size - self.meta_length;
        let blob_offset = entry
            .offset
            .checked_add(8)
            .and_then(|offset| offset.checked_add(self.meta_length as u64))
            .ok_or_else(|| Error::parse("appinfo", "record blob offset overflow"))?;
        self.file
            .seek(SeekFrom::Start(blob_offset))
            .map_err(|error| Error::io(format!("seeking to app {app} in appinfo"), error))?;

        let mut blob = vec![0u8; blob_length];
        self.file.read_exact(&mut blob).map_err(|error| {
            Error::parse("appinfo", format!("truncated blob for app {app}: {error}"))
        })?;

        let data = binary_vdf::parse_with_strings(&blob, &self.strings, self.indexed_keys)
            .map_err(|error| error.context(format!("parsing app {app} in appinfo")))?;

        Ok(AppInfo {
            app_id: app,
            change_number: entry.change_number,
            sha1: entry.sha1,
            data,
        })
    }
}

fn read_string_table(
    reader: &mut BufReader<File>,
    offset: u64,
    file_length: u64,
    strings: &mut Vec<String>,
) -> Result<()> {
    if offset < 8 || offset >= file_length {
        return Err(Error::parse(
            "appinfo",
            format!("string-table offset {offset} is outside the file"),
        ));
    }
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|error| Error::io("seeking to appinfo string table", error))?;

    let count = read_u32(reader, "appinfo string-table count")?;
    if count > MAX_STRING_TABLE_ENTRIES {
        return Err(Error::parse(
            "appinfo",
            format!("string-table count {count} exceeds the supported cap"),
        ));
    }
    let remaining = file_length.saturating_sub(offset.saturating_add(4));
    if u64::from(count) > remaining {
        return Err(Error::parse(
            "appinfo",
            format!("string-table count {count} exceeds the remaining {remaining} bytes"),
        ));
    }
    strings.reserve(count as usize);

    for index in 0..count {
        let position = reader
            .stream_position()
            .map_err(|error| Error::io("reading appinfo string-table position", error))?;
        if position >= file_length {
            return Err(Error::parse(
                "appinfo",
                format!("string-table entry {index} is truncated"),
            ));
        }
        let raw = read_cstring(reader, "appinfo string-table entry")?;
        strings.push(String::from_utf8_lossy(&raw).into_owned());
    }
    Ok(())
}

fn build_index(
    reader: &mut BufReader<File>,
    file_length: u64,
    meta_length: usize,
    apps_start: u64,
) -> Result<BTreeMap<AppId, IndexEntry>> {
    reader
        .seek(SeekFrom::Start(apps_start))
        .map_err(|error| Error::io("seeking to appinfo record index", error))?;

    let mut index = BTreeMap::new();
    loop {
        let offset = reader
            .stream_position()
            .map_err(|error| Error::io("reading appinfo record offset", error))?;
        if offset >= file_length || file_length - offset < 8 {
            break;
        }

        let app_id = read_i32(reader, "appinfo record app id")?;
        let size = read_i32(reader, "appinfo record size")?;
        if app_id == 0 {
            break;
        }
        if app_id < 0 {
            return Err(Error::parse(
                "appinfo",
                format!("negative app id {app_id} at offset {offset}"),
            ));
        }
        let size = usize::try_from(size).map_err(|_| {
            Error::parse(
                "appinfo",
                format!("negative record size at offset {offset}"),
            )
        })?;
        if size < meta_length {
            return Err(Error::parse(
                "appinfo",
                format!(
                    "record for app {app_id} has size {size}, smaller than the {meta_length}-byte metadata"
                ),
            ));
        }
        if u64::try_from(size).unwrap_or(u64::MAX) > MAX_RECORD_BYTES {
            return Err(Error::parse(
                "appinfo",
                format!("record for app {app_id} is unreasonably large ({size} bytes)"),
            ));
        }

        let end = offset
            .checked_add(8)
            .and_then(|value| value.checked_add(size as u64))
            .ok_or_else(|| Error::parse("appinfo", "record end offset overflow"))?;
        if end > file_length {
            return Err(Error::parse(
                "appinfo",
                format!("record for app {app_id} runs past the end of the file"),
            ));
        }

        let metadata = read_exact_vec(reader, meta_length, "appinfo record metadata")?;
        let change_number = u32::from_le_bytes([
            metadata[CHANGE_NUMBER_OFFSET],
            metadata[CHANGE_NUMBER_OFFSET + 1],
            metadata[CHANGE_NUMBER_OFFSET + 2],
            metadata[CHANGE_NUMBER_OFFSET + 3],
        ]);
        let mut sha1 = [0u8; 20];
        if meta_length >= META_LENGTH_V28 {
            sha1.copy_from_slice(&metadata[SHA1_BINARY_OFFSET..SHA1_BINARY_OFFSET + 20]);
        }

        let app_id = u32::try_from(app_id)
            .map_err(|_| Error::parse("appinfo", "app id does not fit in u32"))?;
        index.insert(
            AppId::new(app_id),
            IndexEntry {
                offset,
                size,
                change_number,
                sha1,
            },
        );
        reader
            .seek(SeekFrom::Start(end))
            .map_err(|error| Error::io("seeking over an appinfo record", error))?;
    }
    Ok(index)
}

fn read_u32<R: Read>(reader: &mut R, what: &str) -> Result<u32> {
    let mut bytes = [0u8; 4];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| Error::parse("appinfo", format!("truncated {what}: {error}")))?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_i32<R: Read>(reader: &mut R, what: &str) -> Result<i32> {
    let mut bytes = [0u8; 4];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| Error::parse("appinfo", format!("truncated {what}: {error}")))?;
    Ok(i32::from_le_bytes(bytes))
}

fn read_i64<R: Read>(reader: &mut R, what: &str) -> Result<i64> {
    let mut bytes = [0u8; 8];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| Error::parse("appinfo", format!("truncated {what}: {error}")))?;
    Ok(i64::from_le_bytes(bytes))
}

fn read_exact_vec<R: Read>(reader: &mut R, count: usize, what: &str) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; count];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| Error::parse("appinfo", format!("truncated {what}: {error}")))?;
    Ok(bytes)
}

fn read_cstring<R: Read>(reader: &mut R, what: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        reader
            .read_exact(&mut byte)
            .map_err(|error| Error::parse("appinfo", format!("truncated {what}: {error}")))?;
        if byte[0] == 0 {
            return Ok(bytes);
        }
        bytes.push(byte[0]);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::binary_vdf::{BvdfValue, encode_indexed};

    fn metadata(change_number: u32, sha1: [u8; 20]) -> [u8; 60] {
        let mut bytes = [0u8; 60];
        bytes[16..36].copy_from_slice(b"TEXT-HASH-20-BYTES!!");
        bytes[36..40].copy_from_slice(&change_number.to_le_bytes());
        bytes[40..60].copy_from_slice(&sha1);
        bytes
    }

    fn app_body(name: &str, app_type: &str, executable: &str) -> Bvdf {
        let mut launch_entry = Bvdf::new();
        launch_entry.push_text("executable", executable);
        launch_entry.push_text("type", "default");
        let mut launch = Bvdf::new();
        launch.push_table("0", launch_entry);
        let mut config = Bvdf::new();
        config.push_table("launch", launch);
        config.push_text("installdir", "cs2");

        let mut body = Bvdf::new();
        body.push_text("name", name);
        body.push_text("type", app_type);
        body.push_table("config", config);
        body.push("dup", BvdfValue::String(b"first".to_vec()));
        body.push("dup", BvdfValue::String(b"second".to_vec()));

        let mut root = Bvdf::new();
        root.push_table("appinfo", body);
        root
    }

    fn build_v29(apps: &[(u32, u32, [u8; 20], Bvdf)], universe: u32) -> Vec<u8> {
        let mut strings: Vec<String> = Vec::new();
        let mut encoded = Vec::with_capacity(apps.len());
        for (app_id, change_number, sha1, root) in apps {
            let blob = encode_indexed(root, |name| {
                if let Some(index) = strings.iter().position(|candidate| candidate == name) {
                    return index as u32;
                }
                strings.push(name.to_owned());
                (strings.len() - 1) as u32
            })
            .unwrap();
            encoded.push((*app_id, *change_number, *sha1, blob));
        }

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC_29.to_le_bytes());
        bytes.extend_from_slice(&universe.to_le_bytes());
        let offset_position = bytes.len();
        bytes.extend_from_slice(&[0u8; 8]);

        for (app_id, change_number, sha1, blob) in &encoded {
            bytes.extend_from_slice(&(*app_id as i32).to_le_bytes());
            let size = 60 + blob.len();
            bytes.extend_from_slice(&(size as i32).to_le_bytes());
            bytes.extend_from_slice(&metadata(*change_number, *sha1));
            bytes.extend_from_slice(blob);
        }
        bytes.extend_from_slice(&0i32.to_le_bytes());

        let table_offset = bytes.len() as i64;
        bytes[offset_position..offset_position + 8].copy_from_slice(&table_offset.to_le_bytes());
        bytes.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        for string in &strings {
            bytes.extend_from_slice(string.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    fn build_inline(magic: u32, apps: &[(u32, u32, [u8; 20], Bvdf)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&magic.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        for (app_id, change_number, sha1, root) in apps {
            let blob = crate::binary_vdf::encode(root).unwrap();
            let meta_length = if magic >= MAGIC_28 { 60 } else { 40 };
            bytes.extend_from_slice(&(*app_id as i32).to_le_bytes());
            let size = meta_length + blob.len();
            bytes.extend_from_slice(&(size as i32).to_le_bytes());
            bytes.extend_from_slice(&metadata(*change_number, *sha1)[..meta_length]);
            bytes.extend_from_slice(&blob);
        }
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes
    }

    fn write_temp(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("appinfo.vdf");
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn reads_v29_records_with_indexed_keys_and_memoises_them() {
        let apps = vec![
            (
                730,
                7777,
                [0xAB; 20],
                app_body("Counter-Strike 2", "game", "Game.exe"),
            ),
            (
                220,
                8888,
                [0xCD; 20],
                app_body("Half-Life 2", "game", "hl2.exe"),
            ),
        ];
        let bytes = build_v29(&apps, 1);
        let (_dir, path) = write_temp(&bytes);

        let mut cache = AppInfoCache::open(&path).unwrap();
        assert_eq!(cache.magic_number(), MAGIC_29);
        assert_eq!(cache.universe(), 1);
        assert_eq!(cache.meta_length(), 60);
        assert!(cache.indexed_keys());
        assert_eq!(cache.len(), 2);
        assert_eq!(
            cache.iter_app_ids().collect::<Vec<_>>(),
            vec![AppId::new(220), AppId::new(730)]
        );

        let app = cache.get(AppId::new(730)).unwrap().unwrap();
        assert_eq!(app.change_number, 7777);
        assert_eq!(app.sha1, [0xAB; 20]);
        assert_eq!(
            app.body().unwrap().get_text("name").as_deref(),
            Some("Counter-Strike 2")
        );

        let entry = app.entry();
        assert_eq!(entry.name, "Counter-Strike 2");
        assert_eq!(entry.app_type, AppType::Game);
        assert_eq!(entry.launch.len(), 1);
        assert_eq!(entry.launch[0].executable, "Game.exe");

        let duplicates: Vec<_> = app
            .body()
            .unwrap()
            .items()
            .iter()
            .filter(|property| property.name == "dup")
            .map(|property| property.as_text())
            .collect();
        assert_eq!(duplicates, vec!["first", "second"]);

        // A second read is served from the memoised map and stays correct.
        assert_eq!(
            cache.get(AppId::new(730)).unwrap().unwrap().change_number,
            7777
        );
        assert!(cache.get(AppId::new(999)).unwrap().is_none());
    }

    #[test]
    fn reads_v27_and_v28_inline_key_formats() {
        for (magic, meta_length) in [(MAGIC_27, 40), (MAGIC_28, 60)] {
            let apps = vec![(
                440,
                123,
                [0x11; 20],
                app_body("Team Fortress 2", "game", "hl2.exe"),
            )];
            let bytes = build_inline(magic, &apps);
            let (_dir, path) = write_temp(&bytes);

            let mut cache = AppInfoCache::open(&path).unwrap();
            assert_eq!(cache.magic_number(), magic);
            assert_eq!(cache.meta_length(), meta_length);
            assert!(!cache.indexed_keys());
            let app = cache.get(AppId::new(440)).unwrap().unwrap();
            assert_eq!(app.change_number, 123);
            assert_eq!(app.entry().name, "Team Fortress 2");
            if magic == MAGIC_27 {
                assert_eq!(app.sha1, [0u8; 20]);
            } else {
                assert_eq!(app.sha1, [0x11; 20]);
            }
        }
    }

    #[test]
    fn rejects_unknown_magic_before_parsing_records() {
        let mut bytes = 0x0756_4430u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        let (_dir, path) = write_temp(&bytes);
        let error = AppInfoCache::open(&path).unwrap_err().to_string();
        assert!(error.contains("0x07564430"), "{error}");
    }

    #[test]
    fn rejects_records_smaller_than_metadata_or_past_eof() {
        let mut bytes = MAGIC_28.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&730i32.to_le_bytes());
        bytes.extend_from_slice(&10i32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 10]);
        let (_dir, path) = write_temp(&bytes);
        let error = AppInfoCache::open(&path).unwrap_err().to_string();
        assert!(
            error.contains("smaller than the 60-byte metadata"),
            "{error}"
        );

        let mut bytes = MAGIC_28.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&730i32.to_le_bytes());
        bytes.extend_from_slice(&1000i32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 60]);
        let (_dir, path) = write_temp(&bytes);
        let error = AppInfoCache::open(&path).unwrap_err().to_string();
        assert!(error.contains("past the end"), "{error}");
    }

    #[test]
    fn rejects_string_table_offsets_and_counts_outside_the_file() {
        let mut bytes = MAGIC_29.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&999i64.to_le_bytes());
        let (_dir, path) = write_temp(&bytes);
        let error = AppInfoCache::open(&path).unwrap_err().to_string();
        assert!(error.contains("string-table offset"), "{error}");

        let mut bytes = MAGIC_29.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&16i64.to_le_bytes());
        bytes.extend_from_slice(&999u32.to_le_bytes());
        let (_dir, path) = write_temp(&bytes);
        let error = AppInfoCache::open(&path).unwrap_err().to_string();
        assert!(error.contains("string-table count"), "{error}");
    }

    #[test]
    fn app_type_tags_are_case_insensitive() {
        assert_eq!(AppType::from_tag("GAME"), AppType::Game);
        assert_eq!(AppType::from_tag(" dlc "), AppType::Dlc);
        assert_eq!(AppType::from_tag("episode"), AppType::Mod);
        assert_eq!(AppType::from_tag("something-new"), AppType::Unknown);
        assert_eq!(AppType::Game.as_str(), "game");
        assert_eq!(AppType::Unknown.as_str(), "");
    }
}
