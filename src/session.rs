use std::sync::Arc;

use jj_lib::repo::ReadonlyRepo;

use crate::config::{load_config, DotsyncConfig, DotsyncPaths};
use crate::error::DotsyncError;
use crate::repo::{fetch_origin, load_repo_direct};

/// Everything one run of dotsync knows: where home and the hidden repo are,
/// the repo as this run opened it, the scope graph read out of that repo, and
/// whether the run reached the remote.
///
/// One session per run, built at the command boundary and passed down. Before
/// it, every helper took `&DotsyncPaths` and re-derived the rest — so a single
/// `dotsync commit` opened the same repo seven times, and `dotsync view`
/// fetched once per scope because each helper in the loop fetched for itself.
/// Neither of those is reachable from here: the loop body has no repo of its
/// own to open and no fetch to make.
pub(crate) struct Session {
    paths: DotsyncPaths,
    repo: Arc<ReadonlyRepo>,
    config: DotsyncConfig,
    unreachable_remote: Option<UnreachableRemote>,
}

impl Session {
    pub(crate) async fn open(paths: &DotsyncPaths) -> Result<Self, DotsyncError> {
        let repo = load_repo_direct(paths).await?;
        let config = load_config(paths, repo.as_ref()).await?;
        Ok(Self {
            paths: paths.clone(),
            repo,
            config,
            unreachable_remote: None,
        })
    }

    pub(crate) fn paths(&self) -> &DotsyncPaths {
        &self.paths
    }

    pub(crate) fn config(&self) -> &DotsyncConfig {
        &self.config
    }

    pub(crate) fn repo(&self) -> &Arc<ReadonlyRepo> {
        &self.repo
    }

    /// The one fetch a run makes.
    ///
    /// A remote out of reach is not a dead end. DESIGN's "offline is just
    /// deferred convergence" says to carry on against the last state we did
    /// fetch and let the next run that reaches the remote converge — so
    /// read-only commands report against that state, and mutating commands
    /// build local-ahead history on it, which is what they would do anyway
    /// against a remote that had not moved.
    ///
    /// Recording it here rather than at each caller is the point of having a
    /// session: one decision per run instead of one per helper, and one place
    /// for every command to read the fact back out of.
    pub(crate) async fn fetch(&mut self) -> Result<(), DotsyncError> {
        match fetch_origin(self.repo.clone()).await {
            Ok(repo) => self.advance_to(repo).await,
            Err(DotsyncError::RemoteUnreachable { reason }) => {
                self.unreachable_remote = Some(UnreachableRemote { reason });
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Adopts the repo left behind by a committed transaction, so the rest of
    /// the run reads what that transaction wrote instead of re-opening the
    /// repo from disk to find it.
    pub(crate) async fn advance_to(&mut self, repo: Arc<ReadonlyRepo>) -> Result<(), DotsyncError> {
        // A fetch fast-forwards `all`, and a commit or a cascade can write to
        // it, so the graph is re-read rather than assumed to be the one this
        // session opened with.
        self.config = load_config(&self.paths, repo.as_ref()).await?;
        self.repo = repo;
        Ok(())
    }

    /// Hands a report back together with what the run it came from could not
    /// do.
    pub(crate) fn finish<T>(&self, report: T) -> Run<T> {
        Run {
            report,
            unreachable_remote: self.unreachable_remote.clone(),
        }
    }

    pub(crate) fn unreachable_remote(&self) -> Option<UnreachableRemote> {
        self.unreachable_remote.clone()
    }
}

/// What a command reports, plus what the run it happened in could not do.
///
/// The remote being out of reach is a fact about the run rather than about
/// anything the run found, so it rides alongside the report instead of being
/// threaded into all eight of them.
#[derive(Debug, Clone)]
pub struct Run<T> {
    pub report: T,
    /// `Some` when this run could not reach the remote and worked from the
    /// last state it did fetch. Not a mode: there is no offline mode and no
    /// queue, only local history that is ahead until a run that reaches the
    /// remote converges it.
    pub unreachable_remote: Option<UnreachableRemote>,
}

/// The remote could not be reached, in git's own words.
#[derive(Debug, Clone)]
pub struct UnreachableRemote {
    pub reason: String,
}
