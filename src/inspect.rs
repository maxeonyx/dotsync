use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use jj_lib::repo::Repo as _;

use crate::config::{internal_repo_paths, DotsyncPaths};
use crate::drift::{classify_home_against_scope, RecordedFromHome};
use crate::error::{jj_error, DotsyncError};
use crate::repo::{collect_managed_tree_entries, load_scope_commit, read_tree_entry_bytes};
use crate::scope_graph::scope_depth;
use crate::session::{in_session, Run, Session};
use crate::sync::{load_sync_state, resolve_current_scope, FileDrift};

#[derive(Debug, Clone)]
pub struct ScopeInfo {
    pub name: String,
    pub parents: Vec<String>,
}

/// What `dotsync view` was asked for, and what it found.
///
/// One report rather than four entry points, because the four shapes are one
/// question — what is checked in — asked with different arguments. They are
/// also one run, which is what stops the overview from fetching once per
/// scope: it holds a session, and a session fetches once.
#[derive(Debug, Clone)]
pub enum ViewReport {
    /// Every scope, and every file any of them holds.
    Overview {
        scopes: Vec<ScopeInfo>,
        files: Vec<PathBuf>,
    },
    /// Every file one scope holds.
    Scope { scope: String, files: Vec<PathBuf> },
    /// Every scope that holds one file.
    FileScopes { file: PathBuf, scopes: Vec<String> },
    /// One file's contents on one scope.
    FileContents {
        scope: String,
        file: PathBuf,
        contents: Vec<u8>,
    },
}

#[derive(Debug, Clone)]
pub struct DiffReport {
    pub machine_scope: String,
    pub drifts: Vec<FileDrift>,
}

pub async fn view(
    paths: &DotsyncPaths,
    scope: Option<&str>,
    file: Option<&Path>,
) -> Run<Result<ViewReport, DotsyncError>> {
    in_session(paths, async |session| {
        session.fetch().await?;

        Ok(match (scope, file) {
            (Some(scope), Some(file)) => ViewReport::FileContents {
                scope: scope.to_string(),
                file: file.to_path_buf(),
                contents: scope_file_contents(session, scope, file).await?,
            },
            (Some(scope), None) => ViewReport::Scope {
                scope: scope.to_string(),
                files: scope_files(session, scope)?,
            },
            (None, Some(file)) => {
                let mut scopes = Vec::new();
                for scope in scope_list(session)? {
                    if scope_files(session, &scope.name)?
                        .iter()
                        .any(|path| path == file)
                    {
                        scopes.push(scope.name);
                    }
                }
                ViewReport::FileScopes {
                    file: file.to_path_buf(),
                    scopes,
                }
            }
            (None, None) => {
                let scopes = scope_list(session)?;
                let mut files = BTreeSet::new();
                for scope in &scopes {
                    files.extend(scope_files(session, &scope.name)?);
                }
                ViewReport::Overview {
                    scopes,
                    files: files.into_iter().collect(),
                }
            }
        })
    })
    .await
}

/// The scope graph, root scopes first and alphabetical within a depth, which
/// is the order the DAG reads in.
fn scope_list(session: &Session) -> Result<Vec<ScopeInfo>, DotsyncError> {
    let graph = &session.config().graph;
    let mut memo = HashMap::new();
    let mut scopes = graph
        .parents
        .iter()
        .map(|(name, parents)| {
            Ok((
                scope_depth(graph, name, &mut memo)?,
                ScopeInfo {
                    name: name.clone(),
                    parents: parents.clone(),
                },
            ))
        })
        .collect::<Result<Vec<_>, DotsyncError>>()?;
    scopes.sort_by(|(left_depth, left), (right_depth, right)| {
        left_depth
            .cmp(right_depth)
            .then_with(|| left.name.cmp(&right.name))
    });

    Ok(scopes.into_iter().map(|(_, scope)| scope).collect())
}

fn scope_files(session: &Session, scope: &str) -> Result<Vec<PathBuf>, DotsyncError> {
    let commit = load_scope_commit(session.repo().as_ref(), scope)?;
    let entries =
        collect_managed_tree_entries(&commit.tree(), &internal_repo_paths(session.config()))?;
    Ok(entries.into_keys().collect())
}

async fn scope_file_contents(
    session: &Session,
    scope: &str,
    relative: &Path,
) -> Result<Vec<u8>, DotsyncError> {
    let commit = load_scope_commit(session.repo().as_ref(), scope)?;
    let relative_str = relative.to_str().ok_or_else(|| DotsyncError::NonUtf8Path {
        path: relative.to_path_buf(),
    })?;
    let repo_path = jj_lib::repo_path::RepoPath::from_internal_string(relative_str)
        .map_err(|err| jj_error(format!("invalid repo path {}: {err}", relative.display())))?;
    let value = commit
        .tree()
        .path_value(repo_path)
        .map_err(|err| jj_error(format!("read {} from {scope}: {err}", relative.display())))?;
    let value = value
        .into_resolved()
        .map_err(|conflict| {
            jj_error(format!(
                "{} is conflicted on {scope}: {conflict:?}",
                relative.display()
            ))
        })?
        .ok_or_else(|| {
            jj_error(format!(
                "{} does not exist on scope {scope}",
                relative.display()
            ))
        })?;
    read_tree_entry_bytes(session.repo().store(), relative, &value).await
}

pub async fn diff_home(paths: &DotsyncPaths) -> Run<Result<DiffReport, DotsyncError>> {
    in_session(paths, async |session| {
        session.fetch().await?;
        let sync_state = load_sync_state(session.paths(), session.config())?;
        let machine_scope = resolve_current_scope(session.config(), sync_state.as_ref(), None)?;
        let classification = classify_home_against_scope(
            session,
            sync_state.as_ref(),
            &machine_scope,
            &BTreeSet::new(),
            &RecordedFromHome::default(),
        )
        .await?;

        // Exactly what the sync gate would stop on, and exactly what `status`
        // counts as a change. A remote advance this machine has not applied yet is
        // not drift, so `diff` no longer reports one — nor exits non-zero for it.
        let drifts = classification
            .paths
            .iter()
            .filter(|(_, path)| path.state.is_drift())
            .map(|(relative, path)| FileDrift {
                repo_path: relative.clone(),
                system_path: session.paths().home_dir.join(relative),
                state: path.state,
                repo_bytes: path.tip_bytes.clone(),
                home_bytes: path.home_bytes.clone(),
            })
            .collect();

        Ok(DiffReport {
            machine_scope,
            drifts,
        })
    })
    .await
}
