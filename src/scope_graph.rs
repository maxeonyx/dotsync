use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo;

use crate::error::{jj_error, DotsyncError};

/// The scope every fleet starts from, and the only scope name dotsync chooses
/// for itself: `init` on an empty remote has nothing to hang a machine off
/// yet, so it makes this one first. Nothing else treats the name specially —
/// a root scope is a scope with no parents, whatever it is called.
pub(crate) const ROOT_SCOPE: &str = "all";

/// One scope, and where it sits.
#[derive(Debug, Clone)]
pub(crate) struct Scope {
    pub(crate) name: String,
    pub(crate) parents: Vec<String>,
    pub(crate) children: Vec<String>,
    /// What belongs on this scope, in the words of whoever created it. A name
    /// usually says it — `hyprland` holds hyprland config — and this is for
    /// when it does not.
    pub(crate) description: Option<String>,
}

impl Scope {
    /// A scope no other scope hangs off. Only a leaf can be a machine's own
    /// scope: config on a scope reaches every machine below it, so a scope
    /// with children is shared by definition.
    pub(crate) fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}

/// The DAG of scopes, alphabetical.
///
/// There is no constructor that takes a graph somebody wrote down. This is
/// derived from the repo, and the repo is what the cascade acts on, so the two
/// cannot disagree.
#[derive(Debug, Clone)]
pub(crate) struct ScopeGraph {
    scopes: BTreeMap<String, Scope>,
}

impl ScopeGraph {
    pub(crate) fn get(&self, name: &str) -> Option<&Scope> {
        self.scopes.get(name)
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.scopes.contains_key(name)
    }

    pub(crate) fn scopes(&self) -> impl Iterator<Item = &Scope> {
        self.scopes.values()
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.scopes.keys().map(String::as_str)
    }

    /// A scope with no parents. Named for the stops that have to suggest a
    /// scope to hang something off when the reader has not said: a root is
    /// the one choice that adds nothing a machine did not already share.
    pub(crate) fn a_root(&self) -> Option<&str> {
        self.scopes
            .values()
            .find(|scope| scope.parents.is_empty())
            .map(|scope| scope.name.as_str())
    }

    /// The scopes `scope` reaches, `scope` itself included, nearest first.
    pub(crate) fn ancestors_and_self(&self, scope: &str) -> Vec<&Scope> {
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(scope.to_string());
        while let Some(name) = queue.pop_front() {
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some(scope) = self.scopes.get(&name) {
                found.push(scope);
                queue.extend(scope.parents.iter().cloned());
            }
        }
        found
    }

    /// Every scope, parents before children — the order convergence runs in,
    /// so that a scope is merged after the parents whose heads it merges.
    ///
    /// Every scope rather than the descendants of one: a run converges the
    /// whole graph, because the head that moved may be one no command of
    /// this machine's touched. A change published to `all` by a machine whose
    /// own cascade never finished reaches nobody until some other machine
    /// merges it down, and a pass rooted at whatever this run committed would
    /// never look there.
    pub(crate) fn in_cascade_order(&self) -> Vec<&Scope> {
        let mut ordered: Vec<&Scope> = Vec::new();
        let mut remaining: HashSet<String> = self.scopes.keys().cloned().collect();
        while !remaining.is_empty() {
            let mut ready: Vec<String> = remaining
                .iter()
                .filter(|name| {
                    self.scopes[*name]
                        .parents
                        .iter()
                        .all(|parent| !remaining.contains(parent))
                })
                .cloned()
                .collect();
            ready.sort();
            // A DAG always has something ready, and this graph is one because
            // its edges are the ancestry of creation commits — a cycle would
            // need a commit to be its own ancestor. Asserted rather than
            // trusted, because the alternative to noticing is this loop
            // spinning for ever.
            assert!(
                !ready.is_empty(),
                "the scope graph is not a DAG: {remaining:?} have no order"
            );
            for name in ready {
                remaining.remove(&name);
                ordered.push(&self.scopes[&name]);
            }
        }
        ordered
    }

    /// How far a scope is from a root, which is the order the DAG reads in.
    pub(crate) fn depth(&self, scope: &str) -> usize {
        self.get(scope)
            .and_then(|scope| {
                scope
                    .parents
                    .iter()
                    .map(|parent| self.depth(parent) + 1)
                    .max()
            })
            .unwrap_or(0)
    }
}

/// The first line a scope's creation commit carries. Everything below it is
/// what the scope is for.
fn creation_subject(scope: &str) -> String {
    format!("dotsync: create {scope} scope")
}

/// The commit description a run writes when it creates a scope.
pub(crate) fn creation_description(scope: &str, description: Option<&str>) -> String {
    match description.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!("{}\n\n{text}", creation_subject(scope)),
        None => creation_subject(scope),
    }
}

/// The scope a commit creates, if it is a creation commit.
///
/// The verb is deliberately not spelled out. Dotsync writes `create` and has
/// written `rebuild` (the v0.3.13 recovery release) and `initialize` (every
/// `init` before this one, for the root scope), and those commits are in the
/// repos this binary is upgrading into: what history says is what dotsync has
/// to read. So the shape is what identifies one — dotsync said something
/// about a named scope — and the name is the word before `scope`.
fn scope_created_by(description: &str) -> Option<&str> {
    let said = description
        .lines()
        .next()?
        .strip_prefix("dotsync: ")?
        .strip_suffix(" scope")?;
    let (_verb, named) = said.split_once(' ')?;
    (!named.is_empty() && !named.contains(' ')).then_some(named)
}

fn description_body(description: &str) -> Option<String> {
    let body = description.split_once('\n')?.1.trim();
    (!body.is_empty()).then(|| body.to_string())
}

#[cfg(test)]
mod tests {
    use super::scope_created_by;

    /// Every description dotsync has ever written when it created a scope.
    /// Repos in the field hold all three, and the fleet migrates by upgrading
    /// the binary — so a rule that only reads the newest wording takes the
    /// root scope away from every machine that joined before it.
    #[test]
    fn a_scope_creation_is_read_however_the_release_that_wrote_it_worded_it() {
        for (description, created) in [
            ("dotsync: create linux scope", Some("linux")),
            ("dotsync: rebuild work-linux scope", Some("work-linux")),
            ("dotsync: initialize all scope", Some("all")),
            (
                "dotsync: create hyprland scope\n\nwayland compositor config",
                Some("hyprland"),
            ),
            ("dotsync: cascade from all", None),
            ("dotsync: working copy", None),
            ("dotsync: update scope config", None),
            ("Add usage function for subscription limits", None),
        ] {
            assert_eq!(
                scope_created_by(description),
                created,
                "read the wrong scope out of {description:?}"
            );
        }
    }
}

/// Where a scope was created, and what its creator said it was for.
struct Creation {
    commits: Vec<CommitId>,
    description: Option<String>,
}

/// The scope graph as the repo itself records it.
///
/// A scope is a bookmark whose own history holds the commit that created it.
/// That commit is where the edges come from too: it is written onto the heads
/// of the scope's parents, so the scopes a scope descends from are exactly the
/// ones whose creation commits its history holds, and its parents are the
/// nearest of those.
///
/// Membership is therefore structural, which is what keeps everything else on
/// the remote out. A branch pushed by a plain git client — someone's
/// experiment, a backup, a fork of one scope — was not created by dotsync, so
/// it is not in this graph, and nothing dotsync does reads it, cascades it or
/// publishes it.
///
/// The alternative dotsync shipped until now was a declaration: a
/// `config.toml` on the root scope listing the graph. Nothing compared it with
/// the repo, so writing a scope into it reported success and created no
/// bookmark, and renaming another machine's scope in it left that machine with
/// every command failing. A derived graph cannot say a scope exists when it
/// does not, and cannot be edited into a shape the repo will not honour.
pub(crate) fn derive(repo: &dyn Repo) -> Result<ScopeGraph, DotsyncError> {
    let heads: Vec<(String, Vec<CommitId>)> = repo
        .view()
        .local_bookmarks()
        .map(|(name, target)| {
            (
                name.as_str().to_string(),
                target.added_ids().cloned().collect(),
            )
        })
        .collect();

    let creations = creation_commits(repo, heads.iter().flat_map(|(_, ids)| ids.iter().cloned()))?;

    let index = repo.index();
    // Either side can be more than one commit: a contested head holds two,
    // and two machines can create one scope name at once.
    let reaches = |from: &[CommitId], to: &[CommitId]| -> Result<bool, DotsyncError> {
        for descendant in from {
            for ancestor in to {
                if index
                    .is_ancestor(ancestor, descendant)
                    .map_err(|err| jj_error(format!("read commit ancestry: {err}")))?
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    };

    // A bookmark whose creation commit its own history holds. A creation
    // commit somewhere else in the repo says nothing about this bookmark: a
    // branch force-pushed off its own history is no longer that scope.
    let mut members: Vec<(String, Vec<CommitId>)> = Vec::new();
    for (name, head_ids) in &heads {
        let Some(creation) = creations.get(name) else {
            continue;
        };
        if reaches(head_ids, &creation.commits)? {
            members.push((name.clone(), head_ids.clone()));
        }
    }

    // Creation commit to creation commit, not head to creation commit. A head
    // moves and can be merged into by anything with git — merge a scope into
    // its own parent and head-to-creation makes the two scopes each other's
    // ancestor, which is not a DAG and has no cascade order. A creation commit
    // has the ancestry it was written with for ever, and one of two commits is
    // always older, so this relation cannot come back around.
    let mut reached: HashMap<String, Vec<String>> = HashMap::new();
    for (name, _) in &members {
        let mut found = Vec::new();
        for (other, _) in &members {
            if other == name {
                continue;
            }
            if reaches(&creations[name].commits, &creations[other].commits)? {
                found.push(other.clone());
            }
        }
        reached.insert(name.clone(), found);
    }

    // Parents are the scopes nothing else in reach stands in front of. An
    // edge to a scope that a nearer one already covers says nothing the
    // cascade does not already do, so the graph does not carry it.
    let mut scopes: BTreeMap<String, Scope> = BTreeMap::new();
    for (name, _) in &members {
        let ancestors = &reached[name];
        let parents: Vec<String> = ancestors
            .iter()
            .filter(|candidate| {
                !ancestors
                    .iter()
                    .any(|nearer| nearer != *candidate && reached[nearer].contains(candidate))
            })
            .cloned()
            .collect();
        scopes.insert(
            name.clone(),
            Scope {
                name: name.clone(),
                parents,
                children: Vec::new(),
                description: creations[name].description.clone(),
            },
        );
    }

    let edges: Vec<(String, String)> = scopes
        .values()
        .flat_map(|scope| {
            scope
                .parents
                .iter()
                .map(|parent| (parent.clone(), scope.name.clone()))
        })
        .collect();
    for (parent, child) in edges {
        scopes
            .get_mut(&parent)
            .expect("a parent is a scope in this graph")
            .children
            .push(child);
    }
    for scope in scopes.values_mut() {
        scope.children.sort();
    }

    Ok(ScopeGraph { scopes })
}

/// Every creation commit reachable from the given heads.
fn creation_commits(
    repo: &dyn Repo,
    heads: impl Iterator<Item = CommitId>,
) -> Result<HashMap<String, Creation>, DotsyncError> {
    let mut creations: HashMap<String, Creation> = HashMap::new();
    let mut seen: HashSet<CommitId> = HashSet::new();
    let mut queue: VecDeque<CommitId> = heads.collect();
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let commit = repo
            .store()
            .get_commit(&id)
            .map_err(|err| jj_error(format!("read commit {}: {err}", id.hex())))?;
        if let Some(created) = scope_created_by(commit.description()) {
            let creation = creations
                .entry(created.to_string())
                .or_insert_with(|| Creation {
                    commits: Vec::new(),
                    description: None,
                });
            creation.commits.push(id.clone());
            // Two machines can create one scope name at once, and then it has
            // two creation commits. Whichever description is read first is as
            // good an answer as the other; both describe the same scope.
            creation.description = creation
                .description
                .take()
                .or_else(|| description_body(commit.description()));
        }
        queue.extend(commit.parent_ids().iter().cloned());
    }
    Ok(creations)
}
