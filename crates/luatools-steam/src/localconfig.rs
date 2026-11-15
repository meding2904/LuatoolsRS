//! Per-user launch options stored in Steam's `localconfig.vdf`.
//!
//! Steam keeps the launch options the user edits in the client properties dialog in
//! `userdata/<account>/config/localconfig.vdf`, at
//! `UserLocalConfigStore/Software/Valve/Steam/apps/<appid>/LaunchOptions`. Writing that key is
//! the supported way to change launch options without rewriting the shared `appinfo.vdf`
//! binary; Steam reads it on startup, so a restart is required and the client may overwrite
//! the file if it is running while we write.

use std::path::{Path, PathBuf};

use luatools_core::{AppId, Error, Result};

use crate::vdf::{self, Vdf};

/// Key path from the file root to one app's settings.
fn app_path(app_id: AppId) -> String {
    format!(
        "UserLocalConfigStore.Software.Valve.Steam.apps.{}",
        app_id.get()
    )
}

/// Lists every `userdata/<account>/config/localconfig.vdf` under a Steam root.
#[must_use]
pub fn localconfig_paths(steam_root: &Path) -> Vec<PathBuf> {
    let userdata = steam_root.join("userdata");
    let Ok(accounts) = std::fs::read_dir(&userdata) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = accounts
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join("config").join("localconfig.vdf"))
        .filter(|path| path.is_file())
        .collect();
    paths.sort();
    paths
}

/// Returns the most recently modified account's config, which is the active client account.
#[must_use]
pub fn active_localconfig(steam_root: &Path) -> Option<PathBuf> {
    localconfig_paths(steam_root)
        .into_iter()
        .filter_map(|path| {
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            Some((modified, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

/// Reads one app's configured launch options.
///
/// # Errors
///
/// Returns an error when the file cannot be read or parsed.
pub fn read_launch_options(path: &Path, app_id: AppId) -> Result<Option<String>> {
    let root = vdf::parse_file(path)?;
    let Some(apps) = root.table("UserLocalConfigStore.Software.Valve.Steam.apps") else {
        return Ok(None);
    };
    let Some(app) = apps.table(&app_id.to_string()) else {
        return Ok(None);
    };
    let Some(value) = app.as_str("LaunchOptions") else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value.to_owned()))
    }
}

/// Writes one app's launch options, creating the nested tables when missing.
///
/// An empty `options` string removes the key. The first write to a file keeps a
/// `<name>.luatools.bak` copy so the user can restore Steam's original file.
///
/// # Errors
///
/// Returns an error when the file cannot be read, parsed, or replaced, or when the parsed file
/// is not a VDF object at the root.
pub fn write_launch_options(path: &Path, app_id: AppId, options: &str) -> Result<()> {
    let mut root = vdf::parse_file(path)?;
    let table = root.obtain_table(&app_path(app_id));
    let options = options.trim();
    if options.is_empty() {
        table.remove_key("LaunchOptions");
    } else {
        table.insert_str("LaunchOptions", options);
    }
    // Remove an app table we created but left empty, so the file stays tidy.
    let apps_empty = root
        .table("Software/Valve/Steam/apps")
        .is_some_and(|apps| apps.table(&app_id.to_string()).is_some_and(Vdf::is_empty));
    if apps_empty {
        let apps = root.obtain_table("UserLocalConfigStore.Software.Valve.Steam.apps");
        apps.remove_key(&app_id.to_string());
    }

    let backup = path.with_extension("vdf.luatools.bak");
    if !backup.exists() {
        std::fs::copy(path, &backup)
            .map_err(|error| Error::io(format!("backing up {}", path.display()), error))?;
    }
    let text = vdf::to_string_pretty(&root);
    let temp = path.with_extension("vdf.tmp");
    std::fs::write(&temp, text)
        .map_err(|error| Error::io(format!("writing {}", temp.display()), error))?;
    std::fs::rename(&temp, path)
        .map_err(|error| Error::io(format!("replacing {}", path.display()), error))?;
    Ok(())
}

/// Applies launch options to the active Steam account.
///
/// Returns the file that was written, or `None` when Steam has no userdata directory yet.
///
/// # Errors
///
/// Propagates read and write failures from [`write_launch_options`].
pub fn apply_launch_options(
    steam_root: &Path,
    app_id: AppId,
    options: &str,
) -> Result<Option<PathBuf>> {
    let Some(path) = active_localconfig(steam_root) else {
        return Ok(None);
    };
    write_launch_options(&path, app_id, options)?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#""UserLocalConfigStore"
{
	"Software"
	{
		"Valve"
		{
			"Steam"
			{
				"apps"
				{
					"730"
					{
						"LaunchOptions"		"-novid"
						"LastPlayed"		"1700000000"
					}
				}
			}
		}
	}
}
"#;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("localconfig.vdf");
        std::fs::write(&path, SAMPLE).unwrap();
        (tmp, path)
    }

    #[test]
    fn reads_existing_options() {
        let (_tmp, path) = fixture();
        assert_eq!(
            read_launch_options(&path, AppId::new(730))
                .unwrap()
                .as_deref(),
            Some("-novid")
        );
        assert_eq!(read_launch_options(&path, AppId::new(570)).unwrap(), None);
    }

    #[test]
    fn writes_options_and_keeps_other_keys() {
        let (_tmp, path) = fixture();
        write_launch_options(&path, AppId::new(730), "-console -dx11").unwrap();
        let root = vdf::parse_file(&path).unwrap();
        assert_eq!(
            root.as_str("UserLocalConfigStore.Software.Valve.Steam.apps.730.LaunchOptions"),
            Some("-console -dx11")
        );
        assert_eq!(
            root.as_str("UserLocalConfigStore.Software.Valve.Steam.apps.730.LastPlayed"),
            Some("1700000000")
        );
        assert!(path.with_extension("vdf.luatools.bak").exists());
    }

    #[test]
    fn empty_value_removes_the_key() {
        let (_tmp, path) = fixture();
        write_launch_options(&path, AppId::new(730), "   ").unwrap();
        let root = vdf::parse_file(&path).unwrap();
        assert_eq!(
            root.as_str("UserLocalConfigStore.Software.Valve.Steam.apps.730.LaunchOptions"),
            None
        );
    }

    #[test]
    fn creates_missing_app_and_branch_tables() {
        let (_tmp, path) = fixture();
        write_launch_options(&path, AppId::new(570), "-console").unwrap();
        let root = vdf::parse_file(&path).unwrap();
        assert_eq!(
            root.as_str("UserLocalConfigStore.Software.Valve.Steam.apps.570.LaunchOptions"),
            Some("-console")
        );
    }

    #[test]
    fn finds_localconfigs_and_picks_the_newest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for account in ["111", "222"] {
            let dir = root.join("userdata").join(account).join("config");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("localconfig.vdf"), SAMPLE).unwrap();
        }
        assert_eq!(localconfig_paths(root).len(), 2);
        assert!(active_localconfig(root).is_some());
    }
}
