use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::PathBuf;

use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::{MutableRepo, ReadonlyRepo, Repo as _};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::rewrite::merge_commit_trees;
use serde::{Deserialize, Serialize};

use crate::{jj_error, DotsyncError, ScopeGraph};

#[derive(Debug, Clone)]
pub(crate) struct CascadeCommand {
    pub(crate) root_scope: String,
    pub(crate) description: String,
    pub(crate) original_scope: String,
    pub(crate) machine_scope: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CascadePlan {
    steps: Vec<CascadeStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CascadeStep {
    pub(crate) scope: String,
    pub(crate) parent_scopes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CascadeProgress {
    pub(crate) completed_scopes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CascadePause {
    pub scope: String,
    pub conflicted_files: Vec<String>,
    pub scopes_done: Vec<String>,
    pub scopes_pending: Vec<String>,
    pub original_scope: String,
    pub machine_scope: String,
    pub parent_scopes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CascadeSuccess {
    pub(crate) progress: CascadeProgress,
}

#[derive(Debug, Clone)]
pub(crate) enum CascadeOutcome {
    Completed(CascadeSuccess),
    Paused(CascadePause),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedCascadeState {
    pub(crate) original_scope: String,
    pub(crate) machine_scope: String,
    pub(crate) paused_scope: String,
    pub(crate) paused_working_copy_commit_hex: String,
    pub(crate) paused_parent_commit_hexes: Vec<String>,
    pub(crate) command_description: String,
    pub(crate) committed_scope: String,
    pub(crate) completed_scopes: Vec<String>,
    pub(crate) remaining_steps: Vec<CascadeStep>,
}

pub(crate) trait CascadeStateStore {
    fn load(&self) -> Result<Option<PersistedCascadeState>, DotsyncError>;
    fn save(&self, state: &PersistedCascadeState) -> Result<(), DotsyncError>;
    fn clear(&self) -> Result<(), DotsyncError>;
}

#[derive(Debug, Clone)]
pub(crate) struct JsonCascadeStateStore {
    path: PathBuf,
}

impl JsonCascadeStateStore {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl CascadeStateStore for JsonCascadeStateStore {
    fn load(&self) -> Result<Option<PersistedCascadeState>, DotsyncError> {
        let contents = match fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(DotsyncError::Io {
                    path: self.path.clone(),
                    source,
                })
            }
        };
        let state =
            serde_json::from_str(&contents).map_err(|source| DotsyncError::CascadeState {
                path: self.path.clone(),
                source,
            })?;
        Ok(Some(state))
    }

    fn save(&self, state: &PersistedCascadeState) -> Result<(), DotsyncError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| DotsyncError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let contents =
            serde_json::to_string_pretty(state).map_err(|source| DotsyncError::CascadeState {
                path: self.path.clone(),
                source,
            })?;
        fs::write(&self.path, contents).map_err(|source| DotsyncError::Io {
            path: self.path.clone(),
            source,
        })
    }

    fn clear(&self) -> Result<(), DotsyncError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(DotsyncError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ScopeHeads {
    heads: HashMap<String, Commit>,
}

impl ScopeHeads {
    pub(crate) fn load_existing(
        repo: &ReadonlyRepo,
        graph: &ScopeGraph,
    ) -> Result<Self, DotsyncError> {
        let mut heads = HashMap::new();
        for scope in graph.parents.keys() {
            let target = repo
                .view()
                .get_local_bookmark(RefNameBuf::from(scope.as_str()).as_ref());
            if let Some(commit_id) = target.as_normal() {
                let commit = repo
                    .store()
                    .get_commit(commit_id)
                    .map_err(|err| jj_error(format!("load scope head for {scope}: {err}")))?;
                heads.insert(scope.clone(), commit);
            }
        }
        Ok(Self { heads })
    }

    pub(crate) fn contains(&self, scope: &str) -> bool {
        self.heads.contains_key(scope)
    }

    pub(crate) fn require(&self, scope: &str) -> Result<Commit, DotsyncError> {
        self.heads
            .get(scope)
            .cloned()
            .ok_or_else(|| DotsyncError::MissingScopeBookmark {
                scope: scope.to_string(),
            })
    }

    pub(crate) fn update(&mut self, scope: impl Into<String>, commit: Commit) {
        self.heads.insert(scope.into(), commit);
    }
}

impl CascadePlan {
    pub(crate) fn from_steps(steps: Vec<CascadeStep>) -> Self {
        Self { steps }
    }

    pub(crate) fn pending_scopes(&self) -> Vec<String> {
        self.steps.iter().map(|step| step.scope.clone()).collect()
    }

    pub(crate) fn remaining_steps(&self) -> &[CascadeStep] {
        &self.steps
    }
}

pub(crate) fn build_cascade_plan(
    graph: &ScopeGraph,
    scope_heads: &ScopeHeads,
    command: &CascadeCommand,
) -> CascadePlan {
    let mut steps = Vec::new();
    for scope in descendants_in_topological_order(graph, &command.root_scope) {
        if !scope_heads.contains(&scope) {
            continue;
        }
        let parent_scopes = graph.parents[&scope]
            .iter()
            .filter(|parent| scope_heads.contains(parent))
            .cloned()
            .collect::<Vec<_>>();
        if parent_scopes.is_empty() {
            continue;
        }
        steps.push(CascadeStep {
            scope,
            parent_scopes,
        });
    }
    CascadePlan { steps }
}

pub(crate) async fn execute_cascade_plan(
    mut_repo: &mut MutableRepo,
    scope_heads: &mut ScopeHeads,
    plan: &CascadePlan,
    command: &CascadeCommand,
) -> Result<CascadeOutcome, DotsyncError> {
    execute_cascade_steps(
        mut_repo,
        scope_heads,
        plan.remaining_steps(),
        command,
        CascadeProgress::default(),
    )
    .await
}

pub(crate) async fn resume_cascade(
    mut_repo: &mut MutableRepo,
    scope_heads: &mut ScopeHeads,
    state: &PersistedCascadeState,
) -> Result<CascadeOutcome, DotsyncError> {
    let command = CascadeCommand {
        root_scope: state.committed_scope.clone(),
        description: state.command_description.clone(),
        original_scope: state.original_scope.clone(),
        machine_scope: state.machine_scope.clone(),
    };
    execute_cascade_steps(
        mut_repo,
        scope_heads,
        &state.remaining_steps,
        &command,
        CascadeProgress {
            completed_scopes: state.completed_scopes.clone(),
        },
    )
    .await
}

async fn execute_cascade_steps(
    mut_repo: &mut MutableRepo,
    scope_heads: &mut ScopeHeads,
    steps: &[CascadeStep],
    command: &CascadeCommand,
    mut progress: CascadeProgress,
) -> Result<CascadeOutcome, DotsyncError> {
    for (index, step) in steps.iter().enumerate() {
        let existing_head = scope_heads.require(&step.scope)?;
        let mut parents = vec![existing_head];
        for parent_scope in &step.parent_scopes {
            parents.push(scope_heads.require(parent_scope)?);
        }
        if parents.len() <= 1 {
            continue;
        }

        let merged_tree = merge_commit_trees(mut_repo, &parents)
            .await
            .map_err(|err| jj_error(format!("merge trees for {}: {err}", step.scope)))?;

        if merged_tree.has_conflict() {
            let conflicted_files = merged_tree
                .conflicts()
                .map(|(path, value)| {
                    value.map_err(|err| {
                        jj_error(format!("read conflict for {}: {err}", step.scope))
                    })?;
                    Ok(path.as_internal_file_string().to_string())
                })
                .collect::<Result<Vec<_>, DotsyncError>>()?;

            return Ok(CascadeOutcome::Paused(CascadePause {
                scope: step.scope.clone(),
                conflicted_files,
                scopes_done: progress.completed_scopes.clone(),
                scopes_pending: steps[index..]
                    .iter()
                    .map(|step| step.scope.clone())
                    .collect(),
                original_scope: command.original_scope.clone(),
                machine_scope: command.machine_scope.clone(),
                parent_scopes: step.parent_scopes.clone(),
            }));
        }

        let new_commit = mut_repo
            .new_commit(
                parents.iter().map(|commit| commit.id().clone()).collect(),
                merged_tree,
            )
            .set_description(&command.description)
            .write()
            .await
            .map_err(|err| jj_error(format!("write cascade commit for {}: {err}", step.scope)))?;
        mut_repo.set_local_bookmark_target(
            RefNameBuf::from(step.scope.as_str()).as_ref(),
            RefTarget::normal(new_commit.id().clone()),
        );
        scope_heads.update(step.scope.clone(), new_commit);
        progress.completed_scopes.push(step.scope.clone());
    }
    Ok(CascadeOutcome::Completed(CascadeSuccess { progress }))
}

pub(crate) fn build_paused_state(
    plan: &CascadePlan,
    pause: &CascadePause,
    command: &CascadeCommand,
    scope_heads: &ScopeHeads,
    paused_working_copy_commit_hex: String,
) -> PersistedCascadeState {
    let pause_index = plan
        .remaining_steps()
        .iter()
        .position(|step| step.scope == pause.scope)
        .expect("paused scope should exist in plan");

    PersistedCascadeState {
        original_scope: pause.original_scope.clone(),
        machine_scope: pause.machine_scope.clone(),
        paused_scope: pause.scope.clone(),
        paused_working_copy_commit_hex,
        paused_parent_commit_hexes: paused_parent_commit_ids(scope_heads, pause)
            .into_iter()
            .map(|commit_id| commit_id.hex())
            .collect(),
        command_description: command.description.clone(),
        committed_scope: command.root_scope.clone(),
        completed_scopes: pause.scopes_done.clone(),
        remaining_steps: plan.remaining_steps()[pause_index + 1..].to_vec(),
    }
}

pub(crate) async fn create_scope_head_if_missing(
    mut_repo: &mut MutableRepo,
    scope_heads: &mut ScopeHeads,
    graph: &ScopeGraph,
    scope: &str,
    description: &str,
) -> Result<(), DotsyncError> {
    if scope_heads.contains(scope) {
        return Ok(());
    }

    let parents = graph
        .parents
        .get(scope)
        .ok_or_else(|| DotsyncError::InvalidScope {
            scope: scope.to_string(),
        })?;

    let parent_commit = if let Some(first_parent) = parents.first() {
        scope_heads.require(first_parent)?
    } else {
        return Err(DotsyncError::MissingScopeBookmark {
            scope: scope.to_string(),
        });
    };

    let commit = mut_repo
        .new_commit(vec![parent_commit.id().clone()], parent_commit.tree())
        .set_description(description)
        .write()
        .await
        .map_err(|err| jj_error(format!("write new scope head for {scope}: {err}")))?;
    mut_repo.set_local_bookmark_target(
        RefNameBuf::from(scope).as_ref(),
        RefTarget::normal(commit.id().clone()),
    );
    scope_heads.update(scope.to_string(), commit);
    Ok(())
}

pub(crate) async fn commit_resolved_pause(
    mut_repo: &mut MutableRepo,
    scope_heads: &mut ScopeHeads,
    state: &PersistedCascadeState,
    resolved_tree: &jj_lib::merged_tree::MergedTree,
) -> Result<(), DotsyncError> {
    let resolved_tree = resolved_tree
        .clone()
        .resolve()
        .await
        .map_err(|err| {
            jj_error(format!(
                "resolve paused merge tree for {}: {err}",
                state.paused_scope
            ))
        })?;
    let parents = state
        .paused_parent_commit_hexes
        .iter()
        .map(|hex| load_commit_from_hex(mut_repo.base_repo(), hex))
        .collect::<Result<Vec<_>, _>>()?;

    let commit = mut_repo
        .new_commit(
            parents.iter().map(|commit| commit.id().clone()).collect(),
            resolved_tree,
        )
        .set_description(&state.command_description)
        .write()
        .await
        .map_err(|err| {
            jj_error(format!(
                "write resolved cascade commit for {}: {err}",
                state.paused_scope
            ))
        })?;
    mut_repo.set_local_bookmark_target(
        RefNameBuf::from(state.paused_scope.as_str()).as_ref(),
        RefTarget::normal(commit.id().clone()),
    );
    scope_heads.update(state.paused_scope.clone(), commit);
    Ok(())
}

fn paused_parent_commit_ids(scope_heads: &ScopeHeads, pause: &CascadePause) -> Vec<CommitId> {
    let mut parent_ids = vec![
        scope_heads
            .require(&pause.scope)
            .expect("paused scope head should exist")
            .id()
            .clone(),
    ];
    parent_ids.extend(
        pause
            .parent_scopes
            .iter()
            .map(|parent_scope| {
                scope_heads
                    .require(parent_scope)
                    .expect("paused parent scope head should exist")
                    .id()
                    .clone()
            }),
    );
    parent_ids
}

fn load_commit_from_hex(repo: &ReadonlyRepo, hex: &str) -> Result<Commit, DotsyncError> {
    let commit_id = CommitId::try_from_hex(hex)
        .ok_or_else(|| jj_error(format!("invalid persisted commit id {hex}")))?;
    repo.store()
        .get_commit(&commit_id)
        .map_err(|err| jj_error(format!("load persisted commit {hex}: {err}")))
}

pub(crate) fn pause_conflict_message(graph: &ScopeGraph, pause: &CascadePause) -> String {
    let mut message = String::new();
    message.push_str("dotsync paused while propagating a config change through the scope branches so all machines stay in sync.\n");
    message.push_str("Scopes exist because different machines and operating systems share some config and also have unique config, so dotsync keeps them as a branch DAG.\n");
    message.push_str("This paused because the same file changed differently on branches that now need to be merged.\n\n");
    message.push_str("Scope DAG:\n");
    message.push_str(&render_scope_dag(graph, pause));
    message.push_str("\n");
    message.push_str(&format!("Paused on scope: {}\n", pause.scope));
    message.push_str(&format!(
        "Merging changes from {} into {}\n",
        pause.parent_scopes.join(" + "),
        pause.scope
    ));
    message.push_str("Conflicted files:\n");
    for file in &pause.conflicted_files {
        message.push_str(&format!("- {}\n", file));
    }
    message.push_str("\nResolve by editing the conflicted files in ~/dotfiles/ to remove conflict markers and keep the desired content.\n");
    message.push_str("Then run `dotsync continue` to resume the cascade. Run `dotsync abort` to undo the cascade and return to the pre-commit state.\n");
    message.push_str("The cascade may pause again at another scope; that is normal, just resolve and continue again.\n");
    message.push_str("The scope you are resolving may be another machine's branch, which is expected. After the cascade completes, you'll be back on your machine's branch.\n");
    message.push_str("Do not run other dotsync commands while a cascade is in progress.\n");
    message
}

fn render_scope_dag(graph: &ScopeGraph, pause: &CascadePause) -> String {
    let mut scopes: Vec<_> = graph.parents.keys().cloned().collect();
    scopes.sort();
    let done: HashSet<_> = pause.scopes_done.iter().cloned().collect();
    let pending: HashSet<_> = pause.scopes_pending.iter().cloned().collect();

    let mut lines = Vec::new();
    for scope in scopes {
        let marker = if scope == pause.original_scope {
            "origin"
        } else if scope == pause.scope {
            "paused"
        } else if done.contains(&scope) {
            "done"
        } else if pending.contains(&scope) {
            "pending"
        } else {
            "idle"
        };
        let parents = graph.parents.get(&scope).cloned().unwrap_or_default();
        if parents.is_empty() {
            lines.push(format!("- [{}] {}", marker, scope));
        } else {
            lines.push(format!(
                "- [{}] {} <- {}",
                marker,
                scope,
                parents.join(", ")
            ));
        }
    }
    lines.join("\n") + "\n"
}

pub(crate) fn conflicted_files_from_tree(
    tree: &jj_lib::merged_tree::MergedTree,
) -> Result<Vec<String>, DotsyncError> {
    tree.conflicts()
        .map(|(path, value)| {
            value.map_err(|err| {
                jj_error(format!(
                    "read conflicted path {}: {err}",
                    path.as_internal_file_string()
                ))
            })?;
            Ok(path.as_internal_file_string().to_string())
        })
        .collect()
}

pub(crate) fn repo_path_strings_to_paths(
    paths: &[String],
) -> Result<Vec<RepoPathBuf>, DotsyncError> {
    paths
        .iter()
        .map(|path| {
            RepoPathBuf::from_internal_string(path)
                .map_err(|err| jj_error(format!("invalid conflicted path {path}: {err}")))
        })
        .collect()
}

fn descendants_in_topological_order(graph: &ScopeGraph, scope: &str) -> Vec<String> {
    let descendants: HashSet<String> = descendants_of(graph, scope).into_iter().collect();
    let mut remaining = descendants.clone();
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        let mut ready: Vec<String> = remaining
            .iter()
            .filter(|candidate| {
                graph.parents[*candidate]
                    .iter()
                    .all(|parent| !descendants.contains(parent) || ordered.contains(parent))
            })
            .cloned()
            .collect();
        ready.sort();
        for candidate in ready {
            remaining.remove(&candidate);
            ordered.push(candidate);
        }
    }
    ordered
}

fn descendants_of(graph: &ScopeGraph, scope: &str) -> Vec<String> {
    let mut descendants = Vec::new();
    let mut stack = graph.children.get(scope).cloned().unwrap_or_default();
    let mut seen = HashSet::new();
    while let Some(child) = stack.pop() {
        if seen.insert(child.clone()) {
            descendants.push(child.clone());
            if let Some(grandchildren) = graph.children.get(&child) {
                stack.extend(grandchildren.iter().cloned());
            }
        }
    }
    descendants
}
