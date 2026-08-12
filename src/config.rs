use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::merge::Merge;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::repo::MutableRepo;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use serde::Deserialize;
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value};

use crate::error::{jj_error, DotsyncError};
use crate::repo::{load_scope_commit, read_tree_entry_bytes};
use crate::scope_graph::ScopeGraph;

pub(crate) const DOTSYNC_CONFIG_RELATIVE_PATH: &str = ".config/dotsync/config.toml";

/// The root scope. Every machine descends from it, which is why the scope
/// graph itself is read from here and nowhere else.
pub(crate) const ALL_SCOPE: &str = "all";
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

/// What the file says about itself before it says anything about scopes.
///
/// DESIGN calls the comments in this file load-bearing: choosing a scope is
/// the one decision dotsync cannot make for an agent, and this file is where
/// both DESIGN and the dotfiles skill send it to make that decision. A
/// generated scope list with nothing to read is that mechanism producing
/// nothing.
const CONFIG_HEADER: &str = "\
# dotsync scope graph — which config reaches which machines.
#
# Every scope is a branch. A machine gets everything its own scope holds plus
# everything its ancestor scopes hold, so config on `all` reaches every machine
# and config on a machine scope reaches only that machine.
#
# Choosing a scope is the one decision dotsync cannot make for you: put each
# change on the root-est scope that should own it. These comments are how that
# choice gets made, so when you learn what a scope is for, write it down here
# and commit this file to `all`.

";

const SYNC_STATE_COMMENT: &str = "\
# Where dotsync records which machine scope this home uses. Machine-local: it
# never travels on a scope.
";

/// A scope dotsync is about to create, and enough about it to say what it is
/// for in the file it writes.
pub(crate) struct NewScope {
    pub(crate) name: String,
    pub(crate) parents: Vec<String>,
    pub(crate) kind: ScopeKind,
}

/// Why dotsync created a scope. It only ever creates these three, so it can
/// describe each one; anything a person adds later they describe themselves.
pub(crate) enum ScopeKind {
    Root,
    Os,
    Machine,
}

impl NewScope {
    /// The comment that introduces this scope, and the invitation to say more.
    /// The placeholder is deliberately empty: dotsync knows which machines a
    /// scope covers and cannot know what belongs on it.
    fn comment(&self) -> String {
        let name = &self.name;
        let what_it_covers = match self.kind {
            ScopeKind::Root => format!("`{name}` — every machine, whatever its OS."),
            ScopeKind::Os => format!("`{name}` — every machine whose OS is {name}."),
            ScopeKind::Machine => format!("`{name}` — only the machine called {name}."),
        };
        format!("# {what_it_covers}\n# What belongs here:\n")
    }
}

/// A config file for a machine joining a remote that has none: the header, the
/// scopes this machine needs, and where its sync state lives.
pub(crate) fn new_config(sync_state_relative_path: &Path, scopes: &[NewScope]) -> String {
    let mut document = DocumentMut::new();

    let mut scope_table = Table::new();
    scope_table.decor_mut().set_prefix(CONFIG_HEADER);
    document.insert("scopes", Item::Table(scope_table));

    let mut sync = Table::new();
    sync.insert(
        "state_path",
        Item::Value(Value::from(sync_state_relative_path.display().to_string())),
    );
    if let Some(mut key) = sync.key_mut("state_path") {
        key.leaf_decor_mut().set_prefix(SYNC_STATE_COMMENT);
    }
    document.insert("sync", Item::Table(sync));

    // A fresh document has a `[scopes]` table because this function just put
    // one there, so adding to it cannot fail for the reason it can in a file
    // somebody has edited.
    add_scopes(&mut document, scopes)
        .expect("a document this function just built has a `[scopes]` table")
}

/// The config file with `new_scopes` added, keeping everything it already
/// said.
///
/// Editing rather than re-rendering is the whole point: this used to serialize
/// the parsed scope graph back out, which meant the second machine to run
/// `dotsync init` deleted every comment anyone had written — including the ones
/// the first machine's init wrote. A scope graph round-trips; the guidance
/// beside it does not.
pub(crate) fn config_with_added_scopes(
    existing: &str,
    new_scopes: &[NewScope],
    config_path: &Path,
) -> Result<String, DotsyncError> {
    let mut document = existing
        .parse::<DocumentMut>()
        .map_err(|err| DotsyncError::ConfigEdit {
            path: config_path.to_path_buf(),
            message: err.to_string(),
        })?;
    add_scopes(&mut document, new_scopes).map_err(|message| DotsyncError::ConfigEdit {
        path: config_path.to_path_buf(),
        message,
    })
}

/// Appends scope entries, each under the comment that says what it covers.
///
/// The `Err` is a bare message rather than a `DotsyncError`, because the two
/// callers disagree about what it means: in an existing file it is a config
/// somebody got wrong, and in a fresh one it is impossible.
fn add_scopes(document: &mut DocumentMut, new_scopes: &[NewScope]) -> Result<String, String> {
    let scopes = document["scopes"]
        .as_table_mut()
        .ok_or_else(|| "`[scopes]` is not a table".to_string())?;
    for scope in new_scopes {
        let mut entry = InlineTable::new();
        if !scope.parents.is_empty() {
            entry.insert(
                "parents",
                Value::Array(scope.parents.iter().map(Value::from).collect()),
            );
        }
        entry.fmt();
        scopes.insert(&scope.name, Item::Value(Value::InlineTable(entry)));
        if let Some(mut key) = scopes.key_mut(&scope.name) {
            key.leaf_decor_mut()
                .set_prefix(format!("\n{}", scope.comment()));
        }
    }
    Ok(document.to_string())
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

/// Parses the scope graph out of a repo already in hand.
///
/// The repo is a parameter rather than something this function opens, because
/// it used to open one: "load the config" silently meant "re-open the repo",
/// and `load_config` is called from everywhere.
pub(crate) async fn load_config(
    paths: &DotsyncPaths,
    repo: &dyn jj_lib::repo::Repo,
) -> Result<DotsyncConfig, DotsyncError> {
    parse_config(
        &repo_config_path(paths),
        &load_config_text(paths, repo).await?,
    )
}

/// The config file as it is written, comments and all.
///
/// Read by `init` when it joins a remote, because adding this machine's scopes
/// has to keep everything the file already says — see `config_with_scopes`.
pub(crate) async fn load_config_text(
    paths: &DotsyncPaths,
    repo: &dyn jj_lib::repo::Repo,
) -> Result<String, DotsyncError> {
    let all_commit = load_scope_commit(repo, ALL_SCOPE)?;
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
    String::from_utf8(contents)
        .map_err(|err| jj_error(format!("config file is not valid utf-8: {err}")))
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
