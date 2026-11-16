//! Steam depot manifest (`.manifest`) parsing.
//!
//! A depot manifest is a flat sequence of sections:
//!
//! ```text
//! [magic: u32 LE][length: u32 LE][protobuf payload: length bytes]
//! ```
//!
//! The payload section contains every `FileMapping`; the metadata section carries the
//! depot id, manifest gid, creation time, and the authoritative installed size. Unknown
//! sections (including the signature) are skipped.
//!
//! This module parses the payload too, so callers can list a depot's files without a
//! separate protobuf dependency. Paths are returned exactly as stored (Base64 while their
//! filenames are encrypted). [`key_looks_valid`] can optionally test a depot key against
//! the first encrypted filename; no download or depot-key pipeline lives here.

use std::io::{Cursor, Read as _};
use std::path::Path;

use base64::Engine as _;
use luatools_core::{DepotId, Error, ManifestId, Result};

/// Section magic for `ContentManifestPayload`.
pub const PAYLOAD_MAGIC: u32 = 0x71F6_17D0;
/// Section magic for `ContentManifestMetadata`.
pub const METADATA_MAGIC: u32 = 0x1F48_12BE;
/// Optional end-of-file section magic.
pub const EOF_MAGIC: u32 = 0x32C4_15AB;

/// One file entry from a depot manifest's payload section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFile {
    /// Path relative to the depot root, as stored in the manifest.
    pub path: String,
    /// Uncompressed file size in bytes.
    pub size: u64,
    /// SHA-1 of the complete file, or zero when the field was absent.
    pub sha1: [u8; 20],
    /// Steam file flags.
    pub flags: u32,
}

/// A parsed depot manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Depot this manifest belongs to.
    pub depot_id: DepotId,
    /// Manifest id (gid).
    pub gid: ManifestId,
    /// Creation time as a Unix timestamp.
    pub creation: u64,
    /// True when the payload's filenames are Base64-encoded ciphertext.
    pub filenames_encrypted: bool,
    /// Files described by the payload section. Empty when the payload was omitted.
    pub files: Vec<ManifestFile>,
    /// Installed size from the metadata section, falling back to the sum of file sizes.
    pub total_size: u64,
}

/// Parses a depot manifest from its bytes.
///
/// A missing payload section is accepted (the metadata-only manifests used by Steam's
/// cache migration are valid); a missing metadata section is an error because the depot
/// identity is unknown. A ZIP container (first two bytes `PK`) is inflated whole and its
/// first entry is used, matching Steam's zipped manifests.
///
/// # Errors
///
/// Returns a parse error for an invalid ZIP archive, a truncated section, a missing
/// metadata section, an over-long protobuf varint, or an unsupported protobuf group.
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest> {
    let inflated = if bytes.starts_with(b"PK") {
        Some(inflate_zip(bytes)?)
    } else {
        None
    };
    let data = inflated.as_deref().unwrap_or(bytes);

    let mut metadata = None;
    let mut files = None;
    let mut position = 0usize;

    while position < data.len() {
        let header_end = position
            .checked_add(8)
            .ok_or_else(|| Error::parse("manifest", "section header offset overflow"))?;
        let header = data.get(position..header_end).ok_or_else(|| {
            Error::parse(
                "manifest",
                format!("truncated section header at offset {position}"),
            )
        })?;
        let magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        position = header_end;

        if magic == EOF_MAGIC {
            break;
        }

        let length = usize::try_from(length)
            .map_err(|_| Error::parse("manifest", "section length does not fit in usize"))?;
        let section_end = position
            .checked_add(length)
            .ok_or_else(|| Error::parse("manifest", "section length overflow"))?;
        let section = data.get(position..section_end).ok_or_else(|| {
            Error::parse(
                "manifest",
                format!("truncated section at offset {position} (length {length})"),
            )
        })?;

        match magic {
            PAYLOAD_MAGIC if files.is_none() => files = Some(parse_payload(section)?),
            METADATA_MAGIC if metadata.is_none() => metadata = Some(parse_metadata(section)?),
            _ => {}
        }
        position = section_end;
    }

    let metadata = metadata.ok_or_else(|| Error::parse("manifest", "missing metadata section"))?;
    let files = files.unwrap_or_default();
    let total_size = if metadata.size_on_disk > 0 {
        metadata.size_on_disk
    } else {
        files
            .iter()
            .fold(0u64, |total, file| total.saturating_add(file.size))
    };

    Ok(Manifest {
        depot_id: DepotId::new(metadata.depot_id),
        gid: ManifestId::new(metadata.gid),
        creation: u64::from(metadata.creation),
        filenames_encrypted: metadata.filenames_encrypted,
        files,
        total_size,
    })
}

/// Returns true when the file parses and its own depot id and gid match the expected pair.
///
/// This is how a cached `<depot>_<gid>.manifest` is verified against its filename instead
/// of being trusted because it exists. Unreadable or malformed files return false.
#[must_use]
pub fn matches(path: &Path, depot: DepotId, gid: ManifestId) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    parse_manifest(&bytes).is_ok_and(|manifest| manifest.depot_id == depot && manifest.gid == gid)
}

/// Tests whether a depot key can plausibly decrypt the manifest's first filename.
///
/// This is the format-level key check used before a depot download. It deliberately
/// reports `true` when there is nothing to test: an unreadable file, a key that is not 32
/// bytes, a manifest without encrypted filenames, no payload section, no Base64 filename,
/// or a ciphertext of the wrong shape. It returns `false` only when a filename was
/// testable and AES-256-CBC decryption produced invalid PKCS#7 padding or control bytes.
///
/// A wrong key can still pass by chance (valid padding and printable plaintext), so a
/// `true` result is "no objection", not proof that the key is correct.
#[must_use]
pub fn key_looks_valid(path: &Path, key: &[u8]) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return true;
    };
    key_looks_valid_bytes(&bytes, key)
}

/// [`key_looks_valid`] over manifest bytes already in memory.
///
/// Zipped manifests are inflated first, as in [`parse_manifest`].
#[must_use]
pub fn key_looks_valid_bytes(bytes: &[u8], key: &[u8]) -> bool {
    if key.len() != 32 {
        return true;
    }
    let Ok(manifest) = parse_manifest(bytes) else {
        return true;
    };
    if !manifest.filenames_encrypted {
        return true;
    }
    let Some(first) = manifest.files.first() else {
        return true;
    };
    let Ok(ciphertext) = base64::engine::general_purpose::STANDARD.decode(first.path.as_bytes())
    else {
        return true;
    };
    if ciphertext.len() <= 16 || !ciphertext.len().is_multiple_of(16) {
        return true;
    }
    let Some(plaintext) = decrypt_encrypted_filename(&ciphertext, key) else {
        return false;
    };
    !plaintext.iter().any(|byte| *byte < 0x20 && *byte != 0x00)
}

/// Decrypts one Steam manifest filename with a 32-byte depot key.
///
/// The first 16 ciphertext bytes are the IV encrypted with AES-256-ECB; the remainder is
/// AES-256-CBC with PKCS#7 padding. Returns `None` for a wrong key length, an impossible
/// ciphertext length, or invalid padding. Control-byte validation is the caller's job (see
/// [`key_looks_valid`]).
#[must_use]
pub fn decrypt_encrypted_filename(ciphertext: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    use aes::cipher::block_padding::Pkcs7;
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockDecrypt, BlockDecryptMut, KeyInit, KeyIvInit};

    if key.len() != 32 || ciphertext.len() <= 16 || !ciphertext.len().is_multiple_of(16) {
        return None;
    }
    let key: [u8; 32] = key.try_into().ok()?;
    let (iv_ciphertext, body) = ciphertext.split_at(16);

    let mut iv = GenericArray::clone_from_slice(iv_ciphertext);
    let ecb = <aes::Aes256 as KeyInit>::new_from_slice(&key).ok()?;
    ecb.decrypt_block(&mut iv);

    let mut buffer = body.to_vec();
    let decryptor =
        <cbc::Decryptor<aes::Aes256> as KeyIvInit>::new_from_slices(&key, iv.as_slice()).ok()?;
    let plaintext = decryptor.decrypt_padded_mut::<Pkcs7>(&mut buffer).ok()?;
    Some(plaintext.to_vec())
}

fn inflate_zip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| Error::parse("manifest", format!("invalid zipped manifest: {error}")))?;
    if archive.is_empty() {
        return Ok(bytes.to_vec());
    }
    let mut entry = archive.by_index(0).map_err(|error| {
        Error::parse(
            "manifest",
            format!("reading the first zipped manifest entry: {error}"),
        )
    })?;
    let mut contents = Vec::new();
    entry
        .read_to_end(&mut contents)
        .map_err(|error| Error::io("inflating zipped manifest", error))?;
    Ok(contents)
}

#[derive(Debug, Clone, Copy)]
struct Metadata {
    depot_id: u32,
    gid: u64,
    creation: u32,
    filenames_encrypted: bool,
    size_on_disk: u64,
}

fn parse_metadata(data: &[u8]) -> Result<Metadata> {
    let mut reader = ProtoReader::new(data);
    let mut metadata = Metadata {
        depot_id: 0,
        gid: 0,
        creation: 0,
        filenames_encrypted: false,
        size_on_disk: 0,
    };

    while !reader.is_empty() {
        let (field, wire) = reader.read_tag()?;
        match (field, wire) {
            (1, 0) => {
                metadata.depot_id = u32::try_from(reader.read_varint()?)
                    .map_err(|_| reader.error("depot id does not fit in u32"))?;
            }
            (2, 0) => metadata.gid = reader.read_varint()?,
            (3, 0) => {
                metadata.creation = u32::try_from(reader.read_varint()?)
                    .map_err(|_| reader.error("creation time does not fit in u32"))?;
            }
            (4, 0) => metadata.filenames_encrypted = reader.read_varint()? != 0,
            (5, 0) => metadata.size_on_disk = reader.read_varint()?,
            _ => reader.skip(wire)?,
        }
    }
    Ok(metadata)
}

fn parse_payload(data: &[u8]) -> Result<Vec<ManifestFile>> {
    let mut reader = ProtoReader::new(data);
    let mut files = Vec::new();

    while !reader.is_empty() {
        let (field, wire) = reader.read_tag()?;
        if field == 1 && wire == 2 {
            let mapping = reader.read_bytes()?;
            files.push(parse_file_mapping(mapping)?);
        } else {
            reader.skip(wire)?;
        }
    }
    Ok(files)
}

fn parse_file_mapping(data: &[u8]) -> Result<ManifestFile> {
    let mut reader = ProtoReader::new(data);
    let mut file = ManifestFile {
        path: String::new(),
        size: 0,
        sha1: [0u8; 20],
        flags: 0,
    };

    while !reader.is_empty() {
        let (field, wire) = reader.read_tag()?;
        match (field, wire) {
            (1, 2) => {
                file.path = String::from_utf8_lossy(reader.read_bytes()?).into_owned();
            }
            (2, 0) => file.size = reader.read_varint()?,
            (3, 0) => {
                file.flags = u32::try_from(reader.read_varint()?)
                    .map_err(|_| reader.error("file flags do not fit in u32"))?;
            }
            (5, 2) => {
                let digest = reader.read_bytes()?;
                if digest.len() != file.sha1.len() {
                    return Err(reader.error("file SHA-1 is not 20 bytes"));
                }
                file.sha1.copy_from_slice(digest);
            }
            _ => reader.skip(wire)?,
        }
    }
    Ok(file)
}

struct ProtoReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> ProtoReader<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    const fn is_empty(&self) -> bool {
        self.position >= self.data.len()
    }

    fn read_tag(&mut self) -> Result<(u32, u8)> {
        let tag = self.read_varint()?;
        let field =
            u32::try_from(tag >> 3).map_err(|_| self.error("field number does not fit in u32"))?;
        if field == 0 {
            return Err(self.error("field number 0 is invalid"));
        }
        Ok((field, (tag & 0x07) as u8))
    }

    fn read_varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let Some(byte) = self.data.get(self.position).copied() else {
                return Err(self.error("truncated varint"));
            };
            self.position += 1;

            if shift == 63 && byte > 1 {
                return Err(self.error("varint overflows u64"));
            }
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }

            shift += 7;
            if shift > 63 {
                return Err(self.error("varint is longer than 10 bytes"));
            }
        }
    }

    fn read_bytes(&mut self) -> Result<&'a [u8]> {
        let length = self.read_varint()?;
        let length =
            usize::try_from(length).map_err(|_| self.error("length does not fit in usize"))?;
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| self.error("length overflow"))?;
        let bytes = self
            .data
            .get(self.position..end)
            .ok_or_else(|| self.error("length-delimited field runs past the end"))?;
        self.position = end;
        Ok(bytes)
    }

    fn skip(&mut self, wire: u8) -> Result<()> {
        match wire {
            0 => {
                self.read_varint()?;
            }
            1 => self.skip_bytes(8)?,
            2 => {
                let length = self.read_varint()?;
                let length = usize::try_from(length)
                    .map_err(|_| self.error("length does not fit in usize"))?;
                self.skip_bytes(length)?;
            }
            5 => self.skip_bytes(4)?,
            other => {
                return Err(self.error(&format!("unsupported protobuf wire type {other}")));
            }
        }
        Ok(())
    }

    fn skip_bytes(&mut self, count: usize) -> Result<()> {
        let end = self
            .position
            .checked_add(count)
            .ok_or_else(|| self.error("skip length overflow"))?;
        if end > self.data.len() {
            return Err(self.error("field runs past the end"));
        }
        self.position = end;
        Ok(())
    }

    fn error(&self, message: &str) -> Error {
        Error::parse(
            "manifest",
            format!("{message} at protobuf offset {}", self.position),
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aes::cipher::block_padding::Pkcs7;
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncrypt, BlockEncryptMut, KeyInit, KeyIvInit};
    use std::io::Write as _;

    fn varint(mut value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            let mut byte = (value & 0x7F) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            bytes.push(byte);
            if value == 0 {
                return bytes;
            }
        }
    }

    fn varint_field(field: u32, value: u64) -> Vec<u8> {
        let mut bytes = varint(u64::from(field << 3));
        bytes.extend(varint(value));
        bytes
    }

    fn bytes_field(field: u32, value: &[u8]) -> Vec<u8> {
        let mut bytes = varint(u64::from((field << 3) | 2));
        bytes.extend(varint(value.len() as u64));
        bytes.extend_from_slice(value);
        bytes
    }

    fn section(magic: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + payload.len());
        bytes.extend_from_slice(&magic.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn fixture() -> Vec<u8> {
        let mut mapping = Vec::new();
        mapping.extend(bytes_field(1, b"game/bin/linuxsteamrt64/cs2"));
        mapping.extend(varint_field(2, 1234));
        mapping.extend(varint_field(3, 64));
        mapping.extend(bytes_field(5, &[0xAB; 20]));

        let mut payload = bytes_field(1, &mapping);
        payload.extend(bytes_field(7, b"target"));

        let mut metadata = Vec::new();
        metadata.extend(varint_field(1, 730));
        metadata.extend(varint_field(2, 7_649_880_572_861_247_491));
        metadata.extend(varint_field(3, 1_700_000_000));
        metadata.extend(varint_field(4, 0));
        metadata.extend(varint_field(5, 4096));
        // Unknown fixed64 field: the parser must skip it.
        metadata.extend(varint((9u64 << 3) | 1));
        metadata.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);

        let mut manifest = section(PAYLOAD_MAGIC, &payload);
        manifest.extend(section(METADATA_MAGIC, &metadata));
        manifest.extend(section(EOF_MAGIC, &[]));
        manifest
    }

    fn zip_manifest(entries: &[&[u8]]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (index, entry) in entries.iter().enumerate() {
            writer
                .start_file(
                    format!("{index}.manifest"),
                    zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Stored),
                )
                .unwrap();
            writer.write_all(entry).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn encrypt_filename(name: &[u8], key: &[u8; 32], iv: &[u8; 16]) -> Vec<u8> {
        let ecb = aes::Aes256::new_from_slice(key).unwrap();
        let mut iv_block = GenericArray::clone_from_slice(iv);
        ecb.encrypt_block(&mut iv_block);

        let encryptor =
            <cbc::Encryptor<aes::Aes256> as KeyIvInit>::new_from_slices(key, iv).unwrap();
        let ciphertext = encryptor.encrypt_padded_vec_mut::<Pkcs7>(name);

        let mut output = iv_block.to_vec();
        output.extend_from_slice(&ciphertext);
        output
    }

    fn encrypted_fixture(name: &[u8], key: &[u8; 32], iv: &[u8; 16]) -> Vec<u8> {
        let filename =
            base64::engine::general_purpose::STANDARD.encode(encrypt_filename(name, key, iv));
        let mut mapping = bytes_field(1, filename.as_bytes());
        mapping.extend(varint_field(2, 1234));
        let payload = bytes_field(1, &mapping);

        let mut metadata = varint_field(1, 730);
        metadata.extend(varint_field(2, 99));
        metadata.extend(varint_field(4, 1));

        let mut manifest = section(PAYLOAD_MAGIC, &payload);
        manifest.extend(section(METADATA_MAGIC, &metadata));
        manifest
    }

    #[test]
    fn parses_payload_and_metadata_concrete_values() {
        let manifest = parse_manifest(&fixture()).unwrap();
        assert_eq!(manifest.depot_id, DepotId::new(730));
        assert_eq!(manifest.gid, ManifestId::new(7_649_880_572_861_247_491));
        assert_eq!(manifest.creation, 1_700_000_000);
        assert_eq!(manifest.total_size, 4096);
        assert_eq!(manifest.files.len(), 1);

        let file = &manifest.files[0];
        assert_eq!(file.path, "game/bin/linuxsteamrt64/cs2");
        assert_eq!(file.size, 1234);
        assert_eq!(file.flags, 64);
        assert_eq!(file.sha1, [0xAB; 20]);
    }

    #[test]
    fn metadata_only_manifests_parse_and_sum_file_sizes_when_size_is_missing() {
        let mut metadata = varint_field(1, 440);
        metadata.extend(varint_field(2, 99));
        // No payload and no cb_disk_original: total size is the sum of an empty file list.
        let bytes = section(METADATA_MAGIC, &metadata);
        let manifest = parse_manifest(&bytes).unwrap();
        assert_eq!(manifest.depot_id, DepotId::new(440));
        assert_eq!(manifest.gid, ManifestId::new(99));
        assert_eq!(manifest.total_size, 0);
        assert!(manifest.files.is_empty());

        let mut mapping = bytes_field(1, b"a.bin");
        mapping.extend(varint_field(2, 100));
        let payload = bytes_field(1, &mapping);
        let mut bytes = section(PAYLOAD_MAGIC, &payload);
        bytes.extend(section(METADATA_MAGIC, &metadata));
        let manifest = parse_manifest(&bytes).unwrap();
        assert_eq!(manifest.total_size, 100);
    }

    #[test]
    fn rejects_bad_headers_and_truncated_sections() {
        let error = parse_manifest(&[0xDE, 0xAD, 0xBE, 0xEF, 0, 0, 0, 0])
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing metadata section"), "{error}");

        let error = parse_manifest(&[]).unwrap_err().to_string();
        assert!(error.contains("missing metadata section"), "{error}");

        let mut truncated = METADATA_MAGIC.to_le_bytes().to_vec();
        truncated.extend_from_slice(&100u32.to_le_bytes());
        truncated.extend_from_slice(&[1, 2]);
        let error = parse_manifest(&truncated).unwrap_err().to_string();
        assert!(error.contains("truncated section"), "{error}");

        let error = parse_manifest(b"PK\x03\x04rest").unwrap_err().to_string();
        assert!(error.contains("zipped manifest"), "{error}");
    }

    #[test]
    fn matches_verifies_depot_and_gid_against_the_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("730_7649880572861247491.manifest");
        std::fs::write(&path, fixture()).unwrap();

        assert!(matches(
            &path,
            DepotId::new(730),
            ManifestId::new(7_649_880_572_861_247_491)
        ));
        assert!(!matches(
            &path,
            DepotId::new(731),
            ManifestId::new(7_649_880_572_861_247_491)
        ));
        assert!(!matches(&path, DepotId::new(730), ManifestId::new(1)));
        assert!(!matches(
            &dir.path().join("absent.manifest"),
            DepotId::new(730),
            ManifestId::new(7_649_880_572_861_247_491)
        ));
    }

    #[test]
    fn rejects_overlong_varints_and_groups() {
        let mut metadata = vec![0x80; 11];
        metadata.push(0x00);
        let bytes = section(METADATA_MAGIC, &metadata);
        let error = parse_manifest(&bytes).unwrap_err().to_string();
        assert!(error.contains("varint"), "{error}");

        let group = varint((1u64 << 3) | 3);
        let bytes = section(METADATA_MAGIC, &group);
        let error = parse_manifest(&bytes).unwrap_err().to_string();
        assert!(error.contains("wire type 3"), "{error}");
    }

    #[test]
    fn parses_the_first_entry_of_a_zipped_manifest() {
        let first = fixture();
        let mut other_metadata = varint_field(1, 440);
        other_metadata.extend(varint_field(2, 1));
        let other = section(METADATA_MAGIC, &other_metadata);
        let zipped = zip_manifest(&[&first, &other]);

        let manifest = parse_manifest(&zipped).unwrap();
        assert_eq!(manifest.depot_id, DepotId::new(730));
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].path, "game/bin/linuxsteamrt64/cs2");
        assert!(!manifest.filenames_encrypted);

        let empty = zip_manifest(&[]);
        assert!(parse_manifest(&empty).is_err());
    }

    #[test]
    fn matches_accepts_zipped_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("730_7649880572861247491.manifest");
        std::fs::write(&path, zip_manifest(&[&fixture()])).unwrap();
        assert!(matches(
            &path,
            DepotId::new(730),
            ManifestId::new(7_649_880_572_861_247_491)
        ));
    }

    #[test]
    fn key_check_accepts_the_right_key_and_rejects_the_wrong_one() {
        let key = [0x42; 32];
        let iv = [0x11; 16];
        let bytes = encrypted_fixture(b"game/bin/linuxsteamrt64/cs2", &key, &iv);

        let manifest = parse_manifest(&bytes).unwrap();
        assert!(manifest.filenames_encrypted);
        assert!(key_looks_valid_bytes(&bytes, &key));
        assert!(!key_looks_valid_bytes(&bytes, &[0x43; 32]));
        assert!(key_looks_valid_bytes(&bytes, &[0x42; 31]));

        // Zipped input is inflated before the check.
        let zipped = zip_manifest(&[&bytes]);
        assert!(key_looks_valid_bytes(&zipped, &key));
        assert!(!key_looks_valid_bytes(&zipped, &[0x43; 32]));
    }

    #[test]
    fn key_check_has_no_objection_without_something_testable() {
        let key = [0x42; 32];
        let iv = [0x11; 16];

        // Filenames are not encrypted.
        assert!(key_looks_valid_bytes(&fixture(), &key));

        // Encrypted but no payload, no files, or an empty filename.
        let mut metadata = varint_field(1, 730);
        metadata.extend(varint_field(2, 99));
        metadata.extend(varint_field(4, 1));
        let metadata_only = section(METADATA_MAGIC, &metadata);
        assert!(key_looks_valid_bytes(&metadata_only, &key));

        // First filename is not Base64.
        let mapping = bytes_field(1, b"not base64!!!");
        let payload = bytes_field(1, &mapping);
        let mut not_base64 = section(PAYLOAD_MAGIC, &payload);
        not_base64.extend(section(METADATA_MAGIC, &metadata));
        assert!(key_looks_valid_bytes(&not_base64, &key));

        // Ciphertext is too short / not a whole number of blocks.
        for invalid in [&[0u8; 16][..], &[0u8; 17][..], &[0u8; 33][..]] {
            let encoded = base64::engine::general_purpose::STANDARD.encode(invalid);
            let mapping = bytes_field(1, encoded.as_bytes());
            let payload = bytes_field(1, &mapping);
            let mut manifest = section(PAYLOAD_MAGIC, &payload);
            manifest.extend(section(METADATA_MAGIC, &metadata));
            assert!(key_looks_valid_bytes(&manifest, &key));
        }

        // An unreadable path has nothing to test.
        let dir = tempfile::tempdir().unwrap();
        assert!(key_looks_valid(&dir.path().join("absent.manifest"), &key));

        let encrypted = encrypted_fixture(b"game/bin/linuxsteamrt64/cs2", &key, &iv);
        assert!(key_looks_valid_bytes(&encrypted, &[]));

        let path = dir.path().join("730_99.manifest");
        std::fs::write(&path, &encrypted).unwrap();
        assert!(key_looks_valid(&path, &key));
        assert!(!key_looks_valid(&path, &[0x43; 32]));
    }

    #[test]
    fn key_check_rejects_control_bytes_after_successful_unpadding() {
        let key = [0x42; 32];
        let iv = [0x11; 16];
        let bytes = encrypted_fixture(b"\x01hidden/name", &key, &iv);
        assert!(!key_looks_valid_bytes(&bytes, &key));
    }

    #[test]
    fn decrypt_encrypted_filename_round_trips_and_rejects_bad_shapes() {
        let key = [0x42; 32];
        let iv = [0x11; 16];
        let name = b"game/bin/linuxsteamrt64/cs2";
        let ciphertext = encrypt_filename(name, &key, &iv);

        assert_eq!(
            decrypt_encrypted_filename(&ciphertext, &key).as_deref(),
            Some(name.as_slice())
        );
        assert!(decrypt_encrypted_filename(&ciphertext, &[0x42; 31]).is_none());
        assert!(decrypt_encrypted_filename(&ciphertext[..16], &key).is_none());
        assert!(decrypt_encrypted_filename(&ciphertext[..17], &key).is_none());
        assert!(decrypt_encrypted_filename(&ciphertext, &[0x43; 32]).is_none());
    }
}
