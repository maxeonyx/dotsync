use std::path::PathBuf;

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
