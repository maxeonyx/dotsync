use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::merge::Merge;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::repo::Repo as _;
use jj_lib::repo::MutableRepo;
use jj_lib::repo_path::RepoPathBuf;
use serde::Deserialize;

use crate::error::{jj_error, DotsyncError};
use crate::repo::{load_repo_direct, load_scope_commit, read_tree_entry_bytes};
use crate::scope_graph::{scope_depth, ScopeGraph};

pub(crate) const DOTSYNC_CONFIG_RELATIVE_PATH: &str = ".config/dotsync/config.toml";
pub(crate) const DEFAULT_SYNC_STATE_RELATIVE_PATH: &str = ".config/dotsync/sync-state.json";

#[derive(Debug, Clone)]
pub struct DotsyncPaths {
    pub repo_root: PathBuf,
    pub home_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawConfig {
    scopes: HashMap<String, RawScope>,
    #[serde(default)]
    sync: RawSyncConfig,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawScope {
    #[serde(default)]
    parents: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RawSyncConfig {
    #[serde(default = "default_sync_state_relative_path")]
    state_path: String,
}

impl Default for RawSyncConfig {
    fn default() -> Self {
        Self {
            state_path: default_sync_state_relative_path(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DotsyncConfig {
    pub(crate) graph: ScopeGraph,
    pub(crate) sync_state_relative_path: PathBuf,
}

pub(crate) fn render_config(config: &DotsyncConfig) -> String {
    let graph = &config.graph;
    let mut scopes: Vec<String> = graph.parents.keys().cloned().collect();
    let mut memo = HashMap::new();
    scopes.sort_by(|a, b| {
        let depth_a = scope_depth(graph, a, &mut memo).unwrap_or(usize::MAX);
        let depth_b = scope_depth(graph, b, &mut memo).unwrap_or(usize::MAX);
        depth_a.cmp(&depth_b).then_with(|| a.cmp(b))
    });

    let mut rendered = String::from("[scopes]\n");
    for scope in scopes {
        let parents = &graph.parents[&scope];
        if parents.is_empty() {
            rendered.push_str(&format!("{scope} = {{}}\n"));
        } else {
            let parents = parents
                .iter()
                .map(|parent| format!("\"{parent}\""))
                .collect::<Vec<_>>()
                .join(", ");
            rendered.push_str(&format!("{scope} = {{ parents = [{parents}] }}\n"));
        }
    }
    rendered.push_str("\n[sync]\n");
    rendered.push_str(&format!(
        "state_path = \"{}\"\n",
        config.sync_state_relative_path.display()
    ));
    rendered
}

pub(crate) async fn write_config(
    mut_repo: &mut MutableRepo,
    parent_tree: &jj_lib::merged_tree::MergedTree,
    contents: &str,
) -> Result<jj_lib::merged_tree::MergedTree, DotsyncError> {
    let path = RepoPathBuf::from_internal_string(DOTSYNC_CONFIG_RELATIVE_PATH)
        .map_err(|err| jj_error(format!("invalid config repo path: {err}")))?;
    let mut reader = contents.as_bytes();
    let file_id = mut_repo
        .store()
        .write_file(path.as_ref(), &mut reader)
        .await
        .map_err(|err| jj_error(format!("write config file to repo store: {err}")))?;

    let mut builder = MergedTreeBuilder::new(parent_tree.clone());
    builder.set_or_remove(
        path,
        Merge::normal(TreeValue::File {
            id: file_id,
            executable: false,
            copy_id: CopyId::placeholder(),
        }),
    );
    builder
        .write_tree()
        .await
        .map_err(|err| jj_error(format!("write config tree: {err}")))
}

pub(crate) async fn load_config(paths: &DotsyncPaths) -> Result<DotsyncConfig, DotsyncError> {
    let repo = load_repo_direct(paths).await?;
    let all_commit = load_scope_commit(repo.as_ref(), "all")?;
    let repo_path = jj_lib::repo_path::RepoPath::from_internal_string(DOTSYNC_CONFIG_RELATIVE_PATH)
        .map_err(|err| jj_error(format!("invalid config repo path: {err}")))?;
    let value = all_commit
        .tree()
        .path_value(repo_path)
        .map_err(|err| jj_error(format!("read config tree entry: {err}")))?;
    let value = value
        .into_resolved()
        .map_err(|conflict| jj_error(format!("config path is conflicted on all: {conflict:?}")))?
        .ok_or_else(|| DotsyncError::Io {
            path: repo_config_path(paths),
            source: io::Error::new(io::ErrorKind::NotFound, "config missing on all scope"),
        })?;
    let contents = read_tree_entry_bytes(
        repo.store(),
        Path::new(DOTSYNC_CONFIG_RELATIVE_PATH),
        &value,
    )
    .await?;
    let contents = String::from_utf8(contents)
        .map_err(|err| jj_error(format!("config file is not valid utf-8: {err}")))?;
    parse_config(&repo_config_path(paths), &contents)
}

pub(crate) fn parse_config(path: &Path, contents: &str) -> Result<DotsyncConfig, DotsyncError> {
    let raw: RawConfig = toml::from_str(contents).map_err(|source| DotsyncError::ConfigParse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(DotsyncConfig {
        graph: ScopeGraph::new(
            raw.scopes
                .into_iter()
                .map(|(name, scope)| (name, scope.parents))
                .collect(),
        )?,
        sync_state_relative_path: PathBuf::from(raw.sync.state_path),
    })
}

pub(crate) fn internal_repo_paths(config: &DotsyncConfig) -> BTreeSet<PathBuf> {
    BTreeSet::from([config.sync_state_relative_path.clone()])
}

pub(crate) fn repo_config_path(paths: &DotsyncPaths) -> PathBuf {
    paths.repo_root.join(DOTSYNC_CONFIG_RELATIVE_PATH)
}

pub(crate) fn default_sync_state_relative_path() -> String {
    DEFAULT_SYNC_STATE_RELATIVE_PATH.to_string()
}
