//! Which paths a commit records, and whether it is allowed to.
//!
//! Three questions, in order. Is this a path dotsync may record at all — a path
//! is refused for what it *is* (home itself, an absolute path, dotsync's own
//! repo) before anything looks at what it holds. What does it resolve to — a
//! named directory expands to the files and links under it, and a path named
//! exactly resolves to itself. And does home hold a change of this machine's
//! own there, which is the same three-way classification `status` reports, so a
//! bare `dotsync commit` and `dotsync status` cannot disagree about what
//! changed.
//!
//! The difference between the two ways of naming a path is the whole of what
//! naming one exactly buys you: a bulk selection filters and says what it left
//! out, and a path named exactly is argued with.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use jj_lib::backend::TreeValue;
use jj_lib::repo::Repo as _;

use crate::commit::CommitOptions;
use crate::config::{DotsyncPaths, ALL_SCOPE, DOTSYNC_CONFIG_RELATIVE_PATH};
use crate::drift::{read_entry_bytes, state_of, FileState};
use crate::error::{
    CommitPathProblem, DotsyncError, RefusedCommitPath, RejectedCommitPath, SkipReason,
    SkippedCommitPath,
};
use crate::home::{repo_path_of, Home};
use crate::repo::{collect_managed_tree_entries, load_scope_commit, read_tree_entry_bytes};
use crate::session::Session;
use crate::sync::classify_home_against_head;

/// What this commit will record, and on whose authority.
pub(crate) struct Selection {
    pub(crate) paths: Vec<PathBuf>,
    /// The selected paths the target scope does not have yet. Reported because
    /// putting a file on a shared scope is the one thing a commit does that
    /// every machine on that scope then has written into its home directory.
    pub(crate) newly_tracked: Vec<PathBuf>,
    /// Paths a named directory expanded to that this commit left alone,
    /// because home holds no change of this machine's own at them. Reported,
    /// never silent: a bulk selection that quietly recorded less than it
    /// matched would read to an agent as a complete commit.
    pub(crate) skipped: Vec<SkippedCommitPath>,
    /// The paths `--force` covers. These skip the merge below entirely: the
    /// point of forcing is that home wins here whatever the repo says.
    pub(crate) forced_paths: Vec<PathBuf>,
    /// The forced paths where that authority actually decided something —
    /// where, without it, the commit would have been refused or would have
    /// merged rather than overwritten.
    pub(crate) forced_overwrites: Vec<PathBuf>,
}

/// Decides which paths a commit records and whether it is allowed to.
///
/// Whether a home file holds a change of *this machine's own* is a question
/// about this machine, not about the scope the change is headed for, so it is
/// the same three-way classification `status` reports — which is also why a
/// bare `dotsync commit` and `dotsync status` can no longer disagree about
/// what changed. Which tree the bytes are then written into is a separate
/// question, answered by the caller.
pub(crate) async fn select_changes_to_record(
    session: &mut Session,
    home: &mut Home,
    machine_scope: &str,
    options: &CommitOptions,
    target_entries: &BTreeMap<PathBuf, TreeValue>,
) -> Result<Selection, DotsyncError> {
    let selection = if options.paths.is_empty() {
        None
    } else {
        Some(expand_selection_paths(
            session.paths(),
            &options.scope,
            &options.paths,
            target_entries,
        )?)
    };
    // A named path may be one nothing knows about yet — a new config file — so
    // home is read at it before anything asks what it holds. A bare commit
    // names nothing and adds nothing, so it widens nothing.
    if let Some(selection) = &selection {
        let named = selection
            .everything()
            .iter()
            .map(|relative| repo_path_of(relative))
            .collect::<Result<Vec<_>, DotsyncError>>()?;
        home.observe_paths(session, named).await?;
    }
    let machine_head = load_scope_commit(session.repo().as_ref(), machine_scope)?;
    let classified = classify_home_against_head(session, home, &machine_head).await?;

    // Naming a directory says "commit what changed under here". A bare commit
    // says the same thing about every file already on the scope, so both step
    // around the files the repo moved on without home rather than refusing the
    // run over them. A directory goes one further and picks up files nothing
    // tracks yet, which is how new config reaches a scope in bulk and why a
    // bare commit is not simply the same thing over a wider set. Naming a path
    // exactly says something stronger again about that one path, and that
    // claim is argued with below.
    let mut skipped = selection
        .as_ref()
        .map(|selection| selection.skipped.clone())
        .unwrap_or_default();
    let selected_paths: Vec<PathBuf> = match &selection {
        None => classified
            .iter()
            .filter(|(_, path)| path.state.is_drift())
            .map(|(relative, _)| relative.clone())
            .collect(),
        Some(selection) => {
            let mut selected = selection.named.clone();
            for relative in &selection.under_directory {
                let state = state_of(&classified, relative);
                // `--force` is the explicit claim that home wins for what this
                // command named, so it reaches under a named directory too.
                if !options.force && !selection.named.contains(relative) && state.blocks_commit() {
                    skipped.push(SkippedCommitPath {
                        path: relative.clone(),
                        reason: SkipReason::NotChangedHere(state),
                    });
                    continue;
                }
                selected.insert(relative.clone());
            }
            selected.into_iter().collect()
        }
    };
    reject_scope_graph_outside_all(
        session,
        home,
        &options.scope,
        target_entries,
        &selected_paths,
    )
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
                let state = state_of(&classified, relative);
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
            let state = state_of(&classified, relative);
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
pub(crate) fn load_scope_entries(
    repo: &dyn jj_lib::repo::Repo,
    scope: &str,
) -> Result<BTreeMap<PathBuf, TreeValue>, DotsyncError> {
    let commit = load_scope_commit(repo, scope)?;
    collect_managed_tree_entries(&commit.tree())
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
    home: &Home,
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
    let home_value = home.entry(&config_path)?.as_resolved().cloned().flatten();
    if read_entry_bytes(session.repo().store(), &config_path, home_value.as_ref()).await?
        == repo_bytes
    {
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
        if let Some(problem) = unusable_commit_path(paths, selection_path, repo_relative.as_deref())
        {
            rejected.push(RejectedCommitPath {
                path: named.clone(),
                problem,
            });
            continue;
        }

        let home_path = paths.home_dir.join(selection_path);
        // `symlink_metadata`, not `is_dir`: a link to a directory is one entry
        // whose content is its target, and walking it would record the linked
        // directory's files under this machine's names. That is how `selflink
        // -> $HOME` swept all of home onto a scope.
        let is_directory_selection = std::fs::symlink_metadata(&home_path)
            .is_ok_and(|metadata| metadata.is_dir())
            || target_entries.keys().any(|candidate| {
                candidate != selection_path && path_has_prefix(candidate, selection_path)
            });
        let mut matched = BTreeSet::new();
        if is_directory_selection {
            if home_path.exists() {
                let mut walk = DirectoryWalk {
                    home_root: &home,
                    repo_root: &repo_root,
                    matched: BTreeSet::new(),
                    skipped: Vec::new(),
                };
                walk.walk(&home_path)?;
                matched.extend(walk.matched);
                selected.skipped.extend(walk.skipped);
            }
            matched.extend(
                target_entries
                    .keys()
                    .filter(|candidate| path_has_prefix(candidate, selection_path))
                    .cloned(),
            );
        } else if std::fs::symlink_metadata(&home_path).is_ok()
            || target_entries.contains_key(selection_path)
        {
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
    /// Paths a named directory matched that dotsync cannot record whatever
    /// their content says — links and things that are not files. The rest of
    /// the skipping is decided later, by the classification.
    pub(crate) skipped: Vec<SkippedCommitPath>,
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
    selection_path: &Path,
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
}

/// Walks a named directory: every file and link under it, and why it left the
/// rest.
///
/// Symlinks are entries, never paths to follow: `DirEntry::file_type` does not
/// follow them, so a linked directory is recorded as the one link it is rather
/// than recursed into — which is what keeps `selflink -> $HOME` from sweeping
/// home onto a scope, structurally rather than by a guard.
///
/// The repo-root and internal-path guards compare resolved paths, so that no
/// route into this walk — however the caller's path was spelled — can reach
/// dotsync's own state. Those two are the deliberate silences: they are never
/// committable by any spelling, and naming them exactly is refused out loud.
struct DirectoryWalk<'a> {
    home_root: &'a Canonical,
    repo_root: &'a Canonical,
    matched: BTreeSet<PathBuf>,
    pub(crate) skipped: Vec<SkippedCommitPath>,
}

impl DirectoryWalk<'_> {
    fn walk(&mut self, current: &Path) -> Result<(), DotsyncError> {
        // Once per directory rather than once per entry, and for links as well
        // as files: a link cannot be resolved to learn its own name, because
        // resolving it is exactly what dotsync must not do here.
        let resolved_dir = current.canonicalize().map_err(|source| DotsyncError::Io {
            path: current.to_path_buf(),
            source,
        })?;

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
                if self.repo_root.contains(&path) {
                    continue;
                }
                self.walk(&path)?;
                continue;
            }

            // Relative to home as the filesystem has it, so that a walk which
            // started somewhere aliased still names files the way every other
            // part of dotsync does — or names nothing, if it left home
            // entirely.
            let Ok(relative) = resolved_dir
                .join(entry.file_name())
                .strip_prefix(self.home_root.path())
                .map(Path::to_path_buf)
            else {
                continue;
            };
            if self.repo_root.contains(&path) {
                continue;
            }

            if !file_type.is_file() && !file_type.is_symlink() {
                self.skipped.push(SkippedCommitPath {
                    path: relative,
                    reason: SkipReason::NotARegularFile,
                });
                continue;
            }
            self.matched.insert(relative);
        }

        Ok(())
    }
}

fn path_has_prefix(path: &Path, prefix: &Path) -> bool {
    path == prefix || path.starts_with(prefix)
}
