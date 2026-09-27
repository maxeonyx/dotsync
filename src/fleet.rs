//! The fleet as it stands: what every scope holds, and how that relates to what
//! it inherits.
//!
//! A scope's tree is already its effective config — the cascade merged its
//! parents into it — so the question an agent cannot answer from a tree alone
//! is which of it is the scope's *own*. That is the difference between the
//! scope's tree and the merge of its parents' trees, path by path, and it is
//! the one relation every read and every placement write is phrased in:
//!
//! - **inherited** — the scope holds exactly what its parents give it;
//! - **added** — the scope holds a path its parents do not;
//! - **overridden** — the scope holds its own version of a path its parents
//!   hold;
//! - **removed** — the scope does not hold a path its parents do.
//!
//! Where an inherited version comes from follows: the nearest scopes above
//! whose own version it is. So "which scope owns this file", "what does this
//! machine hold that nothing shares", "which machines hold identical copies"
//! and "why does this machine's file look like this" are all readings of one
//! table rather than four commands' worth of graph walking.
//!
//! Every reading is of the fleet as the next writing run would leave it. A
//! parent that has moved and not yet been merged down would otherwise read as
//! the child overriding everything the parent changed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use jj_lib::backend::TreeValue;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo;

use crate::converge;
use crate::error::DotsyncError;
use crate::repo::{managed_tree_entries, scope_head_tree};
use crate::scope_graph::ScopeGraph;
use crate::session::Session;

/// How a scope's version of a path relates to what the scope inherits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Inherited,
    Added,
    Overridden,
    Removed,
}

impl Standing {
    pub fn code(self) -> &'static str {
        match self {
            Self::Inherited => "inherited",
            Self::Added => "added",
            Self::Overridden => "overridden",
            Self::Removed => "removed",
        }
    }

    /// Whether this is the scope's own doing, rather than its parents'.
    pub fn is_own(self) -> bool {
        self != Self::Inherited
    }
}

/// What kind of thing a scope holds at a path. Two scopes holding the same
/// bytes as different kinds hold different config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Executable,
    Symlink,
    /// The scope's two heads disagree here and nothing has merged them yet.
    Conflicted,
}

impl EntryKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Executable => "executable",
            Self::Symlink => "symlink",
            Self::Conflicted => "conflicted",
        }
    }

    pub(crate) fn of(value: Option<&TreeValue>) -> Self {
        match value {
            Some(TreeValue::File {
                executable: true, ..
            }) => Self::Executable,
            Some(TreeValue::Symlink(_)) => Self::Symlink,
            Some(_) => Self::File,
            None => Self::Conflicted,
        }
    }
}

/// One scope's standing at one path.
#[derive(Debug, Clone)]
pub struct FileRow {
    pub scope: String,
    pub path: PathBuf,
    pub standing: Standing,
    /// What the scope holds there. `None` for a removal, which holds nothing.
    pub kind: Option<EntryKind>,
    /// An id equal across scopes exactly when kind and content are, so
    /// identical copies are visible without reading every version.
    pub content: Option<String>,
    /// The scopes whose own version this is: the scope itself for anything it
    /// added, overrode or removed, and the nearest contributing scopes above it
    /// for anything it inherits.
    pub origin: Vec<String>,
}

/// A scope, where it sits, and what reaches it.
#[derive(Debug, Clone)]
pub struct ScopeInfo {
    pub name: String,
    pub parents: Vec<String>,
    pub children: Vec<String>,
    /// The machines a change to this scope reaches: the leaves below it, or
    /// the scope itself when it is one.
    pub machines: Vec<String>,
    /// What the scope is for, in the words of whoever created it.
    pub description: Option<String>,
}

impl ScopeInfo {
    pub fn is_machine(&self) -> bool {
        self.children.is_empty()
    }
}

type Entries = BTreeMap<PathBuf, Option<TreeValue>>;

/// Every scope's tree, the tree it inherits, and the table relating them.
pub(crate) struct Fleet {
    scopes: Vec<ScopeInfo>,
    holds: BTreeMap<String, (MergedTree, Entries)>,
    inherits: BTreeMap<String, (MergedTree, Entries)>,
    rows: Vec<FileRow>,
}

impl Fleet {
    /// The fleet as the next writing run would leave it.
    ///
    /// The pass runs in a transaction nothing commits — the same prediction
    /// the read-only commands already make to say whether a merge is waiting —
    /// so reading cannot describe a scope differently from the run that
    /// follows. Where the pass would stop, the scopes at and below the stop
    /// read as they are now.
    pub(crate) async fn predicted(session: &Session) -> Result<Self, DotsyncError> {
        let mut tx = session.repo().start_transaction();
        converge::pass(
            &mut tx,
            session.graph(),
            session.machine_scope(),
            None,
            None,
        )
        .await?;
        Self::read(tx.repo(), session.graph()).await
    }

    /// The fleet exactly as `repo` holds it.
    pub(crate) async fn read(repo: &dyn Repo, graph: &ScopeGraph) -> Result<Self, DotsyncError> {
        let mut holds = BTreeMap::new();
        let mut inherits = BTreeMap::new();
        let mut rows: Vec<FileRow> = Vec::new();
        let mut ordered = Vec::new();

        for scope in graph.in_cascade_order() {
            let Some(tree) = scope_head_tree(repo, &scope.name).await? else {
                continue;
            };
            let inherited = converge::inherited_tree(repo, graph, &scope.name).await?;
            let held = managed_tree_entries(&tree)?;
            let from_parents = managed_tree_entries(&inherited)?;

            let paths: BTreeSet<&PathBuf> = held.keys().chain(from_parents.keys()).collect();
            for path in paths {
                let standing = match (held.get(path), from_parents.get(path)) {
                    (Some(here), Some(above)) if here == above => Standing::Inherited,
                    (Some(_), Some(_)) => Standing::Overridden,
                    (Some(_), None) => Standing::Added,
                    (None, Some(_)) => Standing::Removed,
                    (None, None) => unreachable!("the path came from one of the two"),
                };
                let value = held.get(path);
                let origin = match standing {
                    Standing::Inherited => {
                        let mut origin = BTreeSet::new();
                        for parent in &scope.parents {
                            if let Some(row) = rows
                                .iter()
                                .find(|row| &row.scope == parent && &row.path == path)
                                .filter(|row| row.standing != Standing::Removed)
                            {
                                origin.extend(row.origin.iter().cloned());
                            }
                        }
                        origin.into_iter().collect()
                    }
                    _ => vec![scope.name.clone()],
                };
                rows.push(FileRow {
                    scope: scope.name.clone(),
                    path: path.clone(),
                    standing,
                    kind: value.map(|value| EntryKind::of(value.as_ref())),
                    content: value.and_then(|value| value.as_ref().map(content_id)),
                    origin,
                });
            }

            ordered.push(scope.name.clone());
            holds.insert(scope.name.clone(), (tree, held));
            inherits.insert(scope.name.clone(), (inherited, from_parents));
        }

        let scopes = ordered
            .iter()
            .filter_map(|name| graph.get(name))
            .map(|scope| ScopeInfo {
                name: scope.name.clone(),
                parents: scope.parents.clone(),
                children: scope.children.clone(),
                machines: machines_below(graph, &scope.name),
                description: scope.description.clone(),
            })
            .collect();

        Ok(Self {
            scopes,
            holds,
            inherits,
            rows,
        })
    }

    /// Every scope, parents before children.
    pub(crate) fn scopes(&self) -> &[ScopeInfo] {
        &self.scopes
    }

    pub(crate) fn rows(&self) -> &[FileRow] {
        &self.rows
    }

    pub(crate) fn row(&self, scope: &str, path: &Path) -> Option<&FileRow> {
        self.rows
            .iter()
            .find(|row| row.scope == scope && row.path == path)
    }

    /// What a scope holds, as a tree and path by path.
    pub(crate) fn holds(&self, scope: &str) -> Option<&(MergedTree, Entries)> {
        self.holds.get(scope)
    }

    /// What a scope's parents give it, as a tree and path by path.
    pub(crate) fn inherits(&self, scope: &str) -> Option<&(MergedTree, Entries)> {
        self.inherits.get(scope)
    }
}

/// A stable id for one kind of content: equal exactly when kind and bytes are.
fn content_id(value: &TreeValue) -> String {
    let (kind, hex) = match value {
        TreeValue::File {
            id,
            executable: true,
            ..
        } => ("exec", id.hex()),
        TreeValue::File { id, .. } => ("file", id.hex()),
        TreeValue::Symlink(id) => ("link", id.hex()),
        TreeValue::Tree(id) => ("tree", id.hex()),
        TreeValue::GitSubmodule(id) => ("submodule", id.hex()),
    };
    format!("{kind}:{}", &hex[..hex.len().min(12)])
}

/// The machines a scope reaches: the leaves at or below it.
fn machines_below(graph: &ScopeGraph, scope: &str) -> Vec<String> {
    let mut machines = BTreeSet::new();
    let mut queue = vec![scope.to_string()];
    let mut seen = BTreeSet::new();
    while let Some(name) = queue.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(found) = graph.get(&name) else {
            continue;
        };
        if found.is_leaf() {
            machines.insert(name);
        } else {
            queue.extend(found.children.iter().cloned());
        }
    }
    machines.into_iter().collect()
}

/// What a write did to one machine's config.
#[derive(Debug, Clone)]
pub struct MachineEffect {
    pub machine: String,
    pub changes: Vec<PathChange>,
}

#[derive(Debug, Clone)]
pub struct PathChange {
    pub path: PathBuf,
    pub change: Change,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Added,
    Modified,
    Removed,
}

impl Change {
    pub fn code(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Removed => "removed",
        }
    }
}

/// Every machine's config, path by path.
pub(crate) async fn machine_entries(
    repo: &dyn Repo,
    graph: &ScopeGraph,
) -> Result<BTreeMap<String, Entries>, DotsyncError> {
    let mut machines = BTreeMap::new();
    for scope in graph.scopes().filter(|scope| scope.is_leaf()) {
        if let Some(tree) = scope_head_tree(repo, &scope.name).await? {
            machines.insert(scope.name.clone(), managed_tree_entries(&tree)?);
        }
    }
    Ok(machines)
}

/// What a write changes on each machine: every machine's config before and
/// after it, compared.
///
/// This machine is the one machine whose home dotsync can see, so for it a
/// change is only one if home does not already hold the result — a commit
/// from home changes the scopes, and changes nothing in the home it came from.
pub(crate) fn effect(
    before: &BTreeMap<String, Entries>,
    after: &BTreeMap<String, Entries>,
    this_machine: Option<(&str, &BTreeMap<PathBuf, TreeValue>)>,
) -> Vec<MachineEffect> {
    let empty = Entries::new();
    let mut effects = Vec::new();
    for (machine, now) in after {
        let was = before.get(machine).unwrap_or(&empty);
        let paths: BTreeSet<&PathBuf> = was.keys().chain(now.keys()).collect();
        let mut changes = Vec::new();
        for path in paths {
            let (old, new) = (was.get(path), now.get(path));
            if old == new {
                continue;
            }
            if let Some((_, home)) = this_machine.filter(|(name, _)| name == machine) {
                if home.get(path) == new.and_then(Option::as_ref) {
                    continue;
                }
            }
            changes.push(PathChange {
                path: path.clone(),
                change: match (old, new) {
                    (None, _) => Change::Added,
                    (_, None) => Change::Removed,
                    _ => Change::Modified,
                },
            });
        }
        if !changes.is_empty() {
            effects.push(MachineEffect {
                machine: machine.clone(),
                changes,
            });
        }
    }
    effects
}
