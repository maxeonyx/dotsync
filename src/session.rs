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
}

impl Session {
    pub(crate) async fn open(paths: &DotsyncPaths) -> Result<Self, DotsyncError> {
        let repo = load_repo_direct(paths).await?;
        let config = load_config(paths, repo.as_ref()).await?;
        Ok(Self {
            paths: paths.clone(),
            repo,
            config,
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
    pub(crate) async fn fetch(&mut self) -> Result<(), DotsyncError> {
        let repo = fetch_origin(self.repo.clone()).await?;
        self.advance_to(repo).await
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
}
