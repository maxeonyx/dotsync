use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use jj_lib::backend::CommitId;
use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::rewrite::merge_commit_trees;

use crate::cascade::{
    build_cascade_plan, execute_cascade_steps, CascadeCommand, CascadeOutcome, CascadeStep,
    ScopeHeads,
};
use crate::config::{
    internal_repo_paths, load_config, DotsyncPaths, ALL_SCOPE, DOTSYNC_CONFIG_RELATIVE_PATH,
};
use crate::drift::{classify_home_against_scope, read_home_bytes, FileState};
use crate::error::{CommitPathProblem, DotsyncError, RefusedCommitPath, RejectedCommitPath};
use crate::repo::{
    collect_managed_tree_entries, fetch_origin, load_repo_direct, load_scope_commit,
    push_scope_updates, read_tree_entry_bytes, PushReport,
};
use crate::scope_graph::ScopeGraph;
use crate::sync::{ForceScope, SyncReport};

#[derive(Debug, Clone)]
pub struct CommitOptions {
    pub scope: String,
    pub message: String,
    /// Home wins for the paths this commit names, whatever the drift
    /// classifier says about them — and for nothing else. Selection and
    /// authority ride the same argument list on purpose: naming a path says
    /// "include this", and forcing says "and home is right about it", so a
    /// forced commit cannot reach a file it never mentioned.
    pub force: bool,
    /// Empty means every managed file this machine has changed, which is the
    /// same set `dotsync status` reports as changes.
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct PausedCascadeState {
    machine_scope: String,
    paused_scope: String,
    parent_commit_ids: Vec<String>,
    remaining_steps: Vec<PausedCascadeStep>,
    description: String,
    #[serde(default)]
    original_scope_commit_ids: BTreeMap<String, String>,
    /// The conflicted files, and what each held in home when the cascade
    /// paused. `continue` resolves exactly these paths, and refuses when they
    /// have not changed — see `unresolved_conflicted_files`. Defaulted rather
    /// than required so that a pause file written before this field existed
    /// still loads and can still be aborted; `continue` refuses such a pause
    /// outright. Deleted with this whole file when conflicts become commits
    /// (PLAN item 3), like the `WithheldPausedCascade` publish guard.
    #[serde(default)]
    paused_home_contents: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct PausedCascadeStep {
    scope: String,
    parent_scopes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CommitReport {
    pub committed_scope: String,
    /// Paths recorded on the authority of `--force` rather than on the
    /// authority of a change made on this machine. Reported because a forced
    /// commit is the one shape of commit that can discard someone else's work,
    /// and a run that does that has to say so.
    pub forced_overwrites: Vec<PathBuf>,
    pub sync: SyncReport,
    pub push: PushReport,
}

impl CommitReport {
    /// A commit that found nothing to add. It creates no history of its own,
    /// but it still names the scope it targeted and reports what it published
    /// on behalf of earlier runs.
    fn nothing_to_commit(scope: &str, push: PushReport) -> Self {
        Self {
            committed_scope: scope.to_string(),
            forced_overwrites: Vec::new(),
            sync: SyncReport::default(),
            push,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContinueReport {
    pub sync: SyncReport,
    pub push: PushReport,
}

#[derive(Debug, Clone, Default)]
pub struct AbortReport {
    pub aborted_scope: String,
    pub sync: SyncReport,
}

pub async fn commit_and_sync(
    paths: &DotsyncPaths,
    options: CommitOptions,
) -> Result<CommitReport, DotsyncError> {
    reject_commit_if_cascade_paused(paths)?;

    let repo = load_repo_direct(paths).await?;
    let _fetched_repo = fetch_origin(repo).await?;
    // Publish what earlier runs left behind before looking at this commit at
    // all: this commit may turn out to add nothing, and a machine with an
    // interrupted push behind it must still heal. Anything this run goes on to
    // create is published by the push after the cascade.
    let pending_push = push_scope_updates(paths).await?;
    // The push may have written an operation of its own, so build this commit
    // on the repo state it left behind rather than on the fetched snapshot.
    let repo = load_repo_direct(paths).await?;
    let config = load_config(paths).await?;
    let graph = config.graph.clone();

    if !graph.parents.contains_key(&options.scope) {
        return Err(DotsyncError::InvalidScope {
            scope: options.scope.clone(),
        });
    }

    let internal_paths = internal_repo_paths(&config);
    let sync_state = crate::sync::load_sync_state(paths, &config)?;
    let machine_scope = crate::sync::resolve_current_scope(&config, sync_state.as_ref(), None)?;
    let target_entries = load_scope_entries(repo.as_ref(), &options.scope, &internal_paths)?;

    // Whether a home file holds a change of this machine's own is a question
    // about this machine, not about the scope the change is headed for: it is
    // the same three-way classification `status` reports. Which tree the bytes
    // are written into is the separate question, answered further down.
    let named_paths = if options.paths.is_empty() {
        None
    } else {
        Some(expand_selection_paths(
            paths,
            &options.scope,
            &options.paths,
            &target_entries,
            &internal_paths,
        )?)
    };
    let classification = classify_home_against_scope(
        paths,
        repo.as_ref(),
        &config,
        sync_state.as_ref(),
        &machine_scope,
        &named_paths.iter().flatten().cloned().collect(),
    )
    .await?;

    let selected_paths = match named_paths {
        Some(named) => named,
        // The default selection is exactly what `status` calls a change, so
        // the two commands cannot disagree about what a bare commit records.
        None => classification
            .paths
            .iter()
            .filter(|(_, path)| path.state.is_drift())
            .map(|(relative, _)| relative.clone())
            .collect(),
    };
    reject_scope_graph_outside_all(
        paths,
        repo.as_ref(),
        &options.scope,
        &target_entries,
        &selected_paths,
    )
    .await?;

    let refused = selected_paths
        .iter()
        .filter(|_| !options.force)
        .filter_map(|relative| {
            let state = classification.state(relative);
            state.blocks_commit().then(|| RefusedCommitPath {
                path: relative.clone(),
                state,
            })
        })
        .collect::<Vec<_>>();
    if !refused.is_empty() {
        return Err(DotsyncError::StaleCommitPaths {
            scope: options.scope,
            refused,
        });
    }

    // Forcing means "home wins here regardless", so these paths skip the merge
    // below entirely. Recording which ones actually needed that authority is
    // what puts a deliberate revert on the record.
    let forced_paths: Vec<PathBuf> = if options.force {
        selected_paths.clone()
    } else {
        Vec::new()
    };
    let forced_overwrites = forced_paths
        .iter()
        .filter(|relative| {
            let state = classification.state(relative);
            state.blocks_commit() || state == FileState::DivergedEdit
        })
        .cloned()
        .collect::<Vec<_>>();

    if selected_paths.is_empty() {
        return Ok(CommitReport::nothing_to_commit(
            &options.scope,
            pending_push,
        ));
    }

    let last_synced_commit = sync_state
        .as_ref()
        .and_then(|state| repo.store().get_commit(&state.last_synced_revision).ok());

    let mut tx = repo.start_transaction();
    let mut scope_heads = ScopeHeads::load_existing(tx.repo_mut().base_repo(), &graph)?;
    let original_scope_commit_ids = scope_heads.commit_ids_by_scope();
    let base_commit = scope_heads.require(&options.scope)?;

    let merge_base_tree = commit_merge_base_tree(
        tx.repo_mut(),
        &graph,
        &options.scope,
        &machine_scope,
        &base_commit,
        last_synced_commit.as_ref(),
    )
    .await?;
    let mut builder = MergedTreeBuilder::new(merge_base_tree.clone());
    for relative in &selected_paths {
        apply_home_path_to_tree(tx.repo_mut(), paths, relative, &mut builder).await?;
    }
    let home_tree = builder.write_tree().await.map_err(|err| DotsyncError::Jj {
        message: format!("write commit tree for {}: {err}", options.scope),
    })?;

    // Three sides: what the target scope held when this machine last synced,
    // what it holds now, and that same base with this commit's home bytes laid
    // over it. When the scope has not moved the first two are equal and this
    // is a plain assignment; when another machine has moved it, this is the
    // merge that keeps their change instead of overwriting it.
    let merged_tree = MergedTree::merge(Merge::from_removes_adds(
        [(
            merge_base_tree,
            "the state this machine last synced".to_string(),
        )],
        [
            (base_commit.tree(), format!("scope `{}`", options.scope)),
            (home_tree, "your home edit".to_string()),
        ],
    ))
    .await
    .map_err(|err| DotsyncError::Jj {
        message: format!("merge home edit into {}: {err}", options.scope),
    })?;

    let mut builder = MergedTreeBuilder::new(merged_tree);
    for relative in &forced_paths {
        apply_home_path_to_tree(tx.repo_mut(), paths, relative, &mut builder).await?;
    }
    let new_tree = builder.write_tree().await.map_err(|err| DotsyncError::Jj {
        message: format!("write forced commit tree for {}: {err}", options.scope),
    })?;

    if new_tree.has_conflict() {
        let conflicted_files = conflicted_files_from_tree(&new_tree, &options.scope)?;
        let conflicted_paths = conflicted_files
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        // Nothing was written, so there is no transaction to keep: the pause
        // resolves against the scope head that is already there.
        save_paused_cascade_state(
            paths,
            &PausedCascadeState {
                machine_scope: machine_scope.clone(),
                paused_scope: options.scope.clone(),
                parent_commit_ids: vec![base_commit.id().hex()],
                remaining_steps: build_cascade_plan(
                    &graph,
                    &scope_heads,
                    &CascadeCommand {
                        root_scope: options.scope.clone(),
                        description: format!("dotsync: cascade from {}", options.scope),
                    },
                )
                .iter()
                .map(|step| PausedCascadeStep {
                    scope: step.scope.clone(),
                    parent_scopes: step.parent_scopes.clone(),
                })
                .collect(),
                description: options.message.clone(),
                original_scope_commit_ids: original_scope_commit_ids.clone(),
                paused_home_contents: read_home_contents(paths, &conflicted_paths)?,
            },
        )?;
        return Err(DotsyncError::CascadePaused {
            scope: options.scope,
            conflicted_files: conflicted_files.join(", "),
        });
    }

    if new_tree.tree_ids() == base_commit.tree().tree_ids() {
        return Ok(CommitReport::nothing_to_commit(
            &options.scope,
            pending_push,
        ));
    }

    let new_commit = tx
        .repo_mut()
        .new_commit(vec![base_commit.id().clone()], new_tree)
        .set_description(&options.message)
        .write()
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!("write commit for {}: {err}", options.scope),
        })?;
    tx.repo_mut().set_local_bookmark_target(
        RefNameBuf::from(options.scope.as_str()).as_ref(),
        RefTarget::normal(new_commit.id().clone()),
    );
    scope_heads.update(options.scope.clone(), new_commit);

    let cascade_command = CascadeCommand {
        root_scope: options.scope.clone(),
        description: format!("dotsync: cascade from {}", options.scope),
    };
    let plan = build_cascade_plan(&graph, &scope_heads, &cascade_command);
    match execute_cascade_steps(tx.repo_mut(), &mut scope_heads, &plan, &cascade_command).await? {
        CascadeOutcome::Completed => {}
        CascadeOutcome::Paused {
            scope,
            conflicted_files,
        } => {
            let paused_step = plan
                .iter()
                .find(|step| step.scope == scope)
                .ok_or_else(|| DotsyncError::Jj {
                    message: format!("paused cascade step `{scope}` was not in plan"),
                })?;
            let parent_commit_ids = parent_commit_ids_for_step(&scope_heads, paused_step)?;
            let remaining_steps = remaining_steps_after_pause(&plan, &scope);
            tx.commit("dotsync: pause cascade")
                .await
                .map_err(|err| DotsyncError::Jj {
                    message: format!("commit paused cascade state for {}: {err}", options.scope),
                })?;
            let conflicted_paths = conflicted_files
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            save_paused_cascade_state(
                paths,
                &PausedCascadeState {
                    machine_scope,
                    paused_scope: scope.clone(),
                    parent_commit_ids,
                    remaining_steps,
                    description: cascade_command.description,
                    original_scope_commit_ids,
                    paused_home_contents: read_home_contents(paths, &conflicted_paths)?,
                },
            )?;
            return Err(DotsyncError::CascadePaused {
                scope,
                conflicted_files: conflicted_files.join(", "),
            });
        }
    }

    tx.commit("dotsync: commit and cascade")
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!("commit scoped change for {}: {err}", options.scope),
        })?;

    // Push as soon as the history exists: the home sync below can legitimately
    // stop on drift, and a stop must never strand committed scope history.
    let push = push_scope_updates(paths).await?;
    let sync = crate::sync::sync_repo_to_home(
        paths,
        ForceScope::from_paths(&forced_paths),
        Some(&machine_scope),
    )
    .await?;

    Ok(CommitReport {
        committed_scope: options.scope,
        forced_overwrites,
        sync,
        push,
    })
}

/// The tree a home edit is a change *against*: the target scope as it stood
/// when this machine last synced.
///
/// Home was written from the machine scope's tip at the last sync, and that
/// tip descends from the target scope's head at that moment — so the common
/// ancestor of the target scope's head now and this machine's last-synced
/// commit is exactly the version of the target scope home was derived from.
/// When nobody else has moved the scope, that ancestor *is* the current head
/// and the merge below degenerates into the plain assignment dotsync has
/// always done. When somebody has, it is what turns their change into a
/// three-way merge instead of a silent overwrite — and unlike the old
/// "whatever the bookmark pointed at before this run fetched", it cannot be
/// moved by an earlier read-only command, because nothing but a completed sync
/// writes the sync state.
///
/// Falls back to the scope's own head when the target is not an ancestor of
/// this machine's scope: home was never derived from such a scope, so there is
/// no version of it this machine can claim to have started from. Whether those
/// commits should be allowed at all is an open question (PLAN.md §1.5, D6);
/// until it is answered they behave exactly as they did before.
async fn commit_merge_base_tree(
    mut_repo: &mut jj_lib::repo::MutableRepo,
    graph: &ScopeGraph,
    target_scope: &str,
    machine_scope: &str,
    target_head: &jj_lib::commit::Commit,
    last_synced: Option<&jj_lib::commit::Commit>,
) -> Result<jj_lib::merged_tree::MergedTree, DotsyncError> {
    let Some(last_synced) = last_synced else {
        return Ok(target_head.tree());
    };
    if !scope_is_ancestor_or_self(graph, target_scope, machine_scope) {
        return Ok(target_head.tree());
    }

    let base_ids = mut_repo
        .index()
        .common_ancestors(&[target_head.id().clone()], &[last_synced.id().clone()])
        .map_err(|err| DotsyncError::Jj {
            message: format!("find the base of the home edit for {target_scope}: {err}"),
        })?;
    if base_ids.is_empty() {
        return Ok(target_head.tree());
    }

    let base_commits = base_ids
        .iter()
        .map(|id| {
            mut_repo
                .store()
                .get_commit(id)
                .map_err(|err| DotsyncError::Jj {
                    message: format!("load the base of the home edit for {target_scope}: {err}"),
                })
        })
        .collect::<Result<Vec<_>, DotsyncError>>()?;
    merge_commit_trees(mut_repo, &base_commits)
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!("merge the bases of the home edit for {target_scope}: {err}"),
        })
}

fn scope_is_ancestor_or_self(graph: &ScopeGraph, ancestor: &str, scope: &str) -> bool {
    let mut stack = vec![scope.to_string()];
    let mut seen = BTreeSet::new();
    while let Some(candidate) = stack.pop() {
        if candidate == ancestor {
            return true;
        }
        if !seen.insert(candidate.clone()) {
            continue;
        }
        if let Some(parents) = graph.parents.get(&candidate) {
            stack.extend(parents.iter().cloned());
        }
    }
    false
}

fn conflicted_files_from_tree(
    tree: &jj_lib::merged_tree::MergedTree,
    scope: &str,
) -> Result<Vec<String>, DotsyncError> {
    tree.conflicts()
        .map(|(path, value)| {
            value.map_err(|err| DotsyncError::Jj {
                message: format!("read conflict for {scope}: {err}"),
            })?;
            Ok(path.as_internal_file_string().to_string())
        })
        .collect()
}

fn load_scope_entries(
    repo: &dyn jj_lib::repo::Repo,
    scope: &str,
    internal_paths: &std::collections::BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, TreeValue>, DotsyncError> {
    let commit = load_scope_commit(repo, scope)?;
    collect_managed_tree_entries(&commit.tree(), internal_paths)
}

/// Refuses a commit that would record a change to the scope graph on a scope
/// that is not `all`. Dotsync reads the graph only from `all`
/// (`config::load_config`), so such a commit writes a copy that configures
/// nothing — while still syncing into home on that scope's machines, where it
/// overwrites the real one. An unchanged copy records nothing and is left
/// alone, which is what keeps bulk selections working. Checked against what
/// the selection expanded to, because a directory selection reaches the scope
/// graph too.
async fn reject_scope_graph_outside_all(
    paths: &DotsyncPaths,
    repo: &dyn jj_lib::repo::Repo,
    scope: &str,
    target_entries: &BTreeMap<PathBuf, TreeValue>,
    selected_paths: &[PathBuf],
) -> Result<(), DotsyncError> {
    if scope == ALL_SCOPE {
        return Ok(());
    }
    let config_path = PathBuf::from(DOTSYNC_CONFIG_RELATIVE_PATH);
    if !selected_paths.contains(&config_path) {
        return Ok(());
    }
    let repo_bytes = match target_entries.get(&config_path) {
        Some(value) => Some(read_tree_entry_bytes(repo.store(), &config_path, value).await?),
        None => None,
    };
    if read_home_bytes(paths, &config_path)? == repo_bytes {
        return Ok(());
    }
    Err(DotsyncError::UnusableCommitPaths {
        scope: scope.to_string(),
        rejected: vec![RejectedCommitPath {
            path: config_path,
            problem: CommitPathProblem::ScopeGraphOutsideAllScope,
        }],
    })
}

fn expand_selection_paths(
    paths: &DotsyncPaths,
    scope: &str,
    selection_paths: &[PathBuf],
    target_entries: &BTreeMap<PathBuf, TreeValue>,
    internal_paths: &BTreeSet<PathBuf>,
) -> Result<Vec<PathBuf>, DotsyncError> {
    let mut expanded = BTreeSet::new();
    // Relative path of the repo root within home — reject anything under it
    let repo_relative = paths
        .repo_root
        .strip_prefix(&paths.home_dir)
        .ok()
        .map(|p| p.to_path_buf());

    let mut rejected = Vec::new();
    for selection_path in selection_paths {
        if let Some(problem) = unusable_commit_path(
            paths,
            selection_path,
            internal_paths,
            repo_relative.as_deref(),
        ) {
            rejected.push(RejectedCommitPath {
                path: selection_path.clone(),
                problem,
            });
            continue;
        }

        let home_path = paths.home_dir.join(selection_path);
        let is_directory_selection = home_path.is_dir()
            || target_entries.keys().any(|candidate| {
                candidate != selection_path && path_has_prefix(candidate, selection_path)
            });
        let mut matched = BTreeSet::new();
        if is_directory_selection {
            if home_path.exists() {
                collect_home_directory_files(
                    &paths.home_dir,
                    &home_path,
                    &mut matched,
                    internal_paths,
                    &paths.repo_root,
                )?;
            }
            matched.extend(
                target_entries
                    .keys()
                    .filter(|candidate| path_has_prefix(candidate, selection_path))
                    .cloned(),
            );
        } else if home_path.exists() || target_entries.contains_key(selection_path) {
            // A tracked path whose home file is gone is a deletion, so the
            // tracked side counts as a match too.
            matched.insert(selection_path.clone());
        }

        // A path that matches nothing names neither a home file nor a tracked
        // file. Committing it would report success having recorded nothing,
        // which is how a typo reads to an agent as a saved config change.
        if matched.is_empty() {
            rejected.push(RejectedCommitPath {
                path: selection_path.clone(),
                problem: CommitPathProblem::Unmatched { home_path },
            });
            continue;
        }
        expanded.extend(matched);
    }

    // Every bad path in one answer: an agent that fixes one and reruns pays a
    // full fetch-and-commit attempt to discover the next one.
    if !rejected.is_empty() {
        return Err(DotsyncError::UnusableCommitPaths {
            scope: scope.to_string(),
            rejected,
        });
    }

    Ok(expanded.into_iter().collect())
}

/// Why dotsync will not record this path, if it will not. Runs before any
/// matching, because these paths are refused for what they are rather than
/// for what they do or do not resolve to.
fn unusable_commit_path(
    paths: &DotsyncPaths,
    selection_path: &Path,
    internal_paths: &BTreeSet<PathBuf>,
    repo_relative: Option<&Path>,
) -> Option<CommitPathProblem> {
    // An absolute path resolves against the filesystem root, not against home,
    // so it can never name a repo-relative file.
    if selection_path.is_absolute() {
        return Some(CommitPathProblem::Absolute);
    }
    // Dotsync stores this path verbatim as a repo path and every machine on
    // the scope joins it onto its own home directory. A `..` component
    // therefore writes outside home on every machine, so containment has to be
    // checked here rather than trusted to the repo layer.
    if selection_path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Some(CommitPathProblem::EscapesHome);
    }
    // Dotsync's own state lives in home too, and none of it is config.
    if internal_paths.contains(selection_path) {
        return Some(CommitPathProblem::SyncState);
    }
    if let Some(repo_relative) = repo_relative {
        if selection_path == repo_relative {
            return Some(CommitPathProblem::DotsyncRepoRoot {
                repo_root: paths.repo_root.clone(),
            });
        }
        if selection_path.starts_with(repo_relative) {
            return Some(CommitPathProblem::InsideDotsyncRepo {
                repo_root: paths.repo_root.clone(),
            });
        }
    }
    None
}

fn collect_home_directory_files(
    home_root: &Path,
    current: &Path,
    expanded: &mut BTreeSet<PathBuf>,
    internal_paths: &BTreeSet<PathBuf>,
    repo_root: &Path,
) -> Result<(), DotsyncError> {
    for entry in fs::read_dir(current).map_err(|source| DotsyncError::Io {
        path: current.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| DotsyncError::Io {
            path: current.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| DotsyncError::Io {
            path: path.clone(),
            source,
        })?;

        if file_type.is_dir() {
            // Never recurse into the dotsync repo directory itself
            if path.starts_with(repo_root) {
                continue;
            }
            collect_home_directory_files(home_root, &path, expanded, internal_paths, repo_root)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let relative = path
            .strip_prefix(home_root)
            .map_err(|source| DotsyncError::Jj {
                message: format!(
                    "failed to make home path {} relative to {}: {source}",
                    path.display(),
                    home_root.display()
                ),
            })?;
        let relative = relative.to_path_buf();
        if internal_paths.contains(&relative) {
            continue;
        }
        expanded.insert(relative);
    }

    Ok(())
}

fn path_has_prefix(path: &Path, prefix: &Path) -> bool {
    path == prefix || path.starts_with(prefix)
}

async fn apply_home_path_to_tree(
    mut_repo: &mut jj_lib::repo::MutableRepo,
    paths: &DotsyncPaths,
    relative: &Path,
    builder: &mut MergedTreeBuilder,
) -> Result<(), DotsyncError> {
    let relative_str = relative.to_str().ok_or_else(|| DotsyncError::NonUtf8Path {
        path: relative.to_path_buf(),
    })?;
    let repo_path =
        RepoPathBuf::from_internal_string(relative_str).map_err(|err| DotsyncError::Jj {
            message: format!("invalid repo path {}: {err}", relative.display()),
        })?;

    let home_path = paths.home_dir.join(relative);
    if home_path.exists() {
        let bytes = fs::read(&home_path).map_err(|source| DotsyncError::Io {
            path: home_path,
            source,
        })?;
        let mut reader = bytes.as_slice();
        let file_id = mut_repo
            .store()
            .write_file(repo_path.as_ref(), &mut reader)
            .await
            .map_err(|err| DotsyncError::Jj {
                message: format!("write repo file {}: {err}", relative.display()),
            })?;
        builder.set_or_remove(
            repo_path,
            Merge::normal(TreeValue::File {
                id: file_id,
                executable: false,
                copy_id: CopyId::placeholder(),
            }),
        );
    } else {
        builder.set_or_remove(repo_path, Merge::absent());
    }

    Ok(())
}

/// Home contents of the conflicted files, recorded when a cascade pauses so
/// `continue` can tell a resolution from an untouched file.
fn read_home_contents(
    paths: &DotsyncPaths,
    relatives: &[PathBuf],
) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>, DotsyncError> {
    relatives
        .iter()
        .map(|relative| Ok((relative.clone(), read_home_bytes(paths, relative)?)))
        .collect()
}

/// Conflicted files that hold exactly what they held when the cascade paused.
///
/// Today's pause never materializes conflict markers into home, so DESIGN's
/// "`continue` verifies the markers are gone" is vacuously true and `continue`
/// takes home's untouched content as the resolution — silently deleting the
/// losing side and reporting success. Until conflicts become commits and the
/// two sides really are written into home (PLAN item 3), an unchanged file is
/// proof that no resolution was made. Deleted with the pause file.
fn unresolved_conflicted_files(
    paths: &DotsyncPaths,
    state: &PausedCascadeState,
) -> Result<Vec<PathBuf>, DotsyncError> {
    let mut unresolved = Vec::new();
    for (relative, paused_contents) in &state.paused_home_contents {
        if read_home_bytes(paths, relative)? == *paused_contents {
            unresolved.push(relative.clone());
        }
    }
    Ok(unresolved)
}

pub async fn continue_after_conflict(
    paths: &DotsyncPaths,
    force: ForceScope,
) -> Result<ContinueReport, DotsyncError> {
    let state = load_paused_cascade_state(paths)?;
    // A cascade pauses because at least one file conflicted, so an empty
    // record means the pause was written before this check existed rather than
    // that nothing conflicted. Skipping the check there would reopen the
    // silent discard exactly when a machine upgrades mid-pause; `abort` reads
    // nothing this pause lacks, so refusing does not wedge it.
    if state.paused_home_contents.is_empty() {
        return Err(DotsyncError::PausePredatesResolutionCheck {
            scope: state.paused_scope,
        });
    }
    let unresolved = unresolved_conflicted_files(paths, &state)?;
    if !unresolved.is_empty() {
        return Err(DotsyncError::UnresolvedConflict {
            scope: state.paused_scope,
            paths: unresolved,
        });
    }
    let repo = load_repo_direct(paths).await?;
    let config = load_config(paths).await?;
    let mut tx = repo.start_transaction();
    let mut scope_heads = ScopeHeads::load_existing(tx.repo_mut().base_repo(), &config.graph)?;
    let parent_commits = state
        .parent_commit_ids
        .iter()
        .map(|id| load_commit_by_hex(tx.repo_mut(), id))
        .collect::<Result<Vec<_>, DotsyncError>>()?;
    if parent_commits.is_empty() {
        return Err(DotsyncError::Jj {
            message: "paused cascade has no parent commits".to_string(),
        });
    }
    let merged_tree = merge_commit_trees(tx.repo_mut(), &parent_commits)
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!(
                "merge paused cascade parents for {}: {err}",
                state.paused_scope
            ),
        })?;
    let mut builder = MergedTreeBuilder::new(merged_tree);
    for relative in state.paused_home_contents.keys() {
        apply_home_path_to_tree(tx.repo_mut(), paths, relative, &mut builder).await?;
    }
    let resolved_tree = builder.write_tree().await.map_err(|err| DotsyncError::Jj {
        message: format!("write resolved tree for {}: {err}", state.paused_scope),
    })?;
    let resolved_commit = tx
        .repo_mut()
        .new_commit(
            parent_commits
                .iter()
                .map(|commit| commit.id().clone())
                .collect(),
            resolved_tree,
        )
        .set_description(&state.description)
        .write()
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!(
                "write resolved cascade commit for {}: {err}",
                state.paused_scope
            ),
        })?;
    tx.repo_mut().set_local_bookmark_target(
        RefNameBuf::from(state.paused_scope.as_str()).as_ref(),
        RefTarget::normal(resolved_commit.id().clone()),
    );
    scope_heads.update(state.paused_scope.clone(), resolved_commit);

    let command = CascadeCommand {
        root_scope: state.paused_scope.clone(),
        description: state.description.clone(),
    };
    let remaining_plan = state
        .remaining_steps
        .iter()
        .map(|step| CascadeStep {
            scope: step.scope.clone(),
            parent_scopes: step.parent_scopes.clone(),
        })
        .collect::<Vec<_>>();
    match execute_cascade_steps(tx.repo_mut(), &mut scope_heads, &remaining_plan, &command).await? {
        CascadeOutcome::Completed => {}
        CascadeOutcome::Paused {
            scope,
            conflicted_files,
        } => {
            let paused_step = remaining_plan
                .iter()
                .find(|step| step.scope == scope)
                .ok_or_else(|| DotsyncError::Jj {
                    message: format!("paused cascade step `{scope}` was not in remaining plan"),
                })?;
            let parent_commit_ids = parent_commit_ids_for_step(&scope_heads, paused_step)?;
            let remaining_steps = remaining_steps_after_pause(&remaining_plan, &scope);
            tx.commit("dotsync: pause cascade again")
                .await
                .map_err(|err| DotsyncError::Jj {
                    message: format!("commit repeated paused cascade state: {err}"),
                })?;
            let conflicted_paths = conflicted_files
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            save_paused_cascade_state(
                paths,
                &PausedCascadeState {
                    machine_scope: state.machine_scope,
                    paused_scope: scope.clone(),
                    parent_commit_ids,
                    remaining_steps,
                    description: state.description,
                    original_scope_commit_ids: state.original_scope_commit_ids,
                    paused_home_contents: read_home_contents(paths, &conflicted_paths)?,
                },
            )?;
            return Err(DotsyncError::CascadePaused {
                scope,
                conflicted_files: conflicted_files.join(", "),
            });
        }
    }

    tx.commit("dotsync: continue cascade")
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!("commit continued cascade: {err}"),
        })?;
    remove_paused_cascade_state(paths)?;
    let push = push_scope_updates(paths).await?;
    let sync = crate::sync::sync_repo_to_home(paths, force, Some(&state.machine_scope)).await?;
    Ok(ContinueReport { sync, push })
}

pub async fn abort_paused_cascade(paths: &DotsyncPaths) -> Result<AbortReport, DotsyncError> {
    let state = load_paused_cascade_state(paths)?;
    if state.original_scope_commit_ids.is_empty() {
        return Err(DotsyncError::Jj {
            message: "paused cascade state does not include an abort checkpoint; resolve the conflict and run `dotsync continue` instead".to_string(),
        });
    }

    let repo = load_repo_direct(paths).await?;
    let mut tx = repo.start_transaction();
    for (scope, commit_id) in &state.original_scope_commit_ids {
        let commit = load_commit_by_hex(tx.repo_mut(), commit_id)?;
        tx.repo_mut().set_local_bookmark_target(
            RefNameBuf::from(scope.as_str()).as_ref(),
            RefTarget::normal(commit.id().clone()),
        );
    }
    tx.commit("dotsync: abort cascade")
        .await
        .map_err(|err| DotsyncError::Jj {
            message: format!("commit aborted cascade: {err}"),
        })?;
    remove_paused_cascade_state(paths)?;

    // Abort is a full sync of home back to the machine scope's pre-pause tip,
    // not a selective restore: the home edit that started the cascade is
    // exactly what abort exists to discard, so it cannot also be a reason to
    // refuse. Drift outside the paused selection goes the same way, which is
    // what DESIGN.md's "reverts all the config files" says and what the old
    // selective restore quietly did not do.
    let sync =
        crate::sync::sync_repo_to_home(paths, ForceScope::Everything, Some(&state.machine_scope))
            .await?;

    Ok(AbortReport {
        aborted_scope: state.paused_scope,
        sync,
    })
}

fn paused_cascade_state_path(paths: &DotsyncPaths) -> PathBuf {
    paths.repo_root.join(".dotsync-paused-cascade.json")
}

fn save_paused_cascade_state(
    paths: &DotsyncPaths,
    state: &PausedCascadeState,
) -> Result<(), DotsyncError> {
    let path = paused_cascade_state_path(paths);
    let contents = serde_json::to_vec_pretty(state).map_err(|err| DotsyncError::Jj {
        message: format!("serialize paused cascade state: {err}"),
    })?;
    fs::write(&path, contents).map_err(|source| DotsyncError::Io { path, source })
}

fn load_paused_cascade_state(paths: &DotsyncPaths) -> Result<PausedCascadeState, DotsyncError> {
    let path = paused_cascade_state_path(paths);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(DotsyncError::NoPausedCascade);
        }
        Err(source) => return Err(DotsyncError::Io { path, source }),
    };
    serde_json::from_str(&contents).map_err(|err| DotsyncError::Jj {
        message: format!("parse paused cascade state {}: {err}", path.display()),
    })
}

fn remove_paused_cascade_state(paths: &DotsyncPaths) -> Result<(), DotsyncError> {
    let path = paused_cascade_state_path(paths);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DotsyncError::Io { path, source }),
    }
}

/// The scope a paused cascade stopped at, if one is paused. Reads the same
/// state file `continue` and `abort` read, and disappears with it when
/// conflicts become commits.
pub(crate) fn paused_cascade_scope(paths: &DotsyncPaths) -> Result<Option<String>, DotsyncError> {
    match load_paused_cascade_state(paths) {
        Ok(state) => Ok(Some(state.paused_scope)),
        Err(DotsyncError::NoPausedCascade) => Ok(None),
        Err(error) => Err(error),
    }
}

fn reject_commit_if_cascade_paused(paths: &DotsyncPaths) -> Result<(), DotsyncError> {
    match load_paused_cascade_state(paths) {
        Ok(state) => Err(DotsyncError::PausedCascadeInProgress {
            scope: state.paused_scope,
        }),
        Err(DotsyncError::NoPausedCascade) => Ok(()),
        Err(error) => Err(error),
    }
}

fn parent_commit_ids_for_step(
    scope_heads: &ScopeHeads,
    step: &CascadeStep,
) -> Result<Vec<String>, DotsyncError> {
    let mut ids = Vec::with_capacity(step.parent_scopes.len() + 1);
    ids.push(scope_heads.require(&step.scope)?.id().hex());
    for parent_scope in &step.parent_scopes {
        ids.push(scope_heads.require(parent_scope)?.id().hex());
    }
    Ok(ids)
}

fn remaining_steps_after_pause(
    steps: &[CascadeStep],
    paused_scope: &str,
) -> Vec<PausedCascadeStep> {
    steps
        .iter()
        .skip_while(|step| step.scope != paused_scope)
        .skip(1)
        .map(|step| PausedCascadeStep {
            scope: step.scope.clone(),
            parent_scopes: step.parent_scopes.clone(),
        })
        .collect()
}

fn load_commit_by_hex(
    repo: &dyn jj_lib::repo::Repo,
    id: &str,
) -> Result<jj_lib::commit::Commit, DotsyncError> {
    let commit_id = CommitId::try_from_hex(id).ok_or_else(|| DotsyncError::Jj {
        message: format!("paused cascade commit id `{id}` is not valid hex"),
    })?;
    repo.store()
        .get_commit(&commit_id)
        .map_err(|err| DotsyncError::Jj {
            message: format!("load paused cascade commit `{id}`: {err}"),
        })
}
