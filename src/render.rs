use crate::UsageError;
use dotsync::{DotsyncError, ErrorReport, FileDrift, PushReport};
use serde_json::json;
use std::path::Path;

pub(crate) fn render_error_json(error: &ErrorReport) -> serde_json::Value {
    json!({
        "status": "error",
        "error": error.code,
        "message": error.message,
        "drifts": error.drifts.iter().map(render_drift_json).collect::<Vec<_>>(),
        "current_state": error.current_state,
    })
}

pub(crate) fn render_usage_error_json(error: &UsageError) -> serde_json::Value {
    json!({
        "status": "error",
        "error": "usage",
        "message": error.message,
    })
}

pub(crate) fn render_drift_json(drift: &FileDrift) -> serde_json::Value {
    json!({
        "path": display_path(&drift.repo_path),
        "system_path": display_path(&drift.system_path),
        "diff": drift.diff,
    })
}

pub(crate) fn render_error_human(error: &DotsyncError) -> String {
    let error_report = error.to_error_report();

    match error {
        DotsyncError::ScopeDiverged { scope, .. } => render_structured_error(
            &format!("scope `{scope}` has diverged from the remote"),
            "Dotsync fetches remote scope bookmarks before syncing or committing so each machine picks up scope history published by the other machines.",
            "This fetch flow fast-forwards a scope when the remote has simply moved ahead, and leaves the scope alone when this machine holds commits it has not pushed yet.",
            "It expects the local and remote positions of a scope to be on one line of history, so that one of them is an ancestor of the other.",
            error_report
                .current_state
                .as_deref()
                .unwrap_or(&error_report.message),
            "This machine and the remote both have commits on this scope that the other does not, so neither side can be fast-forwarded onto the other.",
            &[
                "Nothing has been lost or changed: your local commits are intact and still unpushed.",
                "Dotsync cannot merge diverged scopes yet — that is https://github.com/maxeonyx/dotsync/issues/17. Report this state rather than repairing the repo by hand.",
            ],
        ),
        DotsyncError::DriftDetected { .. } => render_structured_error(
            "drift detected",
            "Dotsync keeps its hidden repo as the source of truth for your home-directory config: the repo is the source of truth, and dotsync syncs committed repo state into the live system.",
            "This sync flow compares managed files in your home directory against the repo version for this machine scope before copying anything.",
            "This flow expects managed files in your home directory to already match the repo, unless you intentionally choose to overwrite drift.",
            "Drifted files are listed below with diffs.",
            "Dotsync stopped before overwriting local drift so you can inspect what would be replaced.",
            &[
                "If the repo is correct, rerun with `dotsync --force` to overwrite the drift after reviewing the diffs.",
                "If the live file is the change you wanted, run `dotsync status`, then commit the intended path with `dotsync commit <scope> -m \"message\" -- <path>`.",
            ],
        ),
        DotsyncError::CascadePaused { .. } => render_structured_error(
            "cascade paused",
            "Dotsync records a home edit on one scope, then cascades that scope through descendant scope branches so every machine receives the right final config.",
            "This commit flow was merging the scoped change through the scope DAG and reached a branch where the same file had incompatible edits.",
            "It expects you to edit the conflicted file in home to the merged contents you want, then run `dotsync continue` to create the merge commit and resume the cascade.",
            error_report
                .current_state
                .as_deref()
                .unwrap_or(&error_report.message),
            &error_report.message,
            &[
                "edit each conflicted file at its real path in home so it holds the merged contents you want; the file has to change, because dotsync reads the resolution back out of it.",
                "run `dotsync continue` from the same machine to finish cascading and syncing.",
                "or run `dotsync abort` from the same machine to discard the paused cascade; that reverts the conflicted files in home to this machine's scope state.",
                "do not run another dotsync commit while the cascade is paused.",
            ],
        ),
        DotsyncError::UnresolvedConflict { scope, paths } => render_structured_error(
            "conflict not resolved",
            "Dotsync records a home edit on one scope, then cascades that scope through descendant scope branches so every machine receives the right final config. Where two branches changed one file differently, the cascade pauses and asks you for the merged contents.",
            "This continue flow reads each conflicted file back out of your home directory and records what it finds there as the resolution.",
            "It expects those files to have changed since the cascade paused, because the resolution is the contents you write into them.",
            error_report
                .current_state
                .as_deref()
                .unwrap_or(&error_report.message),
            "Dotsync does not yet write the two conflicting versions into home, so an unchanged file is not a resolution - it is only the version that happened to already be there. Recording it would silently discard the other scope's version.",
            &[
                &format!(
                    "read the version dotsync would discard with `dotsync view --scope {scope} --file {}`, and compare it against the file in home.",
                    paths
                        .first()
                        .map(|path| display_path(path))
                        .unwrap_or_default()
                ),
                "write the merged contents into the file in home, then run `dotsync continue`.",
                "`dotsync abort` discards the paused cascade, and reverts the conflicted files in home to this machine's scope state - so anything in home you want to keep must be saved outside home first.",
                &format!(
                    "if home already holds exactly the contents you want: save them outside home, run `dotsync abort`, put them back, commit them to `{scope}` directly, then redo the original commit."
                ),
            ],
        ),
        DotsyncError::PausedCascadeInProgress { .. } => render_structured_error(
            "paused cascade in progress",
            "Dotsync records a home edit on one scope, then cascades that scope through descendant scope branches so every machine receives the right final config.",
            "This commit flow was about to start a new scoped commit, but a previous cascade is still paused for conflict resolution.",
            "It expects exactly one cascade to be active at a time so commit history, conflict resolution, and home sync state stay aligned.",
            error_report
                .current_state
                .as_deref()
                .unwrap_or(&error_report.message),
            "Dotsync stopped before fetching, committing, or syncing because starting another commit would hide the real paused-cascade task and may mutate unrelated scope state.",
            &[
                "edit each conflicted file at its real path in home so it holds the merged contents you want; the file has to change, because dotsync reads the resolution back out of it.",
                "run `dotsync continue` to finish the paused cascade.",
                "or run `dotsync abort` to discard the paused cascade; that reverts the conflicted files in home to this machine's scope state.",
                "after `dotsync continue` succeeds, rerun the new commit if it is still needed.",
            ],
        ),
        DotsyncError::UnusableCommitPaths { scope, rejected } => {
            let mut steps = vec![format!(
                "name paths relative to your home directory: `dotsync commit {scope} -m \"message\" -- .config/fish/config.fish`."
            )];
            steps.push(
                "do not use `~/`, absolute paths, or `..`; dotsync resolves every path against your home directory already, and records it verbatim.".to_string(),
            );
            if rejected.iter().any(|rejected| rejected.is_scope_graph()) {
                steps.push(
                    "commit the scope graph to `all`, which is the only scope dotsync reads it from: `dotsync commit all -m \"message\" -- .config/dotsync/config.toml`."
                        .to_string(),
                );
            }
            if rejected.iter().any(|rejected| rejected.is_dotsync_state()) {
                steps.push(
                    "commit the config files you edited instead; dotsync's own state is not config and cannot travel on a scope."
                        .to_string(),
                );
                steps.push(
                    "to change which scopes exist, edit `.config/dotsync/config.toml` in home and commit that path to `all`."
                        .to_string(),
                );
            }
            steps.push("run `dotsync status` to see which managed files changed.".to_string());

            render_structured_error(
                if rejected.len() == 1 {
                    "cannot commit that path"
                } else {
                    "cannot commit those paths"
                },
                "Dotsync records the home files you name onto a scope branch, then cascades that scope so every machine sharing it receives the change. Every file on a scope is written back into home on each of those machines.",
                "This commit flow resolves each path you name against your home directory, checks that it is a config file dotsync may record, and commits the ones that changed.",
                "It expects every path you name to be a config file inside your home directory, named relative to it, and to exist either in home or on the target scope already.",
                error_report
                    .current_state
                    .as_deref()
                    .unwrap_or(&error_report.message),
                "Dotsync stopped before recording anything. A commit records every path you named or none of them, so fixing the paths above and rerunning the same command is safe.",
                &steps.iter().map(String::as_str).collect::<Vec<_>>(),
            )
        }
        DotsyncError::InvalidScope { .. } => render_structured_error(
            "invalid scope",
            "Dotsync stores dotfiles in a scope DAG so shared config can live on shared ancestor scopes and machine-specific config can stay isolated on leaf scopes.",
            "This commit flow records your repo change on the scope you name and then cascades it through descendant scopes.",
            "It expects the scope you name to exist in the configured scope DAG.",
            &error_report.message,
            "Dotsync stopped because it cannot place this change onto a scope that is not configured.",
            &[
                "choose a real configured scope from the DAG.",
                "Pick the root-est appropriate ancestor scope that should own the change.",
            ],
        ),
        DotsyncError::SyncState { .. } => render_structured_error(
            "invalid sync state",
            "Dotsync keeps the repo as the source of truth and uses a local sync-state file to remember which machine scope was last synced here and which revision that sync used.",
            "This sync flow reads that local state to know which prior managed files may need removal and which machine scope should be treated as authoritative for this home.",
            "It expects that state file, if present, to be valid and readable; it expects that state file, if present, to be valid.",
            &error_report.message,
            "Dotsync stopped because it cannot safely decide what prior sync state to trust.",
            &[
                "fix or delete the bad sync-state file and rerun the command.",
                "After that, let dotsync recreate valid sync state from a successful sync.",
            ],
        ),
        DotsyncError::NotInitialized { path } => format!(
            "dotsync: not initialized\n\nWhat happened:\nDotsync could not find its hidden repo at {}.\n\nWhat to do:\n- Run `dotsync init <remote-url>` from this home directory.\n- Then rerun `dotsync status`.\n\nThe remote URL is the git remote that stores your dotsync repo.",
            path.display()
        ),
        DotsyncError::HomeNotSet
        | DotsyncError::NonUtf8Path { .. }
        | DotsyncError::GitSubmodule { .. }
        | DotsyncError::NoPausedCascade
        | DotsyncError::Io { .. }
        | DotsyncError::ConfigParse { .. }
        | DotsyncError::MissingParent { .. }
        | DotsyncError::ScopeCycle { .. }
        | DotsyncError::NoCurrentScope
        | DotsyncError::MissingScopeBookmark { .. }
        | DotsyncError::RepoAlreadyExists { .. }
        | DotsyncError::MissingHostname
        | DotsyncError::Jj { .. } => format!("dotsync: {}", error_report.message),
    }
}

pub(crate) fn render_structured_error(
    summary: &str,
    what_dotsync_does: &str,
    this_flow: &str,
    expected: &str,
    current_state: &str,
    why_stopped: &str,
    correct_flow_steps: &[&str],
) -> String {
    let correct_flow = correct_flow_steps
        .iter()
        .map(|step| format!("- {step}"))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "dotsync: {summary}\n\nWhat dotsync does:\n{what_dotsync_does}\n\nThis flow:\n{this_flow}\n\nExpected:\n{expected}\n\nCurrent state found:\n{current_state}\n\nWhy dotsync stopped:\n{why_stopped}\n\nCorrect flow:\n{correct_flow}"
    )
}

/// Notes printed to stderr alongside a successful run: what was overwritten,
/// and what did not reach the remote. `push` is `None` only for commands that
/// do not publish at all, so a publishing command cannot quietly omit this.
pub(crate) fn success_notes(drifts: &[FileDrift], push: Option<&PushReport>) -> Vec<String> {
    let mut notes = push.map(notes_for_push).unwrap_or_default();
    notes.extend(notes_for_drifts(drifts));
    notes
}

fn notes_for_push(push: &PushReport) -> Vec<String> {
    match push {
        PushReport::UpToDate => Vec::new(),
        PushReport::Refused {
            scopes,
            rejection_reason,
        } => {
            let reason = rejection_reason
                .clone()
                .unwrap_or_else(|| "no reason reported by the remote".to_string());
            vec![
                format!("dotsync: the remote refused {} ({reason})", scopes.join(", ")),
                "dotsync: those scopes are committed here but not published, so the remote does not have this change yet. The next run will try again.".to_string(),
            ]
        }
        PushReport::WithheldPausedCascade {
            scopes,
            paused_scope,
        } => {
            if scopes.is_empty() {
                return Vec::new();
            }
            vec![
                format!(
                    "dotsync: not publishing {} while the cascade paused at `{paused_scope}` is unresolved",
                    scopes.join(", ")
                ),
                "dotsync: run `dotsync continue` to finish the cascade, or `dotsync abort` to discard it; publishing resumes after that.".to_string(),
            ]
        }
    }
}

fn notes_for_drifts(drifts: &[FileDrift]) -> Vec<String> {
    if drifts.is_empty() {
        return Vec::new();
    }
    let mut notes = vec![format!(
        "dotsync: overwrote {} drifted file(s)",
        drifts.len()
    )];
    notes.extend(drifts.iter().flat_map(|drift| {
        [
            format!("- {}", drift.repo_path.display()),
            drift.diff.clone(),
        ]
    }));
    notes
}

pub(crate) fn render_drifts_human(drifts: &[FileDrift]) -> Vec<String> {
    drifts
        .iter()
        .flat_map(|drift| {
            [
                format!("- {}", drift.repo_path.display()),
                drift.diff.clone(),
            ]
        })
        .collect()
}

pub(crate) fn display_path(path: &Path) -> String {
    path.display().to_string()
}
