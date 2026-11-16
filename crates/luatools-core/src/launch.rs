//! Steam Play-button launch entries.
//!
//! Steam stores the entries behind a game's Play button under `config.launch.<n>` in
//! `appinfo.vdf`. The record lives in the core crate so commands and events can carry a
//! structured edit without depending on the binary appinfo codec.

use serde::{Deserialize, Serialize};

/// One entry behind Steam's Play button.
///
/// Unused optional fields are empty strings. Steam omits them from appinfo entirely rather
/// than writing blank values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct LaunchOption {
    /// The `config.launch` key this entry was read from, when known.
    #[serde(default = "default_index")]
    pub index: String,
    /// The original `config.launch` key, retained across reorders.
    #[serde(default)]
    pub source_index: String,
    /// Executable to run, relative to the install directory.
    pub executable: String,
    /// Command-line arguments.
    pub arguments: String,
    /// Working directory, usually empty.
    pub working_dir: String,
    /// Human-readable label shown by Steam.
    pub description: String,
    /// Steam option type (`default`, `option1`, ...).
    #[serde(rename = "Type")]
    pub option_type: String,
    /// Operating systems this entry applies to (`windows`, `linux,macos`, ...).
    pub os_list: String,
    /// CPU architecture restriction, usually empty.
    pub os_arch: String,
    /// Beta branch key required for this entry.
    pub beta_key: String,
    /// DLC ownership marker carried by some entries.
    pub owns_dlc: String,
}

impl Default for LaunchOption {
    fn default() -> Self {
        Self {
            index: default_index(),
            source_index: String::new(),
            executable: String::new(),
            arguments: String::new(),
            working_dir: String::new(),
            description: String::new(),
            option_type: "default".to_owned(),
            os_list: String::new(),
            os_arch: String::new(),
            beta_key: String::new(),
            owns_dlc: String::new(),
        }
    }
}

fn default_index() -> String {
    "0".to_owned()
}

impl LaunchOption {
    /// A short label built from the executable, arguments, and OS list.
    ///
    /// Values are joined with two spaces, a middle dot, and two spaces.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut parts = Vec::with_capacity(3);
        if !self.executable.trim().is_empty() {
            parts.push(self.executable.trim());
        }
        if !self.arguments.trim().is_empty() {
            parts.push(self.arguments.trim());
        }
        if !self.os_list.trim().is_empty() {
            parts.push(self.os_list.trim());
        }
        parts.join("  ·  ")
    }

    /// Returns true when this option applies to `os`.
    ///
    /// An empty OS list matches every platform. Tokens are comma- or semicolon-separated
    /// and compared ASCII case-insensitively. `win`, `mac`, and `osx` are accepted as
    /// aliases for `windows` and `macos`.
    #[must_use]
    pub fn applies_to_os(&self, os: &str) -> bool {
        let list = self.os_list.trim();
        if list.is_empty() {
            return true;
        }

        let current = normalize_os(os);
        list.split([',', ';'])
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .any(|token| normalize_os(token) == current)
    }

    /// Returns true when this option applies to the platform the process is running on.
    #[must_use]
    pub fn applies_to_current_os(&self) -> bool {
        self.applies_to_os(std::env::consts::OS)
    }
}

/// Filters launch options to those matching `os`, preserving input order.
#[must_use]
pub fn filter_for_os<'a>(options: &'a [LaunchOption], os: &str) -> Vec<&'a LaunchOption> {
    options
        .iter()
        .filter(|option| option.applies_to_os(os))
        .collect()
}

fn normalize_os(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "win" | "windows" => "windows".to_owned(),
        "mac" | "macos" | "osx" | "darwin" => "macos".to_owned(),
        "linux" | "steamos" => "linux".to_owned(),
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(executable: &str, arguments: &str, os_list: &str) -> LaunchOption {
        LaunchOption {
            executable: executable.to_owned(),
            arguments: arguments.to_owned(),
            os_list: os_list.to_owned(),
            description: "Play".to_owned(),
            ..LaunchOption::default()
        }
    }

    #[test]
    fn summary_joins_only_non_empty_parts() {
        assert_eq!(
            option("game.exe", "-novid", "windows").summary(),
            "game.exe  ·  -novid  ·  windows"
        );
        assert_eq!(option("", "", "").summary(), "");
    }

    #[test]
    fn os_matching_accepts_aliases_and_empty_lists() {
        assert!(option("a", "", "").applies_to_os("linux"));
        assert!(option("a", "", "Windows").applies_to_os("win"));
        assert!(option("a", "", "linux,macos").applies_to_os("osx"));
        assert!(!option("a", "", "windows").applies_to_os("linux"));
    }

    #[test]
    fn filtering_preserves_order() {
        let options = vec![
            option("a", "", "windows"),
            option("b", "", ""),
            option("c", "", "linux"),
        ];
        let filtered = filter_for_os(&options, "linux");
        assert_eq!(
            filtered
                .iter()
                .map(|option| option.executable.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );
    }
}
