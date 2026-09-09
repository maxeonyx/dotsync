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
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::refs::BookmarkPushUpdate;
use jj_lib::repo::{ReadonlyRepo, Repo, RepoLoader, StoreFactories};
use jj_lib::rewrite::merge_commit_trees;
use jj_lib::settings::UserSettings;
use jj_lib::str_util::StringExpression;

use crate::error::{jj_error, DotsyncError};
use crate::paths::DotsyncPaths;
use crate::scope_graph::ScopeGraph;
use crate::session::Session;

/// The one remote dotsync has. Named here because "origin" is spelled at every
/// site that asks the view a question about the remote.
pub(crate) const ORIGIN: &str = "origin";

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
/// Every command that writes runs the convergence pass first, and that pass
/// leaves each scope's head a single commit or stops — so by the time anything
/// asks this question a contested head is a state the run has already dealt
/// with. What is left is a scope this repo holds no head for at all, which is
/// something outside dotsync having moved the branch.
pub(crate) fn scope_head_commit(repo: &dyn Repo, scope: &str) -> Result<Commit, DotsyncError> {
    let commit_id =
        scope_head(repo, scope)
            .as_normal()
            .ok_or_else(|| DotsyncError::ScopeNotInRepo {
                scope: scope.to_string(),
            })?;
    repo.store()
        .get_commit(commit_id)
        .map_err(|err| jj_error(format!("load scope commit for {scope}: {err}")))
}

/// The scopes whose head is contested — two machines moved it and it holds
/// both positions.
///
/// Only the commands that report ever see one. A run that writes converges
/// first, which is what turns a contested head back into a single commit, so
/// this describes a repo that has been told two things and has not been asked
/// to reconcile them yet.
pub(crate) fn diverged_scopes(repo: &dyn Repo, graph: &ScopeGraph) -> Vec<String> {
    graph
        .names()
        .filter(|scope| scope_head(repo, scope).has_conflict())
        .map(str::to_string)
        .collect()
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
        rejection: Rejection,
    },
    /// The remote could not be reached, so these scopes stay local-ahead until
    /// a run that can reach it publishes them. That is the same state a
    /// refused push leaves them in, and an ordinary input to the next
    /// convergence — not a failure of this run.
    Unreachable { scopes: Vec<String>, reason: String },
}

/// Why the remote said no — which decides whether asking again can change the
/// answer.
///
/// Dotsync offers each scope with the position it last saw the remote at, so a
/// remote that has moved since fails that lease. That is another machine
/// having pushed first, and converging onto what it published is the answer;
/// the run tries again by itself. A remote that refuses the write for any
/// other reason — a hook, a permission — will refuse it again for as long as
/// whatever refused it is in place, so the run says so and stops.
#[derive(Debug, Clone)]
pub enum Rejection {
    /// The remote moved after this run last looked at it.
    RemoteMoved,
    /// The remote refused the write itself, in its own words.
    RefusedTheWrite { reason: Option<String> },
}

impl Rejection {
    pub fn reason(&self) -> String {
        match self {
            Rejection::RemoteMoved => {
                "another machine published to it after this run last looked".to_string()
            }
            Rejection::RefusedTheWrite { reason: Some(said) } => said.clone(),
            Rejection::RefusedTheWrite { reason: None } => {
                "no reason reported by the remote".to_string()
            }
        }
    }
}

impl PushReport {
    pub fn unpushed_scopes(&self) -> &[String] {
        match self {
            PushReport::UpToDate => &[],
            PushReport::Refused { scopes, .. } => scopes,
            PushReport::Unreachable { scopes, .. } => scopes,
        }
    }
}

/// Scopes whose local bookmark is not where the remote has it.
///
/// Scopes only. The remote is a git remote and anything with git can push to
/// it, so it holds refs that are nobody's scope — an experiment, a backup, a
/// fork of one branch. Dotsync used to offer every bookmark it held, which
/// meant a run would recreate a branch its owner had deleted and undo a rewind
/// its owner meant, without saying anything: it modelled scopes and acted on
/// refs. What makes the two the same set now is that scope
/// membership is structural.
fn pending_bookmark_updates(
    repo: &dyn Repo,
    graph: &ScopeGraph,
) -> Vec<(RefNameBuf, BookmarkPushUpdate)> {
    repo.view()
        .local_remote_bookmarks(ORIGIN.as_ref())
        .filter(|(name, _)| graph.contains(name.as_str()))
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

/// The scopes this machine holds a commit for that the remote has never seen.
///
/// The same set the next push would offer, asked as a question rather than
/// acted on — which is what lets `status` answer it. A refused push used to be
/// reported by the run that hit it and nowhere else, so once that output
/// scrolled away a machine holding unpublished commits read as completely
/// clean; that is how the 2026-07-27 machine sat unnoticed for sixteen days.
pub(crate) fn unpushed_scopes(repo: &dyn Repo, graph: &ScopeGraph) -> Vec<String> {
    pending_bookmark_updates(repo, graph)
        .into_iter()
        .map(|(name, _)| name.as_str().to_string())
        .collect()
}

pub(crate) async fn push_scope_updates(session: &mut Session) -> Result<PushReport, DotsyncError> {
    let repo = session.repo().clone();
    let settings = default_settings()?;
    let subprocess_options = GitSubprocessOptions::from_settings(&settings)
        .map_err(|err| jj_error(format!("load git subprocess settings: {err}")))?;

    let updates = pending_bookmark_updates(repo.as_ref(), session.graph());

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
    // jj separates the two: `rejected` is the lease this run offered failing,
    // and `remote_rejected` is the remote turning the write down. Anything
    // that is not purely the first is treated as the second, so a run only
    // ever retries a race it can actually win.
    let rejection = match stats.remote_rejected.is_empty() && !stats.rejected.is_empty() {
        true => Rejection::RemoteMoved,
        false => Rejection::RefusedTheWrite {
            reason: stats
                .remote_rejected
                .iter()
                .chain(stats.rejected.iter())
                .find_map(|(_, reason)| reason.clone()),
        },
    };
    Ok(PushReport::Refused {
        scopes: refused,
        rejection,
    })
}

/// Every managed path a tree holds, and what is at it — `None` where the tree
/// holds a conflict there.
///
/// A contested scope head's tree is the merge of both sides, so a conflict in
/// it is an ordinary thing for a reader to meet: the path is one this machine
/// cannot state the fate of until the merge is resolved. Readers describe that;
/// writers cannot use it, and take `collect_managed_tree_entries` instead.
pub(crate) fn managed_tree_entries(
    tree: &jj_lib::merged_tree::MergedTree,
) -> Result<BTreeMap<PathBuf, Option<TreeValue>>, DotsyncError> {
    let mut entries = BTreeMap::new();
    for (path, value) in tree.entries() {
        let display_path = PathBuf::from(path.as_internal_file_string());
        let value = value.map_err(|err| {
            jj_error(format!("read tree entry {}: {err}", display_path.display()))
        })?;
        let Some(value) = value.as_resolved() else {
            entries.insert(display_path, None);
            continue;
        };
        let Some(value) = value.clone() else {
            continue;
        };
        match value {
            TreeValue::Tree(_) => {}
            other => {
                entries.insert(display_path, Some(other));
            }
        }
    }
    Ok(entries)
}

/// The same, for a caller about to write home or a commit from it: a
/// conflicted entry has no bytes to write, so it is a stop.
pub(crate) fn collect_managed_tree_entries(
    tree: &jj_lib::merged_tree::MergedTree,
) -> Result<BTreeMap<PathBuf, TreeValue>, DotsyncError> {
    let mut entries = BTreeMap::new();
    for (path, value) in managed_tree_entries(tree)? {
        let Some(value) = value else {
            return Err(jj_error(format!(
                "tree entry {} is conflicted during sync",
                path.display()
            )));
        };
        entries.insert(path, value);
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
///
/// Nothing is abandoned. Abandoning a commit the remote no longer reaches
/// means rewriting whatever sits on top of it, which is jj's answer for a
/// person's own unpublished work and the wrong one for history several
/// machines have: dotsync converges by merging and never rewrites. It was also
/// a stop nothing recovered from — a branch its owner deleted or force-pushed
/// left the fetch abandoning its commits and jj asserting that the rewrites
/// had not been rebased, on every command that fetches, `status` included.
pub(crate) fn default_import_options() -> GitImportOptions {
    GitImportOptions {
        auto_local_bookmark: true,
        abandon_unreachable_commits: false,
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
