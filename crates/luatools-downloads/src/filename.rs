//! Filename sanitisation for files arriving from a URL or `Content-Disposition`.
//!
//! A remote name is never trusted as a path: only its final component is kept, separators and
//! characters Windows rejects are replaced, reserved device names are prefixed, and trailing
//! dots/spaces are removed. The result is always a single, non-empty path component.

/// Name used when the remote name carries nothing usable.
pub const DEFAULT_FILENAME: &str = "download.bin";

/// Longest filename retained, in characters. Well below the 255-byte limit of common
/// filesystems so a long UTF-8 name still fits.
const MAX_FILENAME_CHARS: usize = 200;

/// Turns an untrusted remote name into a safe single-component filename.
///
/// Directory components are discarded (`../../etc/passwd` becomes `passwd`, a Windows path
/// becomes its last component), control and reserved characters become `_`, Windows device
/// names gain a `_` prefix (`CON` becomes `_CON`), and an empty or dot-only result falls back
/// to [`DEFAULT_FILENAME`].
///
/// # Examples
///
/// ```
/// # use luatools_downloads::sanitize_filename;
/// assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
/// assert_eq!(sanitize_filename(r"C:\Windows\CON.txt"), "_CON.txt");
/// assert_eq!(sanitize_filename(".."), "download.bin");
/// ```
#[must_use]
pub fn sanitize_filename(input: &str) -> String {
    // Keep only the last component of either separator style. `rsplit` always yields at least
    // one item, so `next()` cannot miss.
    let base = input.rsplit(['/', '\\']).next().unwrap_or(input).trim();

    if base.is_empty() || base == "." || base == ".." {
        return DEFAULT_FILENAME.to_owned();
    }

    let mut cleaned = String::with_capacity(base.len());
    for ch in base.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            cleaned.push('_');
        } else {
            cleaned.push(ch);
        }
    }

    // Trailing dots and spaces are not addressable on Windows and confuse shell tools.
    let cleaned = cleaned.trim_end_matches([' ', '.']);
    if cleaned.is_empty() {
        return DEFAULT_FILENAME.to_owned();
    }

    let mut name = if is_windows_reserved(cleaned) {
        format!("_{cleaned}")
    } else {
        cleaned.to_owned()
    };

    if name.chars().count() > MAX_FILENAME_CHARS {
        name = name.chars().take(MAX_FILENAME_CHARS).collect();
        name = name.trim_end_matches([' ', '.']).to_owned();
    }

    if name.is_empty() {
        DEFAULT_FILENAME.to_owned()
    } else {
        name
    }
}

/// Whether `name` is one of the Windows device names, with or without an extension.
fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();

    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$") {
        return true;
    }

    let bytes = upper.as_bytes();
    if upper.is_ascii()
        && bytes.len() == 4
        && matches!(&upper[..3], "COM" | "LPT")
        && (b'1'..=b'9').contains(&bytes[3])
    {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_components_cannot_escape() {
        assert_eq!(sanitize_filename(".."), DEFAULT_FILENAME);
        assert_eq!(sanitize_filename("../.."), DEFAULT_FILENAME);
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("folder/.."), DEFAULT_FILENAME);
        assert_eq!(sanitize_filename(r"..\..\boot.ini"), "boot.ini");
    }

    #[test]
    fn absolute_paths_keep_the_last_component() {
        assert_eq!(sanitize_filename("/etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("/"), DEFAULT_FILENAME);
        assert_eq!(
            sanitize_filename(r"C:\Windows\system32\drivers\etc\hosts"),
            "hosts"
        );
        assert_eq!(sanitize_filename(r"\\server\share\tool.zip"), "tool.zip");
    }

    #[test]
    fn separators_and_invalid_characters_are_removed_or_replaced() {
        assert_eq!(sanitize_filename("a/b\\c"), "c");
        assert_eq!(sanitize_filename("a:b*c?.txt"), "a_b_c_.txt");
        assert_eq!(sanitize_filename("tab\tand\nnewline"), "tab_and_newline");
        assert_eq!(sanitize_filename("pipe|name"), "pipe_name");
    }

    #[test]
    fn windows_reserved_names_get_a_prefix() {
        for name in ["CON", "con", "PRN", "AUX", "NUL", "COM1", "lpt9"] {
            let sanitized = sanitize_filename(name);
            assert_eq!(sanitized, format!("_{name}"), "{name} should be prefixed");
        }
        assert_eq!(sanitize_filename("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_filename("LPT9.zip"), "_LPT9.zip");
        assert_eq!(sanitize_filename("COM0"), "COM0");
        assert_eq!(sanitize_filename("console.log"), "console.log");
        assert_eq!(sanitize_filename("LPT10.log"), "LPT10.log");
    }

    #[test]
    fn empty_and_blank_names_fall_back() {
        assert_eq!(sanitize_filename(""), DEFAULT_FILENAME);
        assert_eq!(sanitize_filename("   "), DEFAULT_FILENAME);
        assert_eq!(sanitize_filename("."), DEFAULT_FILENAME);
        assert_eq!(sanitize_filename("..."), DEFAULT_FILENAME);
        assert_eq!(sanitize_filename("/"), DEFAULT_FILENAME);
    }

    #[test]
    fn trailing_dots_and_spaces_are_trimmed() {
        assert_eq!(sanitize_filename("name. "), "name");
        assert_eq!(sanitize_filename("name..."), "name");
        assert_eq!(sanitize_filename("name "), "name");
    }

    #[test]
    fn long_names_are_truncated_to_a_safe_length() {
        let long = format!("{}.zip", "a".repeat(1000));
        let sanitized = sanitize_filename(&long);
        assert!(sanitized.chars().count() <= MAX_FILENAME_CHARS);
        assert!(sanitized.starts_with("aaa"));
    }

    #[test]
    fn non_ascii_four_byte_names_do_not_panic() {
        // Four UTF-8 bytes whose first three are half of a multi-byte character.
        assert_eq!(sanitize_filename("€1"), "€1");
        assert_eq!(sanitize_filename("é12"), "é12");
    }

    #[test]
    fn regular_names_are_unchanged() {
        assert_eq!(
            sanitize_filename("lua-tools_v1.2.3.zip"),
            "lua-tools_v1.2.3.zip"
        );
        assert_eq!(sanitize_filename("café.zip"), "café.zip");
    }
}
