use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use jj_lib::repo_path::{RelativePathParseError, RepoPathBuf};

use crate::error::{jj_error, DotsyncError};

/// Where the release before this one kept its machine-local record of what it
/// had synced. `Home::acquire` deletes it: jj's own view holds that record now,
/// and a second copy of it is a second authority.
pub(crate) const SHED_SYNC_STATE_RELATIVE_PATH: &str = ".config/dotsync/sync-state.json";

/// The two directories one run works with: the home it manages, and the
/// hidden repo it manages it from.
#[derive(Debug, Clone)]
pub struct DotsyncPaths {
    pub repo_root: PathBuf,
    pub home_dir: PathBuf,
}

/// A home-relative path as the repo spells it: its components joined by `/`,
/// whatever this platform separates them with, without `.` components or a
/// trailing separator. A path with a root or a drive is not home-relative,
/// and is returned as given for the caller to refuse.
pub(crate) fn normalized(path: &Path) -> PathBuf {
    if path
        .components()
        .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
    {
        return path.to_path_buf();
    }
    let mut spelled = OsString::new();
    for component in path
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
    {
        if !spelled.is_empty() {
            spelled.push("/");
        }
        spelled.push(component.as_os_str());
    }
    PathBuf::from(spelled)
}

/// A home-relative path as jj names it, built from its components so that a
/// Windows `\` separates them rather than becoming part of a file name.
pub(crate) fn repo_path_of(relative: &Path) -> Result<RepoPathBuf, DotsyncError> {
    RepoPathBuf::from_relative_path(relative).map_err(|err| match err {
        RelativePathParseError::InvalidUtf8 { .. } => DotsyncError::NonUtf8Path {
            path: relative.to_path_buf(),
        },
        err => jj_error(format!("invalid repo path {}: {err}", relative.display())),
    })
}

/// Why Windows cannot hold a file at this repo path, if it cannot: a name
/// with a character Windows refuses (`a\b` is one file on Linux and two
/// folders deep on Windows), a name ending in a dot or space, which Windows
/// silently trims, or a name Windows reserves for a device.
///
/// Splits on `/` itself rather than reading `Path` components, so the answer
/// is the same on every platform the check runs on.
pub(crate) fn windows_cannot_name(path: &Path) -> Option<String> {
    const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL"];
    let spelled = path.to_string_lossy();
    spelled.split('/').find_map(|name| {
        if let Some(bad) = name
            .chars()
            .find(|c| matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control())
        {
            return Some(format!(
                "`{name}` holds {bad:?}, which Windows refuses in a name"
            ));
        }
        if name.ends_with('.') || name.ends_with(' ') {
            return Some(format!(
                "`{name}` ends in a dot or space, which Windows trims"
            ));
        }
        let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
        let numbered = |prefix: &str| {
            stem.strip_prefix(prefix)
                .is_some_and(|n| n.len() == 1 && ('1'..='9').contains(&n.chars().next().unwrap()))
        };
        if RESERVED.contains(&stem.as_str()) || numbered("COM") || numbered("LPT") {
            return Some(format!("`{name}` is a device name Windows reserves"));
        }
        None
    })
}
