//! `appcache/appinfo.vdf` reading and saving.
//!
//! The real file is hundreds of megabytes with roughly 177k app records. This reader keeps
//! only an `appid -> offset` index plus format-29's shared key string table, and parses one
//! app record on demand. Parsed records are memoised so [`AppInfoCache::get`] can return a
//! reference. [`AppInfoCache::save_as`] streams a new file, copying untouched app records
//! byte-for-byte and reserializing only the requested replacements.
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

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use luatools_core::{AppId, Error, Result};
use sha1::{Digest, Sha1};

use crate::binary_vdf::{self, Bvdf};
use crate::launch_options::read_launch_options;
use luatools_core::LaunchOption;

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
        // Steam keeps the descriptive fields in `common` (current appinfo) or `appinfo` (older
        // captures); the launch entries live in `config`. Fall back to the record root for
        // hand-built blobs.
        let app = body
            .get_table("common")
            .or_else(|| body.get_table("appinfo"))
            .unwrap_or(body);
        let name = app.get_text("name").unwrap_or_default();
        let app_type = AppType::from_tag(app.get_text("type").as_deref().unwrap_or_default());
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
    raw_strings: Vec<Vec<u8>>,
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
        let mut raw_strings = Vec::new();
        if indexed_keys {
            let table_offset = read_i64(&mut reader, "appinfo string-table offset")?;
            let table_offset = u64::try_from(table_offset)
                .map_err(|_| Error::parse("appinfo", "negative appinfo string-table offset"))?;
            let resume = reader
                .stream_position()
                .map_err(|error| Error::io("reading appinfo string-table position", error))?;
            read_string_table(
                &mut reader,
                table_offset,
                file_length,
                &mut strings,
                &mut raw_strings,
            )?;
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
            raw_strings,
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

    /// Decoded format-29 string-table entries, in index order.
    ///
    /// Entries that were not valid UTF-8 are decoded lossily for display. Saving preserves
    /// the original raw bytes; use this only for inspection.
    #[must_use]
    pub fn strings(&self) -> &[String] {
        &self.strings
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

    /// Writes a new appinfo file, replacing the blobs of selected apps.
    ///
    /// `edits` maps an app id to a complete replacement root table, including the
    /// `appinfo` wrapper when the source records use one. Apps that are not in `edits` are
    /// copied from the source byte-for-byte, which keeps duplicate keys, pointer/color
    /// tags, and non-UTF-8 values exact. Edited apps are reserialized and, on format 28
    /// and 29, get a fresh `sha1_binary` (SHA-1 of the new blob). `sha1_text` and
    /// `changeNumber` are copied from the source metadata without modification. Format 27
    /// has no binary hash field, so its metadata is written unchanged.
    ///
    /// On format 29 the original raw string-table entries are preserved verbatim, existing
    /// keys keep their indices, and keys that only occur in newly written blobs are
    /// appended in first-use order. Because parsed keys are decoded lossily, an *edited*
    /// blob whose key bytes were not valid UTF-8 interns the decoded replacement text;
    /// untouched apps still keep their exact bytes.
    ///
    /// Edits for app ids that are not in this file's index are ignored, mirroring the
    /// source implementation which iterates its index rather than the edit map.
    ///
    /// The destination is truncated and written directly. This method does not write a
    /// temporary file, flush to stable storage, refresh the in-memory index, or replace the
    /// source; callers must write to a temporary path and rename it into place after
    /// closing the cache, and should stop Steam before replacing its file.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// - `destination` resolves to the source file, which would truncate the source while
    ///   it is still being read;
    /// - the source has shrunk since it was opened, or an indexed record has a size
    ///   smaller than its metadata or running past the end of the source;
    /// - a key or string value in an edited blob contains an interior NUL byte;
    /// - the string table grows past its `u32` index space;
    /// - opening, reading, writing, seeking, or serializing fails.
    pub fn save_as(&mut self, destination: &Path, edits: &HashMap<AppId, Bvdf>) -> Result<()> {
        self.ensure_distinct_destination(destination)?;
        self.validate_source_records()?;

        let mut raw_strings = self.raw_strings.clone();
        let mut lookup: HashMap<String, u32> = HashMap::new();
        for (index, text) in self.strings.iter().enumerate() {
            let Ok(index) = u32::try_from(index) else {
                break;
            };
            lookup.entry(text.clone()).or_insert(index);
        }

        let output = File::create(destination).map_err(|error| {
            Error::io(
                format!("creating appinfo destination {}", destination.display()),
                error,
            )
        })?;
        let mut writer = BufWriter::new(output);

        write_bytes(&mut writer, &self.magic.to_le_bytes(), "appinfo magic")?;
        write_bytes(
            &mut writer,
            &self.universe.to_le_bytes(),
            "appinfo universe",
        )?;
        let table_offset_position = if self.indexed_keys {
            let position = stream_position(&mut writer, "appinfo string-table offset")?;
            write_bytes(
                &mut writer,
                &[0u8; 8],
                "appinfo string-table offset placeholder",
            )?;
            Some(position)
        } else {
            None
        };

        let mut entries: Vec<(AppId, IndexEntry)> = self
            .index
            .iter()
            .map(|(app_id, entry)| (*app_id, *entry))
            .collect();
        entries.sort_unstable_by_key(|(_, entry)| entry.offset);

        for (app_id, entry) in entries {
            let mut metadata = self.read_record_metadata(app_id, entry)?;
            if let Some(replacement) = edits.get(&app_id) {
                let blob = if self.indexed_keys {
                    binary_vdf::encode_indexed_checked(replacement, |name| {
                        if let Some(index) = lookup.get(name) {
                            return Ok(*index);
                        }
                        let index = u32::try_from(raw_strings.len()).map_err(|_| {
                            Error::parse("appinfo", "appinfo string-table index overflow")
                        })?;
                        if name.as_bytes().contains(&0) {
                            return Err(Error::parse(
                                "appinfo",
                                "appinfo string-table key contains an interior NUL byte",
                            ));
                        }
                        raw_strings.push(name.as_bytes().to_vec());
                        lookup.insert(name.to_owned(), index);
                        Ok(index)
                    })?
                } else {
                    binary_vdf::encode(replacement)?
                };

                if self.magic >= MAGIC_28 {
                    let digest = Sha1::digest(&blob);
                    metadata
                        .get_mut(SHA1_BINARY_OFFSET..SHA1_BINARY_OFFSET + 20)
                        .ok_or_else(|| {
                            Error::parse(
                                "appinfo",
                                "record metadata is too short for the binary SHA-1",
                            )
                        })?
                        .copy_from_slice(&digest);
                }
                let size = metadata.len().checked_add(blob.len()).ok_or_else(|| {
                    Error::parse("appinfo", format!("record for app {app_id} is too large"))
                })?;
                let size = i32::try_from(size).map_err(|_| {
                    Error::parse(
                        "appinfo",
                        format!("record for app {app_id} does not fit in a 32-bit size"),
                    )
                })?;

                write_app_id(&mut writer, app_id, "appinfo record app id")?;
                write_bytes(&mut writer, &size.to_le_bytes(), "appinfo record size")?;
                write_bytes(
                    &mut writer,
                    &metadata,
                    &format!("appinfo metadata for app {app_id}"),
                )?;
                write_bytes(
                    &mut writer,
                    &blob,
                    &format!("appinfo blob for app {app_id}"),
                )?;
            } else {
                let size = i32::try_from(entry.size).map_err(|_| {
                    Error::parse(
                        "appinfo",
                        format!("record for app {app_id} does not fit in a 32-bit size"),
                    )
                })?;
                write_app_id(&mut writer, app_id, "appinfo record app id")?;
                write_bytes(&mut writer, &size.to_le_bytes(), "appinfo record size")?;
                write_bytes(
                    &mut writer,
                    &metadata,
                    &format!("appinfo metadata for app {app_id}"),
                )?;
                self.copy_record_blob(app_id, entry, &mut writer)?;
            }
        }

        write_bytes(
            &mut writer,
            &0i32.to_le_bytes(),
            "appinfo record terminator",
        )?;

        if let Some(position) = table_offset_position {
            let table_offset = stream_position(&mut writer, "appinfo string-table position")?;
            let count = u32::try_from(raw_strings.len()).map_err(|_| {
                Error::parse("appinfo", "appinfo string-table count does not fit in u32")
            })?;
            write_bytes(
                &mut writer,
                &count.to_le_bytes(),
                "appinfo string-table count",
            )?;
            for raw in &raw_strings {
                write_bytes(&mut writer, raw, "appinfo string-table entry")?;
                write_bytes(&mut writer, &[0u8], "appinfo string-table terminator")?;
            }

            let table_offset = i64::try_from(table_offset).map_err(|_| {
                Error::parse("appinfo", "appinfo string-table offset does not fit in i64")
            })?;
            seek_to(&mut writer, position, "appinfo string-table offset field")?;
            write_bytes(
                &mut writer,
                &table_offset.to_le_bytes(),
                "appinfo string-table offset",
            )?;
        }

        writer
            .flush()
            .map_err(|error| Error::io("flushing appinfo destination", error))
    }

    fn ensure_distinct_destination(&self, destination: &Path) -> Result<()> {
        let canonical_source = self.path.canonicalize().ok();
        let canonical_destination = destination.canonicalize().ok();
        let same_canonical = canonical_source
            .as_deref()
            .zip(canonical_destination.as_deref())
            .is_some_and(|(source, destination)| source == destination);
        if self.path == destination
            || same_canonical
            || paths_share_an_inode(&self.path, destination)
        {
            return Err(Error::InvalidInput(format!(
                "refusing to save appinfo over its source file {}",
                self.path.display()
            )));
        }
        Ok(())
    }

    fn validate_source_records(&self) -> Result<()> {
        let file_length = self
            .file
            .get_ref()
            .metadata()
            .map_err(|error| Error::io("reading appinfo source length", error))?
            .len();
        for (app_id, entry) in &self.index {
            if entry.size < self.meta_length {
                return Err(Error::parse(
                    "appinfo",
                    format!(
                        "record for app {app_id} has size {}, smaller than the {}-byte metadata",
                        entry.size, self.meta_length
                    ),
                ));
            }
            let end = entry
                .offset
                .checked_add(8)
                .and_then(|value| value.checked_add(entry.size as u64))
                .ok_or_else(|| Error::parse("appinfo", "record end offset overflow"))?;
            if end > file_length {
                return Err(Error::parse(
                    "appinfo",
                    format!(
                        "record for app {app_id} ends at {end}, past the current source length {file_length}"
                    ),
                ));
            }
        }
        Ok(())
    }

    fn read_record_metadata(&mut self, app_id: AppId, entry: IndexEntry) -> Result<Vec<u8>> {
        let meta_offset = entry
            .offset
            .checked_add(8)
            .ok_or_else(|| Error::parse("appinfo", "record metadata offset overflow"))?;
        self.file
            .seek(SeekFrom::Start(meta_offset))
            .map_err(|error| Error::io(format!("seeking to app {app_id} metadata"), error))?;
        read_exact_vec(
            &mut self.file,
            self.meta_length,
            &format!("appinfo metadata for app {app_id}"),
        )
    }

    fn copy_record_blob<W: Write>(
        &mut self,
        app_id: AppId,
        entry: IndexEntry,
        writer: &mut W,
    ) -> Result<()> {
        let blob_length = entry.size.checked_sub(self.meta_length).ok_or_else(|| {
            Error::parse(
                "appinfo",
                format!("record for app {app_id} has no blob after its metadata"),
            )
        })?;
        let blob_offset = entry
            .offset
            .checked_add(8)
            .and_then(|value| value.checked_add(self.meta_length as u64))
            .ok_or_else(|| Error::parse("appinfo", "record blob offset overflow"))?;
        self.file
            .seek(SeekFrom::Start(blob_offset))
            .map_err(|error| Error::io(format!("seeking to app {app_id} blob"), error))?;

        let expected = u64::try_from(blob_length)
            .map_err(|_| Error::parse("appinfo", "record blob length does not fit in u64"))?;
        let copied = std::io::copy(&mut (&mut self.file).take(expected), writer)
            .map_err(|error| Error::io(format!("copying app {app_id} blob"), error))?;
        if copied != expected {
            return Err(Error::parse(
                "appinfo",
                format!("appinfo source ended mid-app while copying app {app_id}"),
            ));
        }
        Ok(())
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
    raw_strings: &mut Vec<Vec<u8>>,
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
        raw_strings.push(raw);
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

fn write_bytes<W: Write>(writer: &mut W, bytes: &[u8], what: &str) -> Result<()> {
    writer
        .write_all(bytes)
        .map_err(|error| Error::io(format!("writing {what}"), error))
}

fn write_app_id<W: Write>(writer: &mut W, value: AppId, what: &str) -> Result<()> {
    let raw = i32::try_from(value.get())
        .map_err(|_| Error::parse("appinfo", format!("{what} {value} does not fit in i32")))?;
    write_bytes(writer, &raw.to_le_bytes(), what)
}

fn stream_position<W: Seek>(writer: &mut W, what: &str) -> Result<u64> {
    writer
        .stream_position()
        .map_err(|error| Error::io(format!("reading {what}"), error))
}

fn seek_to<W: Seek>(writer: &mut W, position: u64, what: &str) -> Result<()> {
    writer
        .seek(SeekFrom::Start(position))
        .map_err(|error| Error::io(format!("seeking to {what}"), error))?;
    Ok(())
}

#[cfg(unix)]
fn paths_share_an_inode(source: &Path, destination: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    match (std::fs::metadata(source), std::fs::metadata(destination)) {
        (Ok(source), Ok(destination)) => {
            source.dev() == destination.dev() && source.ino() == destination.ino()
        }
        _ => false,
    }
}

#[cfg(not(unix))]
fn paths_share_an_inode(_source: &Path, _destination: &Path) -> bool {
    false
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

    #[test]
    fn reads_name_from_the_common_table() {
        let mut common = Bvdf::new();
        common.push_text("name", "Counter-Strike 2");
        common.push_text("type", "Game");
        let mut body = Bvdf::new();
        body.push_table("common", common);
        let entry = SteamAppEntry::from_body(AppId::new(730), &body);
        assert_eq!(entry.name, "Counter-Strike 2");
        assert_eq!(entry.app_type, AppType::Game);
    }

    fn append_indexed_string(output: &mut Vec<u8>, index: u32, value: &[u8]) {
        output.push(0x01);
        output.extend_from_slice(&index.to_le_bytes());
        output.extend_from_slice(value);
        output.push(0);
    }

    /// A v29 file whose string table and one property value contain non-UTF-8 bytes,
    /// whose object has duplicate keys, and which uses pointer and color tags.
    fn v29_raw_fixture() -> Vec<u8> {
        let raw_strings: [&[u8]; 6] = [
            b"appinfo", b"dup", b"legacy", b"\xFDkey", b"pointer", b"color",
        ];

        let mut blob = Vec::new();
        blob.push(0x00); // table
        blob.extend_from_slice(&0u32.to_le_bytes()); // "appinfo"
        append_indexed_string(&mut blob, 1, b"first");
        append_indexed_string(&mut blob, 1, b"second");
        append_indexed_string(&mut blob, 2, &[0x53, 0x74, 0xFD, 0xFE, 0x72]);
        append_indexed_string(&mut blob, 3, b"value");
        blob.push(0x04); // pointer
        blob.extend_from_slice(&4u32.to_le_bytes());
        blob.extend_from_slice(&77i32.to_le_bytes());
        blob.push(0x06); // color
        blob.extend_from_slice(&5u32.to_le_bytes());
        blob.extend_from_slice(&0x1122_3344i32.to_le_bytes());
        blob.push(0x08);
        blob.push(0x08); // root terminator

        let digest = Sha1::digest(&blob);
        let mut sha1 = [0u8; 20];
        sha1.copy_from_slice(&digest);

        let mut bytes = MAGIC_29.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        let offset_position = bytes.len();
        bytes.extend_from_slice(&[0u8; 8]);
        bytes.extend_from_slice(&730i32.to_le_bytes());
        let size = 60 + blob.len();
        bytes.extend_from_slice(&(size as i32).to_le_bytes());
        bytes.extend_from_slice(&metadata(4242, sha1));
        bytes.extend_from_slice(&blob);
        bytes.extend_from_slice(&0i32.to_le_bytes());

        let table_offset = bytes.len() as i64;
        bytes[offset_position..offset_position + 8].copy_from_slice(&table_offset.to_le_bytes());
        bytes.extend_from_slice(&(raw_strings.len() as u32).to_le_bytes());
        for string in raw_strings {
            bytes.extend_from_slice(string);
            bytes.push(0);
        }
        bytes
    }

    fn edit_arguments(cache: &mut AppInfoCache, app: AppId) -> Bvdf {
        let mut replacement = cache.get(app).unwrap().unwrap().data.clone();
        replacement
            .get_table_mut("appinfo")
            .unwrap()
            .push_text("arguments", "-novid");
        replacement
    }

    fn v29_record(bytes: &[u8], app_id: i32) -> &[u8] {
        let mut position = 16usize;
        loop {
            let id = i32::from_le_bytes(bytes[position..position + 4].try_into().unwrap());
            if id == 0 {
                panic!("app {app_id} is missing from the fixture");
            }
            let size = i32::from_le_bytes(bytes[position + 4..position + 8].try_into().unwrap());
            let size = usize::try_from(size).unwrap();
            let end = position + 8 + size;
            if id == app_id {
                return &bytes[position..end];
            }
            position = end;
        }
    }

    #[test]
    fn save_as_without_edits_rebuilds_v29_byte_identically() {
        let bytes = v29_raw_fixture();
        let (dir, path) = write_temp(&bytes);
        let mut cache = AppInfoCache::open(&path).unwrap();
        assert_eq!(cache.strings().len(), 6);
        assert_eq!(cache.strings()[3], String::from_utf8_lossy(b"\xFDkey"));

        let destination = dir.path().join("rebuilt.vdf");
        cache.save_as(&destination, &HashMap::new()).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
    }

    #[test]
    fn save_as_copies_unedited_v29_apps_byte_for_byte() {
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
        let (dir, path) = write_temp(&bytes);
        let mut cache = AppInfoCache::open(&path).unwrap();
        let app = AppId::new(730);
        let mut edits = HashMap::new();
        edits.insert(app, edit_arguments(&mut cache, app));

        let destination = dir.path().join("mixed.vdf");
        cache.save_as(&destination, &edits).unwrap();
        let output = std::fs::read(&destination).unwrap();

        assert_eq!(
            v29_record(&output, 220),
            v29_record(&bytes, 220),
            "the untouched app must be copied byte-for-byte"
        );
        assert_ne!(v29_record(&output, 730), v29_record(&bytes, 730));
    }

    #[test]
    fn save_as_edit_restamps_binary_sha1_and_preserves_text_sha1_and_change_number() {
        let bytes = v29_raw_fixture();
        let (dir, path) = write_temp(&bytes);
        let mut cache = AppInfoCache::open(&path).unwrap();
        let app = AppId::new(730);
        let replacement = edit_arguments(&mut cache, app);

        let mut edits = HashMap::new();
        edits.insert(app, replacement);
        let destination = dir.path().join("edited.vdf");
        cache.save_as(&destination, &edits).unwrap();

        let output = std::fs::read(&destination).unwrap();
        // v29 header is 16 bytes, then appid/size, then the 60-byte metadata.
        let metadata_start = 16 + 8;
        assert_eq!(
            &output[metadata_start + 16..metadata_start + 36],
            b"TEXT-HASH-20-BYTES!!"
        );
        assert_eq!(
            u32::from_le_bytes([
                output[metadata_start + 36],
                output[metadata_start + 37],
                output[metadata_start + 38],
                output[metadata_start + 39],
            ]),
            4242
        );
        let size = i32::from_le_bytes([output[20], output[21], output[22], output[23]]);
        let blob_start = metadata_start + 60;
        let blob_end = blob_start + usize::try_from(size).unwrap() - 60;
        let blob = &output[blob_start..blob_end];
        assert_eq!(
            &output[metadata_start + 40..metadata_start + 60],
            Sha1::digest(blob).as_slice()
        );
        assert_ne!(
            &output[metadata_start + 40..metadata_start + 60],
            [0xAB; 20].as_slice()
        );

        // The non-UTF-8 value and the non-UTF-8 string-table entry survive the edit.
        assert!(
            output
                .windows(5)
                .any(|window| window == [0x53, 0x74, 0xFD, 0xFE, 0x72])
        );
        assert!(output.windows(5).any(|window| window == b"\xFDkey\0"));

        let table_offset = i64::from_le_bytes(output[8..16].try_into().unwrap());
        let table_start = usize::try_from(table_offset).unwrap();
        let count = u32::from_le_bytes(output[table_start..table_start + 4].try_into().unwrap());
        assert_eq!(count, 7, "the new key is interned at the end");

        // Existing raw table entries, including the non-UTF-8 one, are unchanged and
        // stay ahead of the appended key.
        let source_table_offset = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let source_table_start = usize::try_from(source_table_offset).unwrap();
        assert!(output[table_start + 4..].starts_with(&bytes[source_table_start + 4..]));

        let mut reread = AppInfoCache::open(&destination).unwrap();
        let app = reread.get(app).unwrap().unwrap();
        assert_eq!(
            app.body().unwrap().get_text("arguments").as_deref(),
            Some("-novid")
        );
        assert_eq!(app.change_number, 4242);
        assert_eq!(
            app.body()
                .unwrap()
                .items()
                .iter()
                .filter(|property| property.name == "dup")
                .count(),
            2
        );
        assert_eq!(
            app.body().unwrap().find("legacy").unwrap().as_text(),
            String::from_utf8_lossy(&[0x53, 0x74, 0xFD, 0xFE, 0x72])
        );
        assert!(matches!(
            app.body().unwrap().find("pointer").unwrap().value,
            BvdfValue::Pointer(77)
        ));
        assert!(matches!(
            app.body().unwrap().find("color").unwrap().value,
            BvdfValue::Color(0x1122_3344)
        ));
    }

    #[test]
    fn save_as_round_trips_v27_and_v28_and_restamps_only_v28() {
        for (magic, meta_length) in [(MAGIC_27, 40), (MAGIC_28, 60)] {
            let apps = vec![(
                440,
                123,
                [0x11; 20],
                app_body("Team Fortress 2", "game", "hl2.exe"),
            )];
            let bytes = build_inline(magic, &apps);
            let (dir, path) = write_temp(&bytes);
            let mut cache = AppInfoCache::open(&path).unwrap();

            let untouched = dir.path().join("untouched.vdf");
            cache.save_as(&untouched, &HashMap::new()).unwrap();
            assert_eq!(std::fs::read(&untouched).unwrap(), bytes);

            let app = AppId::new(440);
            let replacement = edit_arguments(&mut cache, app);
            let mut edits = HashMap::new();
            edits.insert(app, replacement);
            let destination = dir.path().join("edited.vdf");
            cache.save_as(&destination, &edits).unwrap();
            let output = std::fs::read(&destination).unwrap();

            let metadata_start = 8 + 8;
            assert_eq!(
                &output[metadata_start + 16..metadata_start + 36],
                b"TEXT-HASH-20-BYTES!!"
            );
            assert_eq!(
                u32::from_le_bytes([
                    output[metadata_start + 36],
                    output[metadata_start + 37],
                    output[metadata_start + 38],
                    output[metadata_start + 39],
                ]),
                123
            );
            if magic == MAGIC_28 {
                let size = i32::from_le_bytes([output[12], output[13], output[14], output[15]]);
                let blob_start = metadata_start + meta_length;
                let blob_end = blob_start + usize::try_from(size).unwrap() - meta_length;
                assert_eq!(
                    &output[metadata_start + 40..metadata_start + 60],
                    Sha1::digest(&output[blob_start..blob_end]).as_slice()
                );
            } else {
                assert_eq!(
                    &output[metadata_start..metadata_start + meta_length],
                    &bytes[metadata_start..metadata_start + meta_length]
                );
                assert_eq!(cache.meta_length(), 40);
            }

            let mut reread = AppInfoCache::open(&destination).unwrap();
            assert_eq!(
                reread
                    .get(app)
                    .unwrap()
                    .unwrap()
                    .body()
                    .unwrap()
                    .get_text("arguments")
                    .as_deref(),
                Some("-novid")
            );
        }
    }

    #[test]
    fn save_as_rejects_an_interior_nul_in_a_new_string_table_key() {
        let bytes = v29_raw_fixture();
        let (dir, path) = write_temp(&bytes);
        let mut cache = AppInfoCache::open(&path).unwrap();
        let app = AppId::new(730);
        let mut replacement = edit_arguments(&mut cache, app);
        replacement
            .get_table_mut("appinfo")
            .unwrap()
            .push_text("bad\0key", "value");

        let mut edits = HashMap::new();
        edits.insert(app, replacement);
        let error = cache
            .save_as(&dir.path().join("bad.vdf"), &edits)
            .unwrap_err()
            .to_string();
        assert!(error.contains("interior NUL"), "{error}");
    }

    #[test]
    fn save_as_rejects_the_source_path_and_leaves_it_untouched() {
        let bytes = v29_raw_fixture();
        let (_dir, path) = write_temp(&bytes);
        let mut cache = AppInfoCache::open(&path).unwrap();

        let error = cache
            .save_as(&path, &HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("source file"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn save_as_ignores_edits_for_apps_missing_from_the_file() {
        let bytes = v29_raw_fixture();
        let (dir, path) = write_temp(&bytes);
        let mut cache = AppInfoCache::open(&path).unwrap();

        let mut edits = HashMap::new();
        edits.insert(AppId::new(999), Bvdf::new());
        let destination = dir.path().join("unchanged.vdf");
        cache.save_as(&destination, &edits).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
    }

    #[test]
    fn save_as_rejects_a_source_that_shrank_after_open() {
        let bytes = v29_raw_fixture();
        let (dir, path) = write_temp(&bytes);
        let mut cache = AppInfoCache::open(&path).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(20)
            .unwrap();

        let destination = dir.path().join("partial.vdf");
        let error = cache
            .save_as(&destination, &HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("past the current source length"), "{error}");
        assert!(!destination.exists());
    }
}
