use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use jj_lib::repo::Repo as _;

use crate::config::DotsyncPaths;
use crate::drift::{changed_paths, FileState};
use crate::error::{jj_error, DotsyncError};
use crate::home::Home;
use crate::repo::{collect_managed_tree_entries, load_scope_commit, read_tree_entry_bytes};
use crate::scope_graph::scope_depth;
use crate::session::{in_session, Run, Session};
use crate::sync::{classify_home_against_head, file_drift, finishing, FileDrift};

#[derive(Debug, Clone)]
pub struct ScopeInfo {
    pub name: String,
    pub parents: Vec<String>,
}

/// What `view` found, and the one thing it has to say whatever it was asked.
#[derive(Debug, Clone)]
pub struct ViewReport {
    /// See `StatusReport::paused_cascade`. True of the machine rather than of
    /// the question, so every shape below carries it — `view` is the command
    /// an agent reaches for to get its bearings, and "this machine cannot
    /// commit anything" is the most important bearing there is.
    pub paused_cascade: Option<String>,
    pub found: ViewAnswer,
}

/// The answer to whichever question `view` was asked.
///
/// One report rather than four entry points, because the four shapes are one
/// question — what is checked in — asked with different arguments. They are
/// also one run, which is what stops the overview from fetching once per
/// scope: it holds a session, and a session fetches once.
#[derive(Debug, Clone)]
pub enum ViewAnswer {
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
    /// See `StatusReport::paused_cascade`: `diff` answers the same question in
    /// more detail, so it owes the same warning.
    pub paused_cascade: Option<String>,
    pub drifts: Vec<FileDrift>,
}

pub async fn view(
    paths: &DotsyncPaths,
    scope: Option<&str>,
    file: Option<&Path>,
) -> Run<Result<ViewReport, DotsyncError>> {
    in_session(paths, async |session, _paths| {
        session.fetch().await?;
        // Asked here rather than left to whatever fails first, because "that
        // scope does not exist" is the same mistake `commit` already explains
        // in full — and the answer a lookup failure gave instead was about
        // jj's objects.
        if let Some(scope) = scope {
            if !session.config().graph.parents.contains_key(scope) {
                return Err(DotsyncError::InvalidScope {
                    scope: scope.to_string(),
                });
            }
        }

        let found = match (scope, file) {
            (Some(scope), Some(file)) => ViewAnswer::FileContents {
                scope: scope.to_string(),
                file: file.to_path_buf(),
                contents: scope_file_contents(session, scope, file).await?,
            },
            (Some(scope), None) => ViewAnswer::Scope {
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
                ViewAnswer::FileScopes {
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
                ViewAnswer::Overview {
                    scopes,
                    files: files.into_iter().collect(),
                }
            }
        };

        Ok(ViewReport {
            paused_cascade: crate::pause::paused_cascade_scope(session.paths())?,
            found,
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
    let entries = collect_managed_tree_entries(&commit.tree())?;
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
        .ok_or_else(|| DotsyncError::FileNotOnScope {
            scope: scope.to_string(),
            path: relative.to_path_buf(),
        })?;
    read_tree_entry_bytes(session.repo().store(), relative, &value).await
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
    let machine_scope = home.machine_scope().to_string();
    let head = load_scope_commit(session.repo().as_ref(), &machine_scope)?;

    // The same changes `status` reports, with the two sides shown. A remote
    // advance this machine has not applied yet is not one of them, so `diff`
    // neither reports it nor exits non-zero for it.
    let classified = classify_home_against_head(session, home, &head).await?;
    let mut drifts = Vec::new();
    for (relative, classified) in changed_paths(&classified, FileState::is_drift) {
        drifts.push(file_drift(session, &relative, &classified).await?);
    }

    Ok(DiffReport {
        machine_scope,
        paused_cascade: crate::pause::paused_cascade_scope(session.paths())?,
        drifts,
    })
}
