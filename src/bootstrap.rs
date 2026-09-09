use std::sync::Arc;

use jj_lib::backend::Signature;
use jj_lib::commit::Commit;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::{MutableRepo, ReadonlyRepo, Repo as _};
use jj_lib::rewrite::merge_commit_trees;
use jj_lib::workspace::Workspace;

use crate::error::{jj_error, DotsyncError};
use crate::home::Home;
use crate::machine::{detect_machine, machine_signature, MachineIdentity};
use crate::paths::DotsyncPaths;
use crate::repo::{
    add_origin_remote, default_settings, fetch_origin, load_repo_direct, push_scope_updates,
    scope_head, scope_head_commit, PushReport,
};
use crate::scope_graph::{self, creation_description, ScopeGraph, ROOT_SCOPE};
use crate::session::{in_session, Run, Session};
use crate::sync::{finishing, SyncReport};

#[derive(Debug, Clone)]
pub struct InitReport {
    /// Includes the machine scope this init settled on, as `sync.current_scope`.
    pub sync: SyncReport,
    pub push: PushReport,
}

/// What `create-scope` did. The parents are echoed because they are the whole
/// of what a scope is beyond its name.
#[derive(Debug, Clone)]
pub struct CreatedScope {
    pub scope: String,
    pub parents: Vec<String>,
    pub push: PushReport,
}

/// Unlike every other command, `init` cannot carry on against a last-fetched
/// state, because there isn't one yet — so its run never reports an unreachable
/// remote as an aside. It reports it as the error it is.
pub async fn init(
    paths: &DotsyncPaths,
    remote_url: &str,
    parents: &[String],
) -> Run<Result<InitReport, DotsyncError>> {
    Run {
        report: init_repo(paths, remote_url, parents).await,
        unreachable_remote: None,
    }
}

async fn init_repo(
    paths: &DotsyncPaths,
    remote_url: &str,
    parents: &[String],
) -> Result<InitReport, DotsyncError> {
    if paths.repo_root.exists() {
        return Err(DotsyncError::RepoAlreadyExists {
            path: paths.repo_root.clone(),
        });
    }

    match create_repo_and_join(paths, remote_url, parents).await {
        Ok(report) => Ok(report),
        // Everything under the repo root was made by this run — init refuses
        // to start when it already exists — so an init that stopped part-way
        // takes its own leavings with it. Otherwise the remedy for the
        // commonest failure there is, a remote this machine cannot reach yet,
        // would be deleting a directory by hand before the retry is even
        // allowed to start.
        Err(error) => Err(match std::fs::remove_dir_all(&paths.repo_root) {
            Ok(()) => error,
            // Nothing was created yet, so there is nothing to say.
            Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => error,
            // A cleanup that failed silently would be the worst of both: the
            // retry refuses to start and nothing ever said why.
            Err(source) => DotsyncError::PartialInitLeftBehind {
                path: paths.repo_root.clone(),
                source,
                original: Box::new(error),
            },
        }),
    }
}

async fn create_repo_and_join(
    paths: &DotsyncPaths,
    remote_url: &str,
    parents: &[String],
) -> Result<InitReport, DotsyncError> {
    if let Some(parent) = paths.repo_root.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DotsyncError::Io {
            doing: "create",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::create_dir_all(&paths.repo_root).map_err(|source| DotsyncError::Io {
        doing: "create",
        path: paths.repo_root.clone(),
        source,
    })?;

    let settings = default_settings()?;
    let (_workspace, repo) = Workspace::init_internal_git(&settings, &paths.repo_root)
        .await
        .map_err(|err| jj_error(format!("init repo: {err}")))?;
    let _repo = add_origin_remote(repo, remote_url).await?;
    // The remote lives in the git config rather than in the repo view, and a
    // repo handle carries the git config it was opened with — so unlike every
    // other transaction in dotsync, this one is only visible after re-opening.
    let repo = load_repo_direct(paths).await?;
    let repo = fetch_origin(repo).await?;
    let identity = detect_machine()?;

    let graph = scope_graph::derive(repo.as_ref())?;
    let repo = if graph.names().next().is_none() {
        start_a_new_fleet(repo, &identity, parents).await?
    } else {
        join_the_fleet(repo, &graph, &identity, parents).await?
    };

    let mut session = Session::from_repo(paths, repo).await?;
    let push = push_scope_updates(&mut session).await?;
    // The scopes exist by now, which is what `Home` needs: it puts the working
    // copy commit on this machine's scope bookmark. The sync discards home's
    // side, because `init` is the one command with nothing of yours to carry —
    // whatever home holds at a managed path predates dotsync managing it.
    let mut home = Home::acquire(&mut session, paths).await?;
    let outcome = crate::sync::sync_home_to_machine_scope(
        &mut session,
        &mut home,
        crate::sync::LocalChanges::Discard,
    )
    .await;
    let sync = finishing(home, &session, outcome).await?;

    Ok(InitReport { sync, push })
}

/// A remote with no scopes on it: this machine is the first, so there is
/// nothing to be told and nothing to choose from. It gets the root scope, a
/// scope for its OS — the one thing dotsync knows about a machine that is
/// worth sharing — and its own leaf under that.
async fn start_a_new_fleet(
    repo: Arc<ReadonlyRepo>,
    identity: &MachineIdentity,
    parents: &[String],
) -> Result<Arc<ReadonlyRepo>, DotsyncError> {
    if let Some(named) = parents.first() {
        return Err(DotsyncError::NoSuchParentScope {
            parent: named.clone(),
            scopes: Vec::new(),
        });
    }

    let author = machine_signature(&identity.machine_scope);
    let mut tx = repo.start_transaction();
    let root = tx.repo_mut().store().root_commit();
    let all =
        write_scope_creation(tx.repo_mut(), ROOT_SCOPE, &[root], None, author.clone()).await?;
    let os = write_scope_creation(
        tx.repo_mut(),
        &identity.os_scope,
        &[all],
        None,
        author.clone(),
    )
    .await?;
    write_scope_creation(tx.repo_mut(), &identity.machine_scope, &[os], None, author).await?;
    tx.commit("dotsync: start a new fleet")
        .await
        .map_err(|err| jj_error(format!("commit init scopes: {err}")))
}

/// A remote that already has scopes. Where this machine's config hangs is the
/// one thing its hostname cannot say — `home-linux` and `work-linux` are both
/// linux machines — so it has to be told, and the graph is append-only, so it
/// cannot be moved afterwards.
async fn join_the_fleet(
    repo: Arc<ReadonlyRepo>,
    graph: &ScopeGraph,
    identity: &MachineIdentity,
    parents: &[String],
) -> Result<Arc<ReadonlyRepo>, DotsyncError> {
    if let Some(existing) = graph.get(&identity.machine_scope) {
        // Config on a scope reaches every machine below it, so a scope
        // something else already hangs off cannot be one machine's own. This
        // is what `DOTSYNC_HOSTNAME=linux` used to do: adopt the shared OS
        // scope as this machine's private one and publish this machine's
        // config to every linux machine in the fleet.
        if !existing.is_leaf() {
            return Err(DotsyncError::MachineScopeIsShared {
                scope: existing.name.clone(),
                children: existing.children.clone(),
            });
        }
        if !parents.is_empty() {
            return Err(DotsyncError::MachineScopeAlreadyPlaced {
                scope: existing.name.clone(),
                parents: existing.parents.clone(),
            });
        }
        return Ok(repo);
    }

    // A branch of this name that is not a scope belongs to whoever pushed it,
    // and creating this machine's scope would move it.
    if !scope_head(repo.as_ref(), &identity.machine_scope).is_absent() {
        return Err(DotsyncError::ScopeNameTaken {
            scope: identity.machine_scope.clone(),
        });
    }

    let parent_commits =
        parent_commits_for(repo.as_ref(), graph, &identity.machine_scope, parents)?;
    let mut tx = repo.start_transaction();
    write_scope_creation(
        tx.repo_mut(),
        &identity.machine_scope,
        &parent_commits,
        None,
        machine_signature(&identity.machine_scope),
    )
    .await?;
    tx.commit("dotsync: join the fleet")
        .await
        .map_err(|err| jj_error(format!("commit machine scope: {err}")))
}

/// `dotsync create-scope`: the whole of what can be done to the scope graph.
///
/// A scope is created once and never renamed, reparented or deleted, which is
/// what lets the graph be read off the repo's structure — every edge is a
/// commit's parent, and commits do not change. Rearranging a graph is still an
/// open question (PLAN §2.7); the shape it replaces reported success for a
/// scope it had not created.
pub async fn create_scope(
    paths: &DotsyncPaths,
    scope: &str,
    parents: &[String],
    description: Option<&str>,
) -> Run<Result<CreatedScope, DotsyncError>> {
    in_session(paths, async |session, _paths| {
        // A paused cascade has scopes half cascaded, and this run ends by
        // publishing every scope commit the machine holds — so the pause has
        // to be resolved first, for the reason a commit does.
        crate::pause::reject_commit_if_paused(session, session.machine_scope()).await?;
        session.fetch().await?;
        let graph = session.graph().clone();
        if graph.contains(scope) || !scope_head(session.repo().as_ref(), scope).is_absent() {
            return Err(DotsyncError::ScopeNameTaken {
                scope: scope.to_string(),
            });
        }
        let parent_commits = parent_commits_for(session.repo().as_ref(), &graph, scope, parents)?;

        let repo = session.repo().clone();
        let mut tx = repo.start_transaction();
        write_scope_creation(
            tx.repo_mut(),
            scope,
            &parent_commits,
            description,
            machine_signature(&detect_machine()?.machine_scope),
        )
        .await?;
        session
            .advance_to(
                tx.commit(format!("dotsync: create {scope} scope"))
                    .await
                    .map_err(|err| jj_error(format!("commit scope creation: {err}")))?,
            )
            .await?;

        Ok(CreatedScope {
            scope: scope.to_string(),
            parents: parents.to_vec(),
            push: push_scope_updates(session).await?,
        })
    })
    .await
}

/// The heads a new scope hangs off, refusing anything that is not a scope this
/// repo has.
fn parent_commits_for(
    repo: &dyn jj_lib::repo::Repo,
    graph: &ScopeGraph,
    scope: &str,
    parents: &[String],
) -> Result<Vec<Commit>, DotsyncError> {
    if parents.is_empty() {
        return Err(DotsyncError::ParentScopeRequired {
            scope: scope.to_string(),
            scopes: graph.names().map(str::to_string).collect(),
        });
    }
    parents
        .iter()
        .map(|parent| {
            if !graph.contains(parent) {
                return Err(DotsyncError::NoSuchParentScope {
                    parent: parent.clone(),
                    scopes: graph.names().map(str::to_string).collect(),
                });
            }
            scope_head_commit(repo, parent)
        })
        .collect()
}

/// Writes the commit that creates a scope and points the scope's bookmark at
/// it.
///
/// The commit's own parents are the heads of the scope's parent scopes, so the
/// edge in the graph and the history are one fact rather than two that can
/// disagree — `scope_graph::derive` reads the graph back out of exactly this.
/// Its description names the scope, which is what makes it findable, and
/// carries whatever the creator said the scope is for.
async fn write_scope_creation(
    mut_repo: &mut MutableRepo,
    scope: &str,
    parents: &[Commit],
    description: Option<&str>,
    author: Signature,
) -> Result<Commit, DotsyncError> {
    let tree = merge_commit_trees(mut_repo, parents)
        .await
        .map_err(|err| jj_error(format!("merge the parents of {scope}: {err}")))?;
    if tree.has_conflict() {
        return Err(DotsyncError::ScopeCreationConflict {
            scope: scope.to_string(),
            files: tree
                .conflicts()
                .map(|(path, _)| path.as_internal_file_string().to_string())
                .collect(),
        });
    }

    let commit = mut_repo
        .new_commit(
            parents.iter().map(|parent| parent.id().clone()).collect(),
            tree,
        )
        .set_description(creation_description(scope, description))
        .set_author(author)
        .write()
        .await
        .map_err(|err| jj_error(format!("write the commit creating {scope}: {err}")))?;
    mut_repo.set_local_bookmark_target(
        RefNameBuf::from(scope).as_ref(),
        RefTarget::normal(commit.id().clone()),
    );
    Ok(commit)
}
