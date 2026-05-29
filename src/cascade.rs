use std::collections::{HashMap, HashSet};

use jj_lib::commit::Commit;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::{MutableRepo, ReadonlyRepo, Repo as _};
use jj_lib::rewrite::merge_commit_trees;

use crate::error::{jj_error, DotsyncError};
use crate::scope_graph::ScopeGraph;

#[derive(Debug, Clone)]
pub(crate) struct CascadeCommand {
    pub(crate) root_scope: String,
    pub(crate) description: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CascadePlan {
    steps: Vec<CascadeStep>,
}

#[derive(Debug, Clone)]
pub(crate) struct CascadeStep {
    pub(crate) scope: String,
    pub(crate) parent_scopes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CascadeProgress {
    pub(crate) completed_scopes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CascadeSuccess {
    pub(crate) progress: CascadeProgress,
}

#[derive(Debug, Clone)]
pub(crate) enum CascadeOutcome {
    Completed(CascadeSuccess),
    Paused {
        scope: String,
        conflicted_files: Vec<String>,
    },
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

async fn execute_cascade_steps(
    mut_repo: &mut MutableRepo,
    scope_heads: &mut ScopeHeads,
    steps: &[CascadeStep],
    command: &CascadeCommand,
    mut progress: CascadeProgress,
) -> Result<CascadeOutcome, DotsyncError> {
    for step in steps {
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

            return Ok(CascadeOutcome::Paused {
                scope: step.scope.clone(),
                conflicted_files,
            });
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
