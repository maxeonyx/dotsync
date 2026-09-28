//! Writes as plans: which scope holds which version of which path, run through
//! the convergence pass, with what each machine then receives.
//!
//! Every write dotsync makes is a set of pins — per scope, per path, what the
//! scope is to hold once the pass has merged it (`converge::Pins`) — and one
//! pass that lays them down in cascade order. `commit` pins home's edit onto
//! one scope. `move` and `drop` are this module's own: they change which scope
//! a file's version lives on, taking the content from the repo.
//!
//! Taking the content from the repo is what lets them reach any scope. A write
//! from home needs a version of the target scope that home was derived from,
//! and home is only ever derived from this machine's own scopes. A write whose
//! content is a scope's own tree entry, landing on heads this run has just
//! converged, has an exact base wherever it lands — the only thing it cannot
//! know is whether the machines it reaches have tried the config, which is why
//! every write says which machines it changed.
//!
//! Both keep every other scope's own version where it is. They change who owns
//! a file, not what anybody else decided about it, so neither lands a conflict
//! on another machine's scope for this machine to resolve through its own home.

use std::collections::BTreeMap;
use std::path::PathBuf;

use jj_lib::transaction::Transaction;

use crate::converge::{self, Pause, Pin, Pins};
use crate::error::{jj_error, DotsyncError};
use crate::fleet::{effect, machine_entries, Fleet, MachineEffect, Standing};
use crate::home::{repo_path_of, Home};
use crate::paths::DotsyncPaths;
use crate::pause::{conflicted_files, pause_at, publish_or_pause, reject_commit_if_paused};
use crate::repo::{collect_managed_tree_entries, PushReport};
use crate::session::{in_session, Run, Session};
use crate::status::PausedCascade;
use crate::sync::{finishing, sync_home_to_machine_scope, LocalChanges, SyncReport};

/// `dotsync move` when `to` is named, `dotsync drop` when it is not.
#[derive(Debug, Clone)]
pub struct PlacementOptions {
    pub paths: Vec<PathBuf>,
    pub from: String,
    pub to: Option<String>,
    pub message: String,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct PlacementReport {
    pub machine_scope: String,
    pub paths: Vec<PathBuf>,
    pub planned: Planned,
    /// What the run then did to this machine and the remote. `None` for a dry
    /// run, which does neither.
    pub carried_out: Option<CarriedOut>,
}

/// What a write would do: to every machine, and where it would stop.
#[derive(Debug, Clone)]
pub struct Planned {
    pub effect: Vec<MachineEffect>,
    /// The merge the write would stop at, with every version of every file it
    /// could not resolve. Only a dry run reports one; a real run stops there.
    pub stops_at: Option<PausedCascade>,
}

#[derive(Debug, Clone)]
pub struct CarriedOut {
    pub push: PushReport,
    pub sync: SyncReport,
}

pub async fn place(
    paths: &DotsyncPaths,
    options: PlacementOptions,
) -> Run<Result<PlacementReport, DotsyncError>> {
    in_session(paths, async |session, paths| {
        if options.message.trim().is_empty() {
            return Err(DotsyncError::EmptyCommitMessage {
                scope: options.to.clone().unwrap_or_else(|| options.from.clone()),
            });
        }
        reject_commit_if_paused(session, session.machine_scope()).await?;
        let mut home = Home::acquire(session, paths).await?;
        let outcome = place_in_session(session, &mut home, &options).await;
        finishing(home, session, outcome).await
    })
    .await
}

async fn place_in_session(
    session: &mut Session,
    home: &mut Home,
    options: &PlacementOptions,
) -> Result<PlacementReport, DotsyncError> {
    session.fetch().await?;
    for scope in std::iter::once(&options.from).chain(options.to.as_ref()) {
        if !session.graph().contains(scope) {
            return Err(DotsyncError::InvalidScope {
                scope: scope.clone(),
            });
        }
    }
    if options.to.as_ref() == Some(&options.from) {
        return Err(DotsyncError::MoveOntoItself {
            scope: options.from.clone(),
        });
    }

    let checkpoint = converge::checkpoint(session.repo().as_ref(), session.graph());
    let machine_scope = home.machine_scope().to_string();
    let graph = session.graph().clone();
    let mut tx = session.repo().start_transaction();
    // The convergence every write opens with, in the same transaction as the
    // write, so a dry run can predict both without recording either.
    let (converged, stopped) = converge::pass(&mut tx, &graph, &machine_scope, None, None).await?;
    if let Some(pause) = stopped {
        return Err(stop_before_planning(
            session,
            home,
            &checkpoint,
            tx,
            converged,
            pause,
            options.dry_run,
        )
        .await?);
    }

    let fleet = Fleet::read(tx.repo(), &graph).await?;
    let named: Vec<PathBuf> = options.paths.iter().map(|path| normalized(path)).collect();
    let paths = under_directories(&fleet, options, &named);
    let pins = placement_pins(&fleet, options, &paths)?;
    let planned = plan(session, home, &mut tx, &pins).await?;
    // A pause is recomputed by a pass without pins, so one caused by a
    // placement could never be found again. Refused whole, dry or not.
    if let Some(pause) = &planned.stop {
        let files = conflicted_files(session, &pause.merged, &pause.scope).await?;
        return Err(DotsyncError::PlacementWouldConflict {
            scope: pause.scope.clone(),
            files,
        });
    }

    if options.dry_run {
        drop(tx);
        return Ok(PlacementReport {
            machine_scope,
            paths,
            planned: planned.report(session).await?,
            carried_out: None,
        });
    }
    let Plan {
        moved,
        effect,
        stop,
    } = planned;
    let operation = match options.to {
        Some(_) => "dotsync: move",
        None => "dotsync: drop",
    };
    let carried_out = carry_out(
        session,
        home,
        &checkpoint,
        tx,
        converged || moved,
        stop,
        operation,
    )
    .await?;
    Ok(PlacementReport {
        machine_scope,
        paths,
        planned: Planned {
            effect,
            stops_at: None,
        },
        carried_out: Some(carried_out),
    })
}

/// Which scope holds which version once the move or drop is done.
///
/// `from` stops holding a version of its own: it inherits. `to`, for a move,
/// holds the version `from` had. Every other scope that holds its own version
/// keeps exactly that, so the only machines whose config changes are the ones
/// that took theirs from `from` or will now take it from `to`.
fn placement_pins(
    fleet: &Fleet,
    options: &PlacementOptions,
    paths: &[PathBuf],
) -> Result<Pins, DotsyncError> {
    let mut by_scope: BTreeMap<String, Vec<(jj_lib::repo_path::RepoPathBuf, Pin)>> =
        BTreeMap::new();
    for path in paths {
        let own = fleet
            .row(&options.from, path)
            .filter(|row| placeable(row.standing, options));
        let Some(_) = own else {
            return Err(DotsyncError::NotOwnOnScope {
                scope: options.from.clone(),
                path: path.clone(),
                origin: fleet
                    .row(&options.from, path)
                    .map(|row| row.origin.clone())
                    .unwrap_or_default(),
            });
        };
        // Its version may be what settles its parents' disagreement.
        let parents = fleet
            .scopes()
            .iter()
            .find(|scope| scope.name == options.from)
            .map(|scope| scope.parents.clone())
            .unwrap_or_default();
        if let Some((_, inherited)) = fleet.inherits(&options.from) {
            if matches!(inherited.get(path), Some(None)) {
                return Err(DotsyncError::ParentsDisagree {
                    scope: options.from.clone(),
                    path: path.clone(),
                    parents,
                });
            }
        }
        let repo_path = repo_path_of(path)?;
        let value_on = |scope: &str| {
            let (tree, _) = fleet.holds(scope).expect("a scope with a row has a tree");
            tree.path_value(&repo_path)
                .map_err(|err| jj_error(format!("read {} on {scope}: {err}", path.display())))
        };

        if let Some(to) = &options.to {
            by_scope
                .entry(to.clone())
                .or_default()
                .push((repo_path.clone(), Pin::Holds(value_on(&options.from)?)));
        }
        by_scope
            .entry(options.from.clone())
            .or_default()
            .push((repo_path.clone(), Pin::Inherits));
        for row in fleet.rows().iter().filter(|row| {
            &row.path == path
                && row.standing.is_own()
                && row.scope != options.from
                && Some(&row.scope) != options.to.as_ref()
        }) {
            by_scope
                .entry(row.scope.clone())
                .or_default()
                .push((repo_path.clone(), Pin::Holds(value_on(&row.scope)?)));
        }
    }
    Ok(Pins {
        by_scope,
        description: options.message.clone(),
    })
}

/// Whether a scope's standing at a path is something this placement can act
/// on. What moves is a version, so a removal has nothing to carry; a drop acts
/// on anything the scope holds of its own.
fn placeable(standing: Standing, options: &PlacementOptions) -> bool {
    match options.to {
        Some(_) => matches!(standing, Standing::Added | Standing::Overridden),
        None => standing.is_own(),
    }
}

/// Every path named, with a directory replaced by what `from` holds of its own
/// under it — the way a directory named to `commit` stands for what is under
/// it. A skill is a folder, and moving or removing one is one change.
///
/// A path the scope holds an entry at is itself, whatever is under it, and a
/// directory with nothing placeable under it stays as named, so the refusal
/// names what the caller typed.
fn under_directories(fleet: &Fleet, options: &PlacementOptions, named: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for path in named {
        let under: Vec<PathBuf> = match fleet.row(&options.from, path) {
            Some(_) => Vec::new(),
            None => fleet
                .rows()
                .iter()
                .filter(|row| {
                    row.scope == options.from
                        && row.path.starts_with(path)
                        && placeable(row.standing, options)
                })
                .map(|row| row.path.clone())
                .collect(),
        };
        match under.is_empty() {
            true => paths.push(path.clone()),
            false => paths.extend(under),
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// The pins laid down by one more pass over a transaction that has already
/// converged, and every machine's config compared before and after.
pub(crate) struct Plan {
    pub(crate) moved: bool,
    pub(crate) effect: Vec<MachineEffect>,
    pub(crate) stop: Option<Pause>,
}

impl Plan {
    /// The plan as a dry run reports it.
    pub(crate) async fn report(self, session: &Session) -> Result<Planned, DotsyncError> {
        let stops_at = match self.stop {
            Some(pause) => Some(PausedCascade {
                conflicts: conflicted_files(session, &pause.merged, &pause.scope).await?,
                scope: pause.scope,
            }),
            None => None,
        };
        Ok(Planned {
            effect: self.effect,
            stops_at,
        })
    }
}

pub(crate) async fn plan(
    session: &Session,
    home: &Home,
    tx: &mut Transaction,
    pins: &Pins,
) -> Result<Plan, DotsyncError> {
    let graph = session.graph();
    let before = machine_entries(tx.repo(), graph).await?;
    let (moved, stop) = converge::pass(tx, graph, home.machine_scope(), None, Some(pins)).await?;
    let after = machine_entries(tx.repo(), graph).await?;
    let home_entries = collect_managed_tree_entries(&home.snapshot_tree())?;
    Ok(Plan {
        moved,
        effect: effect(&before, &after, Some((home.machine_scope(), &home_entries))),
        stop,
    })
}

/// A planned write made real: recorded, published, and synced into home —
/// or, where it stops at a merge, recorded as far as the stop and presented.
pub(crate) async fn carry_out(
    session: &mut Session,
    home: &mut Home,
    checkpoint: &BTreeMap<String, String>,
    tx: Transaction,
    moved: bool,
    stop: Option<Pause>,
    operation: &str,
) -> Result<CarriedOut, DotsyncError> {
    if moved {
        session
            .advance_to(
                tx.commit(operation)
                    .await
                    .map_err(|err| jj_error(format!("record {operation}: {err}")))?,
            )
            .await?;
    }
    if let Some(pause) = stop {
        return Err(pause_at(session, home, checkpoint, pause).await?);
    }
    // Push as soon as the history exists: the home sync below can stop on a
    // conflict, and a stop must never strand committed scope history.
    let push = publish_or_pause(session, home, checkpoint).await?;
    let sync = sync_home_to_machine_scope(session, home, LocalChanges::Carry).await?;
    Ok(CarriedOut { push, sync })
}

/// A merge that was already waiting before the write could be planned. A real
/// run records the convergence it managed and stops there, like any other run;
/// a dry run records nothing and presents the same stop.
async fn stop_before_planning(
    session: &mut Session,
    home: &mut Home,
    checkpoint: &BTreeMap<String, String>,
    tx: Transaction,
    converged: bool,
    pause: Pause,
    dry_run: bool,
) -> Result<DotsyncError, DotsyncError> {
    if dry_run {
        drop(tx);
        return crate::pause::present(session, home.machine_scope(), &pause.merged, &pause.scope)
            .await;
    }
    if converged {
        session
            .advance_to(
                tx.commit("dotsync: converge")
                    .await
                    .map_err(|err| jj_error(format!("commit the convergence: {err}")))?,
            )
            .await?;
    }
    pause_at(session, home, checkpoint, pause).await
}

/// A path as the repo spells it: relative, forward slashes, no trailing
/// separator.
pub(crate) fn normalized(path: &std::path::Path) -> PathBuf {
    let spelled = path.to_string_lossy().replace('\\', "/");
    PathBuf::from(spelled.trim_start_matches("./").trim_end_matches('/'))
}
