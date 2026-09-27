use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jj_lib::repo::Repo as _;

use crate::drift::{changed_paths, FileState};
use crate::error::DotsyncError;
use crate::fleet::{EntryKind, FileRow, Fleet, ScopeInfo, Standing};
use crate::home::Home;
use crate::paths::DotsyncPaths;
use crate::place::normalized;
use crate::repo::read_tree_entry_bytes;
use crate::session::{in_session, Run, Session};
use crate::status::MachineState;
use crate::sync::{classify_home_against_machine_scope, file_drift, finishing, FileDrift};

/// The graph, for `dotsync scopes`.
#[derive(Debug, Clone)]
pub struct ScopesReport {
    /// True of the machine rather than of the question, so every read carries
    /// it — the reads are what an agent reaches for to get its bearings, and
    /// "this machine cannot commit anything" is the most important bearing
    /// there is.
    pub machine: MachineState,
    pub scopes: Vec<ScopeInfo>,
}

/// Rows of the fleet table, for `dotsync files`.
#[derive(Debug, Clone)]
pub struct FilesReport {
    pub machine: MachineState,
    pub rows: Vec<FileRow>,
}

/// One file on one scope, for `dotsync show`.
#[derive(Debug, Clone)]
pub struct ShowReport {
    pub machine: MachineState,
    pub row: FileRow,
    /// A symlink's content is its target.
    pub contents: Vec<u8>,
}

/// Two versions of every path that differs, for `dotsync diff <scope>...`.
#[derive(Debug, Clone)]
pub struct CompareReport {
    pub machine: MachineState,
    /// What is on the left: a scope, or what a scope inherits.
    pub left: String,
    pub right: String,
    pub changes: Vec<ScopeDifference>,
}

#[derive(Debug, Clone)]
pub struct ScopeDifference {
    pub path: PathBuf,
    /// `None` where that side does not hold the path.
    pub left: Option<Vec<u8>>,
    pub right: Option<Vec<u8>>,
    pub left_kind: Option<EntryKind>,
    pub right_kind: Option<EntryKind>,
}

#[derive(Debug, Clone)]
pub struct DiffReport {
    /// `diff` answers `status`'s question in more detail, so it owes the same
    /// qualifications.
    pub machine: MachineState,
    pub drifts: Vec<FileDrift>,
}

/// Every read opens the same way: fetch, check every scope it names, predict
/// the fleet. A read works on any repo state this machine can be in —
/// including before it has joined, which is when choosing where to join needs
/// it most.
async fn read_fleet(session: &mut Session, named: &[&str]) -> Result<Fleet, DotsyncError> {
    session.fetch().await?;
    for scope in named {
        if !session.graph().contains(scope) {
            return Err(DotsyncError::InvalidScope {
                scope: scope.to_string(),
            });
        }
    }
    Fleet::predicted(session).await
}

pub async fn scopes(paths: &DotsyncPaths) -> Run<Result<ScopesReport, DotsyncError>> {
    in_session(paths, async |session, _paths| {
        let fleet = read_fleet(session, &[]).await?;
        Ok(ScopesReport {
            machine: MachineState::read(session).await?,
            scopes: fleet.scopes().to_vec(),
        })
    })
    .await
}

/// What `dotsync files` was asked.
#[derive(Debug, Clone, Default)]
pub struct FilesQuery {
    /// Only these scopes; every scope when empty.
    pub scopes: Vec<String>,
    /// Only what each scope adds, overrides or removes.
    pub own: bool,
    /// Only these paths and whatever is under them; every path when empty.
    pub paths: Vec<PathBuf>,
}

pub async fn files(
    paths: &DotsyncPaths,
    query: FilesQuery,
) -> Run<Result<FilesReport, DotsyncError>> {
    in_session(paths, async |session, _paths| {
        let named: Vec<&str> = query.scopes.iter().map(String::as_str).collect();
        let fleet = read_fleet(session, &named).await?;
        // Scope by scope in the order the graph reads, then path by path.
        let mut rows: Vec<FileRow> = Vec::new();
        for scope in fleet.scopes() {
            if !query.scopes.is_empty() && !query.scopes.contains(&scope.name) {
                continue;
            }
            rows.extend(
                fleet
                    .rows()
                    .iter()
                    .filter(|row| row.scope == scope.name)
                    .filter(|row| !query.own || row.standing.is_own())
                    // A removal is a scope's own decision and nothing it
                    // holds, so it belongs to the question "what is this
                    // scope's own", not to "what does this scope hold".
                    .filter(|row| query.own || row.standing != Standing::Removed)
                    .filter(|row| {
                        query.paths.is_empty()
                            || query.paths.iter().any(|prefix| under(&row.path, prefix))
                    })
                    .cloned(),
            );
        }
        Ok(FilesReport {
            machine: MachineState::read(session).await?,
            rows,
        })
    })
    .await
}

pub async fn show(
    paths: &DotsyncPaths,
    scope: &str,
    path: &Path,
) -> Run<Result<ShowReport, DotsyncError>> {
    in_session(paths, async |session, _paths| {
        let fleet = read_fleet(session, &[scope]).await?;
        let path = normalized(path);
        let row = fleet
            .row(scope, &path)
            .filter(|row| row.standing != Standing::Removed)
            .cloned()
            .ok_or_else(|| DotsyncError::FileNotOnScope {
                scope: scope.to_string(),
                path: path.clone(),
            })?;
        let (_, entries) = fleet.holds(scope).expect("a scope with a row has a tree");
        let contents = match entries.get(&path) {
            Some(Some(value)) => {
                read_tree_entry_bytes(session.repo().store(), &path, value).await?
            }
            // Two heads that disagree have no one version to print. Every
            // version of a conflict is what the pause presents.
            _ => {
                return Err(DotsyncError::ConflictedOnScope {
                    scope: scope.to_string(),
                    path,
                })
            }
        };
        Ok(ShowReport {
            machine: MachineState::read(session).await?,
            row,
            contents,
        })
    })
    .await
}

/// `dotsync diff <scope>` compares what a scope inherits with what it holds;
/// `dotsync diff <left> <right>` compares two scopes.
pub async fn compare(
    paths: &DotsyncPaths,
    left: &str,
    right: Option<&str>,
    only: &[PathBuf],
) -> Run<Result<CompareReport, DotsyncError>> {
    in_session(paths, async |session, _paths| {
        let named: Vec<&str> = [Some(left), right].into_iter().flatten().collect();
        let fleet = read_fleet(session, &named).await?;
        let empty = BTreeMap::new();
        let (left_label, left_entries, right_label, right_entries) = match right {
            None => (
                format!("what {left} inherits"),
                fleet
                    .inherits(left)
                    .map(|(_, entries)| entries)
                    .unwrap_or(&empty),
                left.to_string(),
                fleet
                    .holds(left)
                    .map(|(_, entries)| entries)
                    .unwrap_or(&empty),
            ),
            Some(right) => (
                left.to_string(),
                fleet
                    .holds(left)
                    .map(|(_, entries)| entries)
                    .unwrap_or(&empty),
                right.to_string(),
                fleet
                    .holds(right)
                    .map(|(_, entries)| entries)
                    .unwrap_or(&empty),
            ),
        };

        let store = session.repo().store().clone();
        let mut changes = Vec::new();
        let paths: std::collections::BTreeSet<&PathBuf> =
            left_entries.keys().chain(right_entries.keys()).collect();
        for path in paths {
            if !only.is_empty() && !only.iter().any(|prefix| under(path, &normalized(prefix))) {
                continue;
            }
            let (l, r) = (left_entries.get(path), right_entries.get(path));
            if l == r {
                continue;
            }
            let bytes = async |value: Option<&Option<jj_lib::backend::TreeValue>>| match value {
                Some(Some(value)) => read_tree_entry_bytes(&store, path, value).await.map(Some),
                _ => Ok(None),
            };
            changes.push(ScopeDifference {
                path: path.to_path_buf(),
                left_kind: l.map(|value| EntryKind::of(value.as_ref())),
                right_kind: r.map(|value| EntryKind::of(value.as_ref())),
                left: bytes(l).await?,
                right: bytes(r).await?,
            });
        }

        Ok(CompareReport {
            machine: MachineState::read(session).await?,
            left: left_label,
            right: right_label,
            changes,
        })
    })
    .await
}

/// Whether `path` is `prefix` or lies under it.
fn under(path: &Path, prefix: &Path) -> bool {
    let prefix = normalized(prefix);
    prefix.as_os_str().is_empty() || prefix == Path::new(".") || path.starts_with(&prefix)
}

pub async fn diff_home(paths: &DotsyncPaths) -> Run<Result<DiffReport, DotsyncError>> {
    in_session(paths, async |session, paths| {
        let mut home = Home::acquire(session, paths).await?;
        let outcome = diff_report(session, &mut home).await;
        finishing(home, session, outcome).await
    })
    .await
}

async fn diff_report(session: &mut Session, home: &mut Home) -> Result<DiffReport, DotsyncError> {
    session.fetch().await?;

    // The same changes `status` reports, with the two sides shown. A remote
    // advance this machine has not applied yet is not one of them, so `diff`
    // neither reports it nor exits non-zero for it.
    let classified = classify_home_against_machine_scope(session, home).await?;
    let mut drifts = Vec::new();
    for (relative, classified) in changed_paths(&classified, FileState::is_drift) {
        drifts.push(file_drift(session, &relative, &classified).await?);
    }

    Ok(DiffReport {
        machine: MachineState::read(session).await?,
        drifts,
    })
}
