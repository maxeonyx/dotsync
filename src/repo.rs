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
        .map_err(|err| jj_error(format!("fetch origin: {err}")))?;
    fetch
        .import_refs()
        .map_err(|err| jj_error(format!("import fetched refs: {err}")))?;
    sync_local_bookmarks_from_remote(tx.repo_mut(), "origin".as_ref())?;
    tx.commit("dotsync: fetch origin")
        .await
        .map_err(|err| jj_error(format!("commit fetch operation: {err}")))
}

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

    for (name, remote_id) in &updates {
        let Some(local_id) = mut_repo
            .view()
            .get_local_bookmark(name.as_ref())
            .as_normal()
        else {
            continue;
        };
        if local_id == remote_id {
            continue;
        }
        let local_is_ancestor =
            mut_repo
                .index()
                .is_ancestor(local_id, remote_id)
                .map_err(|err| {
                    jj_error(format!(
                        "check bookmark ancestry for {}: {err}",
                        name.as_str()
                    ))
                })?;
        if !local_is_ancestor {
            return Err(DotsyncError::FetchWouldOverwriteLocalBookmark {
                bookmark: name.as_str().to_string(),
                local_target: local_id.hex(),
                remote_target: remote_id.hex(),
            });
        }
    }

    for (name, id) in updates {
        mut_repo.set_local_bookmark_target(name.as_ref(), RefTarget::normal(id));
    }

    Ok(())
}

pub(crate) async fn push_scope_updates(paths: &DotsyncPaths) -> Result<(), DotsyncError> {
    let repo = load_repo_direct(paths).await?;
    let settings = default_settings()?;
    let subprocess_options = GitSubprocessOptions::from_settings(&settings)
        .map_err(|err| jj_error(format!("load git subprocess settings: {err}")))?;

    let updates: Vec<(RefNameBuf, BookmarkPushUpdate)> = repo
        .view()
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
        .collect();

    if updates.is_empty() {
        return Ok(());
    }

    let mut tx = repo.start_transaction();
    git::push_branches(
        tx.repo_mut(),
        subprocess_options,
        "origin".as_ref(),
        &GitBranchPushTargets {
            branch_updates: updates,
        },
        &mut QuietGitCallback,
        &GitPushOptions::default(),
    )
    .map_err(|err| jj_error(format!("push branches: {err}")))?;
    tx.commit("dotsync: push scope updates")
        .await
        .map_err(|err| jj_error(format!("commit push operation: {err}")))?;
    Ok(())
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
        .ok_or_else(|| DotsyncError::MissingScopeBookmark {
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
    let relative_str = relative.to_str().ok_or(DotsyncError::NotImplemented(
        "non-utf8 repo paths are not supported yet",
    ))?;
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
        TreeValue::GitSubmodule(_) => Err(DotsyncError::NotImplemented(
            "git submodule sync is not supported yet",
        )),
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
