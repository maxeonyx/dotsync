use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gix::remote::fetch::Tags;
use jj_lib::backend::TreeValue;
use jj_lib::commit::Commit;
use jj_lib::config::StackedConfig;
use jj_lib::git::{
    self, GitBranchPushTargets, GitFetch, GitFetchRefExpression, GitImportOptions, GitProgress,
    GitPushOptions, GitSidebandLineTerminator, GitSubprocessCallback, GitSubprocessOptions,
};
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::{RefNameBuf, RemoteRefSymbol};
use jj_lib::refs::BookmarkPushUpdate;
use jj_lib::repo::{ReadonlyRepo, Repo, RepoLoader, StoreFactories};
use jj_lib::rewrite::merge_commit_trees;
use jj_lib::settings::UserSettings;
use jj_lib::str_util::StringExpression;
use jj_lib::view::View;

use crate::error::{jj_error, DotsyncError};
use crate::paths::DotsyncPaths;
use crate::scope_graph::ScopeGraph;
use crate::session::Session;

/// The one remote dotsync has. Named here because "origin" is spelled at every
/// site that asks the view a question about the remote.
const ORIGIN: &str = "origin";

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

/// Where a scope's head stands on this machine.
///
/// A `RefTarget` rather than a commit id, because a head is in one of three
/// states and only one of them is a commit id (DESIGN, "A scope head has three
/// states"): absent, exactly one commit, or contested — this machine and the
/// remote each holding commits the other does not, with no single answer to
/// which is the head.
///
/// The fetch is what puts it in one of them. jj's import merges each scope the
/// remote published into the head this machine holds, using the position the
/// remote was last seen at: caught up either way resolves to one commit, and
/// only two sides that moved apart stay both. Dotsync used to redo that merge
/// by hand afterwards, from six cases and without the last-seen position, and
/// stopped the whole fetch on the two it could not name.
pub(crate) fn scope_head<'a>(repo: &'a dyn Repo, scope: &str) -> &'a RefTarget {
    repo.view()
        .get_local_bookmark(RefNameBuf::from(scope).as_ref())
}

/// The tree a scope's head holds — what a command that only reads the repo
/// answers about.
///
/// `None` when the scope has no head at all, which is a state to report rather
/// than a failure: a read-only command answers on any repo state. A contested
/// head answers with the merge of its sides, which is the tree a convergence
/// would write onto it, so reading and converging cannot disagree about what
/// the scope holds.
pub(crate) async fn scope_head_tree(
    repo: &dyn Repo,
    scope: &str,
) -> Result<Option<MergedTree>, DotsyncError> {
    let mut commits = Vec::new();
    for id in scope_head(repo, scope).added_ids() {
        commits.push(
            repo.store()
                .get_commit(id)
                .map_err(|err| jj_error(format!("load scope head for {scope}: {err}")))?,
        );
    }
    if commits.is_empty() {
        return Ok(None);
    }
    Ok(Some(merge_commit_trees(repo, &commits).await.map_err(
        |err| jj_error(format!("merge the two heads of scope {scope}: {err}")),
    )?))
}

/// The one commit a scope's head is, for the commands that write history onto
/// it.
///
/// A commit is written onto a parent rather than onto possibilities, so the two
/// states that are not a single commit are refusals here, and each says which
/// one it met: a scope with no head has nothing to build on, and a contested
/// one has to be merged before anything can be written on it.
pub(crate) fn scope_head_commit(repo: &dyn Repo, scope: &str) -> Result<Commit, DotsyncError> {
    let target = scope_head(repo, scope);
    if target.has_conflict() {
        return Err(scope_diverged(repo.view(), scope));
    }
    let commit_id = target
        .as_normal()
        .ok_or_else(|| DotsyncError::ScopeNotInRepo {
            scope: scope.to_string(),
        })?;
    repo.store()
        .get_commit(commit_id)
        .map_err(|err| jj_error(format!("load scope commit for {scope}: {err}")))
}

/// The scopes whose head is contested.
///
/// Read-only commands report this and writing commands stop on it, from one
/// reading of one state — so a `status` that says nothing about a scope cannot
/// be followed by a `dotsync` that stops on it.
pub(crate) fn diverged_scopes(repo: &dyn Repo, graph: &ScopeGraph) -> Vec<String> {
    graph
        .names()
        .filter(|scope| scope_head(repo, scope).has_conflict())
        .map(str::to_string)
        .collect()
}

/// The stop a run that writes makes when a scope's head is contested.
///
/// Named for the state rather than for the command, because every command that
/// writes meets it the same way and none of them can merge it.
pub(crate) fn scope_diverged(view: &View, scope: &str) -> DotsyncError {
    let name = RefNameBuf::from(scope);
    let hexes = |target: &RefTarget| {
        target
            .added_ids()
            .map(|id| id.hex())
            .collect::<Vec<_>>()
            .join(", ")
    };
    DotsyncError::ScopeDiverged {
        scope: scope.to_string(),
        head: hexes(view.get_local_bookmark(name.as_ref())),
        published: hexes(
            &view
                .get_remote_bookmark(RemoteRefSymbol {
                    name: name.as_ref(),
                    remote: ORIGIN.as_ref(),
                })
                .target,
        ),
    }
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
        .local_remote_bookmarks(ORIGIN.as_ref())
        .filter_map(|(name, targets)| {
            // Skipping a head that is not one commit: absent means the remote
            // holds a scope this machine does not, which is nothing to
            // publish, and every command that pushes has already stopped on a
            // contested scope of its own before reaching here.
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

pub(crate) fn collect_managed_tree_entries(
    tree: &jj_lib::merged_tree::MergedTree,
) -> Result<BTreeMap<PathBuf, TreeValue>, DotsyncError> {
    let mut entries = BTreeMap::new();
    for (path, value) in tree.entries() {
        let display_path = PathBuf::from(path.as_internal_file_string());
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

/// The content of a tree entry that may not be there. Absent is not empty —
/// a path no tree holds has no content, which is how a deletion and an
/// addition read on their respective sides.
pub(crate) async fn read_entry_bytes(
    store: &Arc<jj_lib::store::Store>,
    relative: &Path,
    value: Option<&TreeValue>,
) -> Result<Option<Vec<u8>>, DotsyncError> {
    match value {
        Some(value) => Ok(Some(read_tree_entry_bytes(store, relative, value).await?)),
        None => Ok(None),
    }
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

/// Every scope the remote publishes is one this machine follows, which is what
/// makes jj's import the whole of dotsync's reconciliation: a scope another
/// machine created arrives as a head this machine holds, and a scope both have
/// moved arrives contested rather than as two positions dotsync has to compare
/// itself.
pub(crate) fn default_import_options() -> GitImportOptions {
    GitImportOptions {
        auto_local_bookmark: true,
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
