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
use crate::config::{internal_repo_paths, DotsyncPaths, ALL_SCOPE, DOTSYNC_CONFIG_RELATIVE_PATH};
use crate::drift::{classify_home_against_scope, read_home_bytes, FileState, RecordedFromHome};
use crate::error::{
    CommitPathProblem, DotsyncError, RefusedCommitPath, RejectedCommitPath, SkippedCommitPath,
};
use crate::repo::{
    collect_managed_tree_entries, load_scope_commit, push_scope_updates, read_tree_entry_bytes,
    PushReport,
};
use crate::scope_graph::ScopeGraph;
use crate::session::{in_session, Run, Session};
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
    /// This machine's own scope. Known before the commit decides whether it has
    /// anything to record, so both outcomes can name it — the empty string a
    /// no-op commit used to report came from standing in a default sync report
    /// for the sync it never ran.
    pub machine_scope: String,
    /// Paths a named directory matched that this commit left alone. Empty for
    /// every other shape of commit: a bare commit selects what changed rather
    /// than filtering a list, and a path named exactly is refused out loud.
    pub skipped: Vec<SkippedCommitPath>,
    pub push: PushReport,
    /// What the commit recorded, or `None` when it found nothing to record.
    ///
    /// A commit with nothing to record writes no history, so it also runs no
    /// cascade and no home sync — and therefore has no synced files, no newly
    /// tracked files and no forced overwrites, rather than empty lists of them.
    pub recorded: Option<RecordedCommit>,
}

/// The half of a commit report that only exists when the commit recorded
/// something.
#[derive(Debug, Clone)]
pub struct RecordedCommit {
    /// Paths this commit put on the scope for the first time. Every machine
    /// sharing that scope will have them written into its home directory, so a
    /// run that adds files says which ones rather than reading like a run that
    /// changed a line.
    pub newly_tracked: Vec<PathBuf>,
    /// Paths recorded on the authority of `--force` rather than on the
    /// authority of a change made on this machine. Reported because a forced
    /// commit is the one shape of commit that can discard someone else's work,
    /// and a run that does that has to say so.
    pub forced_overwrites: Vec<PathBuf>,
    pub sync: SyncReport,
}

impl CommitReport {
    /// A commit that found nothing to add. It creates no history of its own,
    /// but it still names the scope it targeted and reports what it published
    /// on behalf of earlier runs.
    fn nothing_to_commit(
        scope: &str,
        machine_scope: &str,
        skipped: Vec<SkippedCommitPath>,
        push: PushReport,
    ) -> Self {
        Self {
            committed_scope: scope.to_string(),
            machine_scope: machine_scope.to_string(),
            skipped,
            push,
            recorded: None,
        }
    }
}

/// A commit that stopped part-way, and what it had already done when it did.
///
/// A forced overwrite is finished the moment the history carrying it is
/// written and pushed: nothing later in the run can take it back. Returning a
/// bare error would drop that fact exactly when it matters most — a run that
/// reverted another machine's change and then failed.
#[derive(Debug)]
pub struct CommitFailure {
    pub forced_overwrites: Vec<PathBuf>,
    pub error: DotsyncError,
}

impl From<DotsyncError> for CommitFailure {
    /// Everything that can go wrong before any history exists.
    fn from(error: DotsyncError) -> Self {
        Self {
            forced_overwrites: Vec::new(),
            error,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContinueReport {
    pub sync: SyncReport,
    pub push: PushReport,
}

#[derive(Debug, Clone)]
pub struct AbortReport {
    /// The scope the cascade was paused at. It is not the scope that was
    /// aborted — the cascade was — and it is not the scope the discarded commit
    /// was made on either, which is why it says which of the three it is.
    pub paused_scope: String,
    pub sync: SyncReport,
}

pub async fn commit_and_sync(
    paths: &DotsyncPaths,
    options: CommitOptions,
) -> Run<Result<CommitReport, CommitFailure>> {
    in_session(paths, async |session, paths| {
        commit_in_session(session, paths, options).await
    })
    .await
}

async fn commit_in_session(
    session: &mut Session,
    paths: &DotsyncPaths,
    options: CommitOptions,
) -> Result<CommitReport, CommitFailure> {
    reject_commit_if_cascade_paused(paths)?;

    session.fetch().await?;
    // Publish what earlier runs left behind before looking at this commit at
    // all: this commit may turn out to add nothing, and a machine with an
    // interrupted push behind it must still heal. Anything this run goes on to
    // create is published by the push after the cascade.
    let pending_push = push_scope_updates(session).await?;
    let graph = session.config().graph.clone();

    if !graph.parents.contains_key(&options.scope) {
        return Err(DotsyncError::InvalidScope {
            scope: options.scope.clone(),
        }
        .into());
    }

    let internal_paths = internal_repo_paths(session.config());
    let sync_state = crate::sync::load_sync_state(session.paths(), session.config())?;
    let machine_scope =
        crate::sync::resolve_current_scope(session.config(), sync_state.as_ref(), None)?;
    let target_entries =
        load_scope_entries(session.repo().as_ref(), &options.scope, &internal_paths)?;

    let selection = select_changes_to_record(
        session,
        sync_state.as_ref(),
        &machine_scope,
        &options,
        &target_entries,
        &internal_paths,
    )
    .await?;
    let Selection {
        paths: selected_paths,
        newly_tracked,
        skipped,
        forced_paths,
        forced_overwrites,
    } = selection;

    if selected_paths.is_empty() {
        return Ok(CommitReport::nothing_to_commit(
            &options.scope,
            &machine_scope,
            skipped,
            pending_push,
        ));
    }

    let last_synced_commit = sync_state.as_ref().and_then(|state| {
        session
            .repo()
            .store()
            .get_commit(&state.last_synced_revision)
            .ok()
    });

    // Whether this commit's bytes reach this machine at all. When the target
    // scope is an ancestor of the machine scope the cascade carries them down
    // into home, so home has a version of the target scope it started from and
    // will have a version of it to catch up to. When it is not — another
    // machine's leaf, a sibling branch of the DAG — neither is true, and both
    // the merge base and the sync baseline below fall back to what dotsync did
    // before this wave. Whether such commits should be allowed without an
    // explicit per-path force is still open (PLAN.md §1.5, D6).
    let cascades_into_home = scope_is_ancestor_or_self(&graph, &options.scope, &machine_scope);

    let repo = session.repo().clone();
    let mut tx = repo.start_transaction();
    let mut scope_heads = ScopeHeads::load_existing(tx.repo_mut().base_repo(), &graph)?;
    let original_scope_commit_ids = scope_heads.commit_ids_by_scope();
    let base_commit = scope_heads.require(&options.scope)?;

    let merge_base_tree = commit_merge_base_tree(
        tx.repo_mut(),
        cascades_into_home,
        &options.scope,
        &base_commit,
        last_synced_commit.as_ref(),
    )
    .await?;
    let mut recorded_from_home = RecordedFromHome::default();
    let mut builder = MergedTreeBuilder::new(merge_base_tree.clone());
    for relative in &selected_paths {
        let bytes = apply_home_path_to_tree(tx.repo_mut(), paths, relative, &mut builder).await?;
        if cascades_into_home {
            recorded_from_home.record(relative, bytes);
        }
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
        let bytes = apply_home_path_to_tree(tx.repo_mut(), paths, relative, &mut builder).await?;
        if cascades_into_home {
            recorded_from_home.record(relative, bytes);
        }
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
        }
        .into());
    }

    if new_tree.tree_ids() == base_commit.tree().tree_ids() {
        return Ok(CommitReport::nothing_to_commit(
            &options.scope,
            &machine_scope,
            skipped,
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
            }
            .into());
        }
    }

    session
        .advance_to(
            tx.commit("dotsync: commit and cascade")
                .await
                .map_err(|err| DotsyncError::Jj {
                    message: format!("commit scoped change for {}: {err}", options.scope),
                })?,
        )
        .await?;

    // Push as soon as the history exists: the home sync below can legitimately
    // stop on drift, and a stop must never strand committed scope history.
    let push = push_scope_updates(session).await;
    // From here the forced history exists and is published, so every exit has
    // to carry what it overwrote.
    let stopped = |error: DotsyncError| CommitFailure {
        forced_overwrites: forced_overwrites.clone(),
        error,
    };
    let push = push.map_err(stopped)?;
    // The commit just read these paths out of home and wrote them into the
    // repo, so for this sync those bytes are the last-synced side: home is
    // behind whatever the cascade merged them into, not drifted from it.
    let sync = crate::sync::sync_repo_to_home(
        session,
        ForceScope::from_paths(&forced_paths),
        &recorded_from_home,
        Some(&machine_scope),
    )
    .await
    .map_err(stopped)?;

    Ok(CommitReport {
        committed_scope: options.scope,
        machine_scope,
        skipped,
        push,
        recorded: Some(RecordedCommit {
            newly_tracked,
            forced_overwrites,
            sync,
        }),
    })
}

/// What this commit will record, and on whose authority.
struct Selection {
    paths: Vec<PathBuf>,
    /// The selected paths the target scope does not have yet. Reported because
    /// putting a file on a shared scope is the one thing a commit does that
    /// every machine on that scope then has written into its home directory.
    newly_tracked: Vec<PathBuf>,
    /// Paths a named directory expanded to that this commit left alone,
    /// because home holds no change of this machine's own at them. Reported,
    /// never silent: a bulk selection that quietly recorded less than it
    /// matched would read to an agent as a complete commit.
    skipped: Vec<SkippedCommitPath>,
    /// The paths `--force` covers. These skip the merge below entirely: the
    /// point of forcing is that home wins here whatever the repo says.
    forced_paths: Vec<PathBuf>,
    /// The forced paths where that authority actually decided something —
    /// where, without it, the commit would have been refused or would have
    /// merged rather than overwritten.
    forced_overwrites: Vec<PathBuf>,
}

/// Decides which paths a commit records and whether it is allowed to.
///
/// Whether a home file holds a change of *this machine's own* is a question
/// about this machine, not about the scope the change is headed for, so it is
/// the same three-way classification `status` reports — which is also why a
/// bare `dotsync commit` and `dotsync status` can no longer disagree about
/// what changed. Which tree the bytes are then written into is a separate
/// question, answered by the caller.
async fn select_changes_to_record(
    session: &Session,
    sync_state: Option<&crate::sync::SyncState>,
    machine_scope: &str,
    options: &CommitOptions,
    target_entries: &BTreeMap<PathBuf, TreeValue>,
    internal_paths: &BTreeSet<PathBuf>,
) -> Result<Selection, DotsyncError> {
    let selection = if options.paths.is_empty() {
        None
    } else {
        Some(expand_selection_paths(
            session.paths(),
            &options.scope,
            &options.paths,
            target_entries,
            internal_paths,
        )?)
    };
    let classification = classify_home_against_scope(
        session,
        sync_state,
        machine_scope,
        &selection
            .as_ref()
            .map(SelectedPaths::everything)
            .unwrap_or_default(),
        &RecordedFromHome::default(),
    )
    .await?;

    // Naming a directory says "commit what changed under here". A bare commit
    // says the same thing about every file already on the scope, so both step
    // around the files the repo moved on without home rather than refusing the
    // run over them. A directory goes one further and picks up files nothing
    // tracks yet, which is how new config reaches a scope in bulk and why a
    // bare commit is not simply the same thing over a wider set. Naming a path
    // exactly says something stronger again about that one path, and that
    // claim is argued with below.
    let mut skipped = Vec::new();
    let selected_paths: Vec<PathBuf> = match &selection {
        None => classification
            .paths
            .iter()
            .filter(|(_, path)| path.state.is_drift())
            .map(|(relative, _)| relative.clone())
            .collect(),
        Some(selection) => {
            let mut selected = selection.named.clone();
            for relative in &selection.under_directory {
                let state = classification.state(relative);
                // `--force` is the explicit claim that home wins for what this
                // command named, so it reaches under a named directory too.
                if !options.force && !selection.named.contains(relative) && state.blocks_commit() {
                    skipped.push(SkippedCommitPath {
                        path: relative.clone(),
                        state,
                    });
                    continue;
                }
                selected.insert(relative.clone());
            }
            selected.into_iter().collect()
        }
    };
    reject_scope_graph_outside_all(session, &options.scope, target_entries, &selected_paths)
        .await?;

    let newly_tracked = selected_paths
        .iter()
        .filter(|relative| !target_entries.contains_key(*relative))
        .cloned()
        .collect::<Vec<_>>();

    if !options.force {
        let refused = selected_paths
            .iter()
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
                scope: options.scope.clone(),
                refused,
            });
        }
        return Ok(Selection {
            paths: selected_paths,
            newly_tracked,
            skipped,
            forced_paths: Vec::new(),
            forced_overwrites: Vec::new(),
        });
    }

    let forced_overwrites = selected_paths
        .iter()
        .filter(|relative| {
            let state = classification.state(relative);
            state.blocks_commit() || state == FileState::DivergedEdit
        })
        .cloned()
        .collect::<Vec<_>>();
    Ok(Selection {
        forced_paths: selected_paths.clone(),
        paths: selected_paths,
        newly_tracked,
        skipped,
        forced_overwrites,
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
/// Falls back to the scope's own head when the commit does not cascade into
/// this home: home was never derived from such a scope, so there is no version
/// of it this machine can claim to have started from.
async fn commit_merge_base_tree(
    mut_repo: &mut jj_lib::repo::MutableRepo,
    cascades_into_home: bool,
    target_scope: &str,
    target_head: &jj_lib::commit::Commit,
    last_synced: Option<&jj_lib::commit::Commit>,
) -> Result<jj_lib::merged_tree::MergedTree, DotsyncError> {
    let Some(last_synced) = last_synced else {
        return Ok(target_head.tree());
    };
    if !cascades_into_home {
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
    session: &Session,
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
        Some(value) => {
            Some(read_tree_entry_bytes(session.repo().store(), &config_path, value).await?)
        }
        None => None,
    };
    if read_home_bytes(session.paths(), &config_path)? == repo_bytes {
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
) -> Result<SelectedPaths, DotsyncError> {
    let mut selected = SelectedPaths::default();
    // Relative path of the repo root within home — reject anything under it
    let repo_relative = paths
        .repo_root
        .strip_prefix(&paths.home_dir)
        .ok()
        .map(|p| p.to_path_buf());
    // Every guard below asks "is this path inside somewhere it may not be",
    // and a symlink is a way of writing a path that is not where it looks.
    // Resolving home and the repo root once means those questions get asked
    // about the file that will actually be read, not about the string.
    let home = Canonical::of(&paths.home_dir);
    let repo_root = Canonical::of(&paths.repo_root);

    let mut rejected = Vec::new();
    for named in selection_paths {
        // `.config/fish/` and `.config/fish` name the same directory, and
        // `./x` names `x` — but dotsync records the path it is given verbatim,
        // as a repo path, which accepts neither trailing separators nor `.`
        // components. Normalising here rather than at the repo layer keeps it
        // a question about the selection, answered where the teaching messages
        // are; what gets quoted back to the user is still what they typed.
        let selection_path = &named
            .components()
            .filter(|component| !matches!(component, Component::CurDir))
            .collect::<PathBuf>();
        if let Some(problem) = unusable_commit_path(
            paths,
            &home,
            selection_path,
            internal_paths,
            repo_relative.as_deref(),
        ) {
            rejected.push(RejectedCommitPath {
                path: named.clone(),
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
                    &home,
                    &home_path,
                    &mut matched,
                    internal_paths,
                    &repo_root,
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
                path: named.clone(),
                problem: CommitPathProblem::Unmatched { home_path },
            });
            continue;
        }
        if is_directory_selection {
            selected.under_directory.extend(matched);
        } else {
            selected.named.extend(matched);
        }
    }

    // Every bad path in one answer: an agent that fixes one and reruns pays a
    // full fetch-and-commit attempt to discover the next one.
    if !rejected.is_empty() {
        return Err(DotsyncError::UnusableCommitPaths {
            scope: scope.to_string(),
            rejected,
        });
    }

    // A path named both ways — `-- .config/fish/ .config/fish/aliases.fish` —
    // was named exactly, and that is the stronger claim of the two.
    selected
        .under_directory
        .retain(|path| !selected.named.contains(path));
    Ok(selected)
}

/// What the paths a commit named resolved to, kept apart by how they were
/// named. A directory is a bulk selection and filters; a path named exactly is
/// a claim about that path, and dotsync argues with it rather than quietly
/// dropping it.
#[derive(Debug, Default)]
struct SelectedPaths {
    named: BTreeSet<PathBuf>,
    under_directory: BTreeSet<PathBuf>,
}

impl SelectedPaths {
    /// Everything the selection matched, however it was named — the set the
    /// classification has to cover.
    fn everything(&self) -> BTreeSet<PathBuf> {
        self.named.union(&self.under_directory).cloned().collect()
    }
}

/// Why dotsync will not record this path, if it will not. Runs before any
/// matching, because these paths are refused for what they are rather than
/// for what they do or do not resolve to.
fn unusable_commit_path(
    paths: &DotsyncPaths,
    home: &Canonical,
    selection_path: &Path,
    internal_paths: &BTreeSet<PathBuf>,
    repo_relative: Option<&Path>,
) -> Option<CommitPathProblem> {
    // Everything dotsync manages lives in home, so naming home itself names
    // everything — including the files that are on this machine precisely
    // because they are not shared. Checked before the absolute-path rule so
    // that both ways of saying it get the same answer.
    // Normalisation has already dropped the `.` components, so naming home
    // relatively arrives here as the empty path.
    let names_home_root = selection_path.as_os_str().is_empty() || selection_path == paths.home_dir;
    if names_home_root {
        return Some(CommitPathProblem::HomeRoot);
    }
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
    // Last, because it is the only one that touches the filesystem. A path
    // that does not resolve to itself under home went through a link on the
    // way — as the last component or as any component above it — so what
    // dotsync would read is not what was named.
    if let Some(resolves_to) = home.resolves_elsewhere(selection_path) {
        return Some(CommitPathProblem::Symlink { resolves_to });
    }
    None
}

/// A directory as the filesystem really has it, with the links followed once.
///
/// Falls back to the path as given when it cannot be resolved, which is only
/// reachable before `init` has created the repo — every guard that uses this
/// runs against paths that exist.
struct Canonical(PathBuf);

impl Canonical {
    fn of(path: &Path) -> Self {
        Self(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn contains(&self, path: &Path) -> bool {
        path.canonicalize()
            .is_ok_and(|resolved| resolved.starts_with(&self.0))
    }

    /// Where `relative` really leads, if that is not where it says.
    ///
    /// A path that does not exist yet resolves to nothing and is not a link;
    /// whether it names anything at all is a question the caller asks next.
    fn resolves_elsewhere(&self, relative: &Path) -> Option<PathBuf> {
        let resolved = self.0.join(relative).canonicalize().ok()?;
        (resolved != self.0.join(relative)).then_some(resolved)
    }
}

/// Every real file at or under `current`, as paths relative to home.
///
/// Symlinks are skipped rather than followed: `DirEntry::file_type` does not
/// follow them, so a linked directory is neither recursed into nor recorded.
/// The repo-root and internal-path guards below compare resolved paths, so
/// that no route into this walk — however the caller's path was spelled — can
/// reach dotsync's own state.
fn collect_home_directory_files(
    home_root: &Canonical,
    current: &Path,
    expanded: &mut BTreeSet<PathBuf>,
    internal_paths: &BTreeSet<PathBuf>,
    repo_root: &Canonical,
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
            if repo_root.contains(&path) {
                continue;
            }
            collect_home_directory_files(home_root, &path, expanded, internal_paths, repo_root)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        // Relative to home as the filesystem has it, so that a walk which
        // started somewhere aliased still names files the way every other part
        // of dotsync does — or names nothing, if it left home entirely.
        let Ok(resolved) = path.canonicalize() else {
            continue;
        };
        let Ok(relative) = resolved.strip_prefix(home_root.path()) else {
            continue;
        };
        let relative = relative.to_path_buf();
        if internal_paths.contains(&relative) || repo_root.contains(&path) {
            continue;
        }
        expanded.insert(relative);
    }

    Ok(())
}

fn path_has_prefix(path: &Path, prefix: &Path) -> bool {
    path == prefix || path.starts_with(prefix)
}

/// Writes home's current content for `relative` into `builder`, and returns
/// exactly the bytes it recorded — `None` when home has no such file, which is
/// how a deletion is recorded. The caller keeps those bytes as the baseline
/// for its own home sync; see `RecordedFromHome`.
async fn apply_home_path_to_tree(
    mut_repo: &mut jj_lib::repo::MutableRepo,
    paths: &DotsyncPaths,
    relative: &Path,
    builder: &mut MergedTreeBuilder,
) -> Result<Option<Vec<u8>>, DotsyncError> {
    let relative_str = relative.to_str().ok_or_else(|| DotsyncError::NonUtf8Path {
        path: relative.to_path_buf(),
    })?;
    let repo_path =
        RepoPathBuf::from_internal_string(relative_str).map_err(|err| DotsyncError::Jj {
            message: format!("invalid repo path {}: {err}", relative.display()),
        })?;

    let Some(bytes) = read_home_bytes(paths, relative)? else {
        builder.set_or_remove(repo_path, Merge::absent());
        return Ok(None);
    };

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
    Ok(Some(bytes))
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
) -> Run<Result<ContinueReport, DotsyncError>> {
    in_session(paths, async |session, paths| {
        continue_in_session(session, paths, force).await
    })
    .await
}

async fn continue_in_session(
    session: &mut Session,
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
    let graph = session.config().graph.clone();
    let repo = session.repo().clone();
    let mut tx = repo.start_transaction();
    let mut scope_heads = ScopeHeads::load_existing(tx.repo_mut().base_repo(), &graph)?;
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
    let cascades_into_home =
        scope_is_ancestor_or_self(&graph, &state.paused_scope, &state.machine_scope);
    let mut recorded_from_home = RecordedFromHome::default();
    let mut builder = MergedTreeBuilder::new(merged_tree);
    for relative in state.paused_home_contents.keys() {
        let bytes = apply_home_path_to_tree(tx.repo_mut(), paths, relative, &mut builder).await?;
        if cascades_into_home {
            recorded_from_home.record(relative, bytes);
        }
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

    session
        .advance_to(
            tx.commit("dotsync: continue cascade")
                .await
                .map_err(|err| DotsyncError::Jj {
                    message: format!("commit continued cascade: {err}"),
                })?,
        )
        .await?;
    remove_paused_cascade_state(paths)?;
    let push = push_scope_updates(session).await?;
    let sync = crate::sync::sync_repo_to_home(
        session,
        force,
        &recorded_from_home,
        Some(&state.machine_scope),
    )
    .await?;
    Ok(ContinueReport { sync, push })
}

pub async fn abort_paused_cascade(paths: &DotsyncPaths) -> Run<Result<AbortReport, DotsyncError>> {
    in_session(paths, async |session, paths| {
        abort_in_session(session, paths).await
    })
    .await
}

async fn abort_in_session(
    session: &mut Session,
    paths: &DotsyncPaths,
) -> Result<AbortReport, DotsyncError> {
    let state = load_paused_cascade_state(paths)?;
    if state.original_scope_commit_ids.is_empty() {
        return Err(DotsyncError::Jj {
            message: "paused cascade state does not include an abort checkpoint; resolve the conflict and run `dotsync continue` instead".to_string(),
        });
    }

    let repo = session.repo().clone();
    let mut tx = repo.start_transaction();
    for (scope, commit_id) in &state.original_scope_commit_ids {
        let commit = load_commit_by_hex(tx.repo_mut(), commit_id)?;
        tx.repo_mut().set_local_bookmark_target(
            RefNameBuf::from(scope.as_str()).as_ref(),
            RefTarget::normal(commit.id().clone()),
        );
    }
    session
        .advance_to(
            tx.commit("dotsync: abort cascade")
                .await
                .map_err(|err| DotsyncError::Jj {
                    message: format!("commit aborted cascade: {err}"),
                })?,
        )
        .await?;
    remove_paused_cascade_state(paths)?;

    // Abort is a full sync of home back to the machine scope's pre-pause tip,
    // not a selective restore: the home edit that started the cascade is
    // exactly what abort exists to discard, so it cannot also be a reason to
    // refuse. Drift outside the paused selection goes the same way, which is
    // what DESIGN.md's "reverts all the config files" says and what the old
    // selective restore quietly did not do.
    let sync = crate::sync::sync_repo_to_home(
        session,
        ForceScope::Everything,
        &RecordedFromHome::default(),
        Some(&state.machine_scope),
    )
    .await?;

    Ok(AbortReport {
        paused_scope: state.paused_scope,
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
