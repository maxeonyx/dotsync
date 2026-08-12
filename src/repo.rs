use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gix::remote::fetch::Tags;
use jj_lib::backend::{CommitId, TreeValue};
use jj_lib::config::StackedConfig;
use jj_lib::git::{
    self, GitBranchPushTargets, GitFetch, GitFetchRefExpression, GitImportOptions, GitProgress,
    GitPushOptions, GitSidebandLineTerminator, GitSubprocessCallback, GitSubprocessOptions,
};
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::refs::BookmarkPushUpdate;
use jj_lib::repo::{MutableRepo, ReadonlyRepo, Repo as _, RepoLoader, StoreFactories};
use jj_lib::settings::UserSettings;
use jj_lib::str_util::StringExpression;

use crate::config::DotsyncPaths;
use crate::error::{jj_error, DotsyncError};
use crate::session::Session;

pub(crate) fn default_settings() -> Result<UserSettings, DotsyncError> {
    let config = StackedConfig::with_defaults();
    UserSettings::from_config(config).map_err(|err| jj_error(format!("load jj settings: {err}")))
}

pub(crate) async fn load_repo_direct(
    paths: &DotsyncPaths,
) -> Result<Arc<ReadonlyRepo>, DotsyncError> {
    let jj_repo_dir = paths.repo_root.join(".jj/repo");
    if !jj_repo_dir.exists() {
        return Err(DotsyncError::NotInitialized {
            path: paths.repo_root.clone(),
        });
    }

    let settings = default_settings()?;
    let loader =
        RepoLoader::init_from_file_system(&settings, &jj_repo_dir, &StoreFactories::default())
            .map_err(|err| jj_error(format!("load repo loader from file system: {err}")))?;
    loader
        .load_at_head()
        .await
        .map_err(|err| jj_error(format!("load repo at head: {err}")))
}

pub(crate) async fn add_origin_remote(
    repo: Arc<ReadonlyRepo>,
    remote_url: &str,
) -> Result<Arc<ReadonlyRepo>, DotsyncError> {
    let mut tx = repo.start_transaction();
    git::add_remote(
        tx.repo_mut(),
        "origin".as_ref(),
        remote_url,
        None,
        Tags::None,
        &StringExpression::all(),
    )
    .map_err(|err| jj_error(format!("add origin remote: {err}")))?;
    tx.commit("dotsync: add origin remote")
        .await
        .map_err(|err| jj_error(format!("commit remote setup: {err}")))
}

pub(crate) async fn fetch_origin(
    repo: Arc<ReadonlyRepo>,
) -> Result<Arc<ReadonlyRepo>, DotsyncError> {
    let settings = default_settings()?;
    let subprocess_options = GitSubprocessOptions::from_settings(&settings)
        .map_err(|err| jj_error(format!("load git subprocess settings: {err}")))?;
    let import_options = default_import_options();
    let mut tx = repo.start_transaction();
    let mut fetch = GitFetch::new(tx.repo_mut(), subprocess_options, &import_options)
        .map_err(|err| jj_error(format!("prepare fetch: {err}")))?;
    let refspecs = git::expand_fetch_refspecs(
        "origin".as_ref(),
        GitFetchRefExpression {
            bookmark: StringExpression::all(),
            tag: StringExpression::none(),
        },
    )
    .map_err(|err| jj_error(format!("expand fetch refspecs: {err}")))?;
    fetch
        .fetch(
            "origin".as_ref(),
            refspecs,
            &mut QuietGitCallback,
            None,
            None,
        )
        .map_err(|err| match err {
            git::GitFetchError::Subprocess(_) => DotsyncError::RemoteUnreachable {
                reason: remote_failure_reason(&err),
            },
            other => jj_error(format!("fetch origin: {other}")),
        })?;
    fetch
        .import_refs()
        .map_err(|err| jj_error(format!("import fetched refs: {err}")))?;
    sync_local_bookmarks_from_remote(tx.repo_mut(), "origin".as_ref())?;
    tx.commit("dotsync: fetch origin")
        .await
        .map_err(|err| jj_error(format!("commit fetch operation: {err}")))
}

/// What git said when dotsync could not talk to the remote.
///
/// jj wraps every failure of its `git` subprocess in one variant whose inner
/// type it does not export, so dotsync cannot tell "host did not resolve" from
/// "permission denied" from "your git is too old for this option". It does not
/// need to: all of them mean this run did not reach the remote, and all of
/// them are handled the same way. Which one it was is git's own words, quoted
/// back in the notice so the reader can tell them apart.
fn remote_failure_reason(error: &dyn std::fmt::Display) -> String {
    error
        .to_string()
        .trim_start_matches("External git program failed:")
        .trim()
        .to_string()
}

/// Reconciles every local scope bookmark against the remote bookmark it
/// tracks. DESIGN.md "The convergence model" describes four cases; this loop
/// decides six things per scope, because two of them are about the local
/// bookmark existing at all, and divergence is detected in two different
/// places:
///
/// - no local bookmark: a scope another machine published — create it
/// - conflicted local bookmark: jj's import already tried to reconcile this
///   scope and could not, which *is* divergence — error (never reset it, or
///   the local commits are orphaned and the home files that came with them are
///   deleted by the following sync)
/// - local == remote: nothing to do
/// - local behind remote: fast-forward the local bookmark
/// - local ahead of remote: unpushed local work — keep it, the caller publishes
///   it when it pushes
/// - neither is an ancestor of the other: divergence that the import did not
///   turn into a conflicted bookmark — for example when the remote bookmark is
///   not tracked, so the import left the local one alone — error
///
/// Erroring leaves the whole fetch transaction uncommitted, so a diverged
/// repo is unchanged by the attempt and reports the same thing next run.
pub(crate) fn sync_local_bookmarks_from_remote(
    mut_repo: &mut MutableRepo,
    remote_name: &jj_lib::ref_name::RemoteName,
) -> Result<(), DotsyncError> {
    let updates: Vec<(RefNameBuf, CommitId)> = mut_repo
        .view()
        .remote_bookmarks(remote_name)
        .filter_map(|(name, remote_ref)| {
            remote_ref
                .target
                .as_normal()
                .map(|id| (RefNameBuf::from(name.as_str()), id.clone()))
        })
        .collect();

    for (name, remote_id) in updates {
        let local_target = mut_repo.view().get_local_bookmark(name.as_ref()).clone();
        if local_target.is_absent() {
            // A scope this machine does not have yet, published by another
            // machine.
            mut_repo.set_local_bookmark_target(name.as_ref(), RefTarget::normal(remote_id));
            continue;
        }
        let Some(local_id) = local_target.as_normal().cloned() else {
            // A conflicted bookmark is jj's own record that the fetched remote
            // position could not be reconciled with the local one. Its sides
            // are the local and the remote head; report only the local one.
            return Err(DotsyncError::ScopeDiverged {
                scope: name.as_str().to_string(),
                local_target: local_target
                    .added_ids()
                    .filter(|id| **id != remote_id)
                    .map(|id| id.hex())
                    .collect::<Vec<_>>()
                    .join(", "),
                remote_target: remote_id.hex(),
            });
        };
        if local_id == remote_id {
            continue;
        }

        let ancestry = |from: &CommitId, to: &CommitId| {
            mut_repo.index().is_ancestor(from, to).map_err(|err| {
                jj_error(format!(
                    "check bookmark ancestry for {}: {err}",
                    name.as_str()
                ))
            })
        };
        if ancestry(&local_id, &remote_id)? {
            mut_repo.set_local_bookmark_target(name.as_ref(), RefTarget::normal(remote_id));
            continue;
        }
        if ancestry(&remote_id, &local_id)? {
            continue;
        }

        return Err(DotsyncError::ScopeDiverged {
            scope: name.as_str().to_string(),
            local_target: local_id.hex(),
            remote_target: remote_id.hex(),
        });
    }

    Ok(())
}

/// What a run did about publishing local scope commits. Any scope named by
/// this report is committed on this machine and absent from the remote.
///
/// A refused push is not a dead end — the scope stays local-ahead, which is an
/// ordinary state — but the run must say so, or the user is left believing a
/// change reached the remote when it did not. There is deliberately no
/// `Default`: a command that pushes has to say what happened.
#[derive(Debug, Clone)]
pub enum PushReport {
    /// Nothing is waiting to be published: the push succeeded, or there was
    /// nothing to push.
    UpToDate,
    /// The remote refused these scopes.
    Refused {
        scopes: Vec<String>,
        rejection_reason: Option<String>,
    },
    /// Dotsync did not offer these scopes to the remote, because publishing a
    /// half-cascaded scope would put history on the remote that `dotsync abort`
    /// could no longer take back.
    WithheldPausedCascade {
        scopes: Vec<String>,
        paused_scope: String,
    },
    /// The remote could not be reached, so these scopes stay local-ahead until
    /// a run that can reach it publishes them. That is the same state a
    /// refused push leaves them in, and an ordinary input to the next
    /// convergence — not a failure of this run.
    Unreachable { scopes: Vec<String>, reason: String },
}

impl PushReport {
    pub fn unpushed_scopes(&self) -> &[String] {
        match self {
            PushReport::UpToDate => &[],
            PushReport::Refused { scopes, .. } => scopes,
            PushReport::WithheldPausedCascade { scopes, .. } => scopes,
            PushReport::Unreachable { scopes, .. } => scopes,
        }
    }
}

/// Scopes whose local bookmark is not where the remote has it.
fn pending_bookmark_updates(repo: &ReadonlyRepo) -> Vec<(RefNameBuf, BookmarkPushUpdate)> {
    repo.view()
        .local_remote_bookmarks("origin".as_ref())
        .filter_map(|(name, targets)| {
            let local = targets.local_target.as_normal()?.clone();
            let remote = targets.remote_ref.target.as_normal().cloned();
            if remote.as_ref() == Some(&local) {
                return None;
            }
            Some((
                RefNameBuf::from(name.as_str()),
                BookmarkPushUpdate {
                    old_target: remote,
                    new_target: Some(local),
                },
            ))
        })
        .collect()
}

/// The scopes a push would offer the remote right now.
pub(crate) fn pending_push_scopes(session: &Session) -> Vec<String> {
    pending_bookmark_updates(session.repo())
        .into_iter()
        .map(|(name, _)| name.as_str().to_string())
        .collect()
}

pub(crate) async fn push_scope_updates(session: &mut Session) -> Result<PushReport, DotsyncError> {
    let repo = session.repo().clone();
    let settings = default_settings()?;
    let subprocess_options = GitSubprocessOptions::from_settings(&settings)
        .map_err(|err| jj_error(format!("load git subprocess settings: {err}")))?;

    let updates = pending_bookmark_updates(&repo);

    if updates.is_empty() {
        return Ok(PushReport::UpToDate);
    }

    let attempted: Vec<String> = updates
        .iter()
        .map(|(name, _)| name.as_str().to_string())
        .collect();
    let mut tx = repo.start_transaction();
    let stats = match git::push_branches(
        tx.repo_mut(),
        subprocess_options,
        "origin".as_ref(),
        &GitBranchPushTargets {
            branch_updates: updates,
        },
        &mut QuietGitCallback,
        &GitPushOptions::default(),
    ) {
        Ok(stats) => stats,
        Err(err @ git::GitPushError::Subprocess(_)) => {
            return Ok(PushReport::Unreachable {
                scopes: attempted,
                reason: remote_failure_reason(&err),
            })
        }
        Err(other) => return Err(jj_error(format!("push branches: {other}"))),
    };
    session
        .advance_to(
            tx.commit("dotsync: push scope updates")
                .await
                .map_err(|err| jj_error(format!("commit push operation: {err}")))?,
        )
        .await?;

    let pushed: Vec<&str> = stats
        .pushed
        .iter()
        .map(|reference| reference.as_str().trim_start_matches("refs/heads/"))
        .collect();
    let refused: Vec<String> = attempted
        .into_iter()
        .filter(|scope| !pushed.contains(&scope.as_str()))
        .collect();
    if refused.is_empty() {
        return Ok(PushReport::UpToDate);
    }
    Ok(PushReport::Refused {
        scopes: refused,
        rejection_reason: stats
            .rejected
            .iter()
            .chain(stats.remote_rejected.iter())
            .find_map(|(_, reason)| reason.clone()),
    })
}

pub(crate) fn load_scope_commit(
    repo: &dyn jj_lib::repo::Repo,
    scope: &str,
) -> Result<jj_lib::commit::Commit, DotsyncError> {
    let commit_id = repo
        .view()
        .get_local_bookmark(RefNameBuf::from(scope).as_ref())
        .as_normal()
        .cloned()
        .ok_or_else(|| DotsyncError::ScopeNotInRepo {
            scope: scope.to_string(),
        })?;
    repo.store()
        .get_commit(&commit_id)
        .map_err(|err| jj_error(format!("load scope commit for {scope}: {err}")))
}

pub(crate) fn collect_managed_tree_entries(
    tree: &jj_lib::merged_tree::MergedTree,
    excluded_paths: &std::collections::BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, TreeValue>, DotsyncError> {
    let mut entries = BTreeMap::new();
    for (path, value) in tree.entries() {
        let display_path = PathBuf::from(path.as_internal_file_string());
        if excluded_paths.contains(&display_path) {
            continue;
        }
        let value = value.map_err(|err| {
            jj_error(format!("read tree entry {}: {err}", display_path.display()))
        })?;
        let Some(value) = value.as_resolved() else {
            return Err(jj_error(format!(
                "tree entry {} is conflicted during sync",
                display_path.display()
            )));
        };
        let Some(value) = value.clone() else {
            continue;
        };
        match value {
            TreeValue::Tree(_) => {}
            other => {
                entries.insert(display_path, other);
            }
        }
    }
    Ok(entries)
}

pub(crate) async fn read_tree_entry_bytes(
    store: &Arc<jj_lib::store::Store>,
    relative: &Path,
    value: &TreeValue,
) -> Result<Vec<u8>, DotsyncError> {
    let relative_str = relative.to_str().ok_or_else(|| DotsyncError::NonUtf8Path {
        path: relative.to_path_buf(),
    })?;
    let repo_path = jj_lib::repo_path::RepoPath::from_internal_string(relative_str)
        .map_err(|err| jj_error(format!("invalid repo path {}: {err}", relative.display())))?;
    match value {
        TreeValue::File { id, .. } => {
            let mut reader = store
                .read_file(repo_path, id)
                .await
                .map_err(|err| jj_error(format!("read repo file {}: {err}", relative.display())))?;
            let mut contents = Vec::new();
            use tokio::io::AsyncReadExt;
            reader.read_to_end(&mut contents).await.map_err(|err| {
                jj_error(format!(
                    "read repo file bytes {}: {err}",
                    relative.display()
                ))
            })?;
            Ok(contents)
        }
        TreeValue::Symlink(id) => {
            let target = store.read_symlink(repo_path, id).await.map_err(|err| {
                jj_error(format!("read repo symlink {}: {err}", relative.display()))
            })?;
            Ok(target.into_bytes())
        }
        TreeValue::GitSubmodule(_) => Err(DotsyncError::GitSubmodule {
            path: relative.to_path_buf(),
        }),
        TreeValue::Tree(_) => unreachable!("tree entries are filtered out before copying"),
    }
}

pub(crate) fn default_import_options() -> GitImportOptions {
    GitImportOptions {
        auto_local_bookmark: false,
        abandon_unreachable_commits: true,
        remote_auto_track_bookmarks: HashMap::new(),
    }
}

#[derive(Debug, Default)]
pub(crate) struct QuietGitCallback;

impl GitSubprocessCallback for QuietGitCallback {
    fn needs_progress(&self) -> bool {
        false
    }

    fn progress(&mut self, _progress: &GitProgress) -> io::Result<()> {
        Ok(())
    }

    fn local_sideband(
        &mut self,
        _message: &[u8],
        _term: Option<GitSidebandLineTerminator>,
    ) -> io::Result<()> {
        Ok(())
    }

    fn remote_sideband(
        &mut self,
        _message: &[u8],
        _term: Option<GitSidebandLineTerminator>,
    ) -> io::Result<()> {
        Ok(())
    }
}
