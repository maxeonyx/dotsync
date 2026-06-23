use std::fs;
use std::path::PathBuf;

use crate::commit::{
    commit_and_sync, CommandOutcome, CommitOptions, CommitReport, CommitSelection,
};
use crate::config::{
    load_config, render_config, DotsyncConfig, DotsyncPaths, DOTSYNC_CONFIG_RELATIVE_PATH,
};
use crate::error::DotsyncError;
use crate::scope_graph::ScopeGraph;

#[derive(Debug, Clone)]
pub struct AddScopeOptions {
    pub scope: String,
    pub parents: Vec<String>,
    pub children: Vec<String>,
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct AddScopeReport {
    pub scope: String,
    pub commit: CommitReport,
}

pub async fn add_scope(
    paths: &DotsyncPaths,
    options: AddScopeOptions,
) -> Result<CommandOutcome<AddScopeReport>, DotsyncError> {
    let config = load_config(paths).await?;
    let updated_config = add_scope_to_config(config, &options)?;
    let config_path = paths.home_dir.join(DOTSYNC_CONFIG_RELATIVE_PATH);
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|source| DotsyncError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(&config_path, render_config(&updated_config)).map_err(|source| DotsyncError::Io {
        path: config_path,
        source,
    })?;

    let CommandOutcome::Success(commit) = commit_and_sync(
        paths,
        CommitOptions {
            scope: "all".to_string(),
            message: format!("add {} scope", options.scope),
            force: options.force,
            selection: CommitSelection::Paths(vec![PathBuf::from(DOTSYNC_CONFIG_RELATIVE_PATH)]),
        },
    )
    .await?;

    Ok(CommandOutcome::Success(AddScopeReport {
        scope: options.scope,
        commit,
    }))
}

fn add_scope_to_config(
    config: DotsyncConfig,
    options: &AddScopeOptions,
) -> Result<DotsyncConfig, DotsyncError> {
    let mut parents = config.graph.parents;
    if parents.contains_key(&options.scope) {
        return Err(DotsyncError::Jj {
            message: format!("scope `{}` already exists", options.scope),
        });
    }
    for parent in &options.parents {
        if !parents.contains_key(parent) {
            return Err(DotsyncError::InvalidScope {
                scope: parent.clone(),
            });
        }
    }

    parents.insert(options.scope.clone(), options.parents.clone());
    for child in &options.children {
        let child_parents = parents
            .get_mut(child)
            .ok_or_else(|| DotsyncError::InvalidScope {
                scope: child.clone(),
            })?;
        let mut inserted_between_parent_and_child = false;
        for parent in child_parents.iter_mut() {
            if options.parents.contains(parent) {
                *parent = options.scope.clone();
                inserted_between_parent_and_child = true;
            }
        }
        if !inserted_between_parent_and_child && !child_parents.contains(&options.scope) {
            child_parents.push(options.scope.clone());
        }
    }

    Ok(DotsyncConfig {
        graph: ScopeGraph::new(parents)?,
        sync_state_relative_path: config.sync_state_relative_path,
    })
}
