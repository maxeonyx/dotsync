use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use jj_lib::merge::Merge;
use jj_lib::repo::Repo as _;

use crate::drift::{changed_paths, FileState};
use crate::error::{jj_error, DotsyncError};
use crate::home::Home;
use crate::paths::DotsyncPaths;
use crate::repo::{collect_managed_tree_entries, read_tree_entry_bytes, scope_head_tree};
use crate::session::{in_session, Run, Session};
use crate::status::MachineState;
use crate::sync::{classify_home_against_machine_scope, file_drift, finishing, FileDrift};

#[derive(Debug, Clone)]
pub struct ScopeInfo {
    pub name: String,
    pub parents: Vec<String>,
    /// What the scope is for, in the words of whoever created it. Only there
    /// when they said.
    pub description: Option<String>,
}

/// What `view` found, and what it has to say whatever it was asked.
#[derive(Debug, Clone)]
pub struct ViewReport {
    /// True of the machine rather than of the question, so every shape below
    /// carries it — `view` is the command an agent reaches for to get its
    /// bearings, and "this machine cannot commit anything" is the most
    /// important bearing there is.
    pub machine: MachineState,
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
    /// `diff` answers `status`'s question in more detail, so it owes the same
    /// qualifications.
    pub machine: MachineState,
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
            if !session.graph().contains(scope) {
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
                files: scope_files(session, scope).await?,
            },
            (None, Some(file)) => {
                let mut scopes = Vec::new();
                for scope in scope_list(session) {
                    if scope_files(session, &scope.name)
                        .await?
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
                let scopes = scope_list(session);
                let mut files = BTreeSet::new();
                for scope in &scopes {
                    files.extend(scope_files(session, &scope.name).await?);
                }
                ViewAnswer::Overview {
                    scopes,
                    files: files.into_iter().collect(),
                }
            }
        };

        Ok(ViewReport {
            machine: MachineState::read(session)?,
            found,
        })
    })
    .await
}

/// The scope graph, root scopes first and alphabetical within a depth, which
/// is the order the DAG reads in.
fn scope_list(session: &Session) -> Vec<ScopeInfo> {
    let graph = session.graph();
    let mut scopes: Vec<(usize, ScopeInfo)> = graph
        .scopes()
        .map(|scope| {
            (
                graph.depth(&scope.name),
                ScopeInfo {
                    name: scope.name.clone(),
                    parents: scope.parents.clone(),
                    description: scope.description.clone(),
                },
            )
        })
        .collect();
    scopes.sort_by(|(left_depth, left), (right_depth, right)| {
        left_depth
            .cmp(right_depth)
            .then_with(|| left.name.cmp(&right.name))
    });

    scopes.into_iter().map(|(_, scope)| scope).collect()
}

/// The files one scope holds. A scope the graph names and the repo has no head
/// for holds none — `view` describes the state it is in rather than refusing
/// to describe it, which is the whole of what it is for.
async fn scope_files(session: &Session, scope: &str) -> Result<Vec<PathBuf>, DotsyncError> {
    let Some(tree) = scope_head_tree(session.repo().as_ref(), scope).await? else {
        return Ok(Vec::new());
    };
    let entries = collect_managed_tree_entries(&tree)?;
    Ok(entries.into_keys().collect())
}

async fn scope_file_contents(
    session: &Session,
    scope: &str,
    relative: &Path,
) -> Result<Vec<u8>, DotsyncError> {
    let relative_str = relative.to_str().ok_or_else(|| DotsyncError::NonUtf8Path {
        path: relative.to_path_buf(),
    })?;
    let repo_path = jj_lib::repo_path::RepoPath::from_internal_string(relative_str)
        .map_err(|err| jj_error(format!("invalid repo path {}: {err}", relative.display())))?;
    let value = match scope_head_tree(session.repo().as_ref(), scope).await? {
        Some(tree) => tree.path_value(repo_path),
        // A scope with no head holds no files, so the answer is the same one a
        // scope that simply does not hold this file gives.
        None => Ok(Merge::absent()),
    }
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

    // The same changes `status` reports, with the two sides shown. A remote
    // advance this machine has not applied yet is not one of them, so `diff`
    // neither reports it nor exits non-zero for it.
    let classified = classify_home_against_machine_scope(session, home).await?;
    let mut drifts = Vec::new();
    for (relative, classified) in changed_paths(&classified, FileState::is_drift) {
        drifts.push(file_drift(session, &relative, &classified).await?);
    }

    Ok(DiffReport {
        machine_scope,
        machine: MachineState::read(session)?,
        drifts,
    })
}
