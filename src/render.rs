use crate::UsageError;
use dotsync::{DotsyncError, ErrorReport, FileDrift};
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
        DotsyncError::FetchWouldOverwriteLocalBookmark { .. } => render_structured_error(
            "fetch would overwrite local bookmark",
            "Dotsync fetches remote scope bookmarks before syncing or preparing a scoped commit so each machine sees published scope history from the shared repo.",
            "This fetch flow may fast-forward a local bookmark when the remote simply advances it, but it must not rewrite local bookmark history.",
            "It expects every remote bookmark update to either match the local bookmark or move it forward; dotsync must not move a local bookmark backward or sideways or lose unpublished local state.",
            error_report
                .current_state
                .as_deref()
                .unwrap_or(&error_report.message),
            "Dotsync stopped because this remote update would move a local bookmark backward or sideways, which would discard or bypass unpublished local state.",
            &[
                "Publish or intentionally discard the local-only bookmark state before syncing.",
                "If the remote bookmark was rewritten intentionally, reconcile that history explicitly instead of letting dotsync reset the local bookmark.",
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
            "It expects you to resolve the conflicted file in home, then run `dotsync continue` to create the merge commit and resume the cascade.",
            error_report
                .current_state
                .as_deref()
                .unwrap_or(&error_report.message),
            &error_report.message,
            &[
                "edit each conflicted file at its real path in home and keep the desired final contents.",
                "run `dotsync continue` from the same machine to finish cascading and syncing.",
                "do not run another dotsync commit while the cascade is paused.",
            ],
        ),
        DotsyncError::ConcurrentScopeConflict { .. } => render_structured_error(
            "concurrent scope conflict",
            "Dotsync stores one shared version of each file on a scope branch, and machines import selected home edits into that scope explicitly.",
            "This commit flow fetched remote scope history before committing, then found that the selected home file does not match a scope path that changed since this machine's previous local view of that scope.",
            "It expects you to resolve the home file against the already-published scope version before creating a new shared-scope commit.",
            error_report
                .current_state
                .as_deref()
                .unwrap_or(&error_report.message),
            &error_report.message,
            &[
                "inspect the already-published scope version before deciding what the shared file should contain.",
                "edit the conflicted file in home so it contains the resolved shared contents.",
                "rerun `dotsync commit <scope> -m \"message\" -- <path>` after resolving the file.",
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
                "edit each conflicted file at its real path in home and keep the desired final contents.",
                "run `dotsync continue` to finish the paused cascade.",
                "after `dotsync continue` succeeds, rerun the new commit if it is still needed.",
            ],
        ),
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
        DotsyncError::NotInitialized { .. } => render_structured_error(
            "not initialized",
            "Dotsync keeps its hidden repo at ~/.local/share/dotsync/repo and syncs committed scope state into your home directory.",
            "This command needs the hidden repo before it can inspect scopes, compare files, or sync managed config.",
            "Run `dotsync init <remote-url>` once so dotsync can clone or create its repo state.",
            error_report
                .current_state
                .as_deref()
                .unwrap_or(&error_report.message),
            "Dotsync stopped before reading repo state because this home directory has not been initialized.",
            &[
                "run `dotsync init <remote-url>` with the git remote that stores your dotsync repo.",
                "after init succeeds, rerun this command.",
            ],
        ),
        DotsyncError::NotImplemented(_)
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

pub(crate) fn success_notes_for_drifts(drifts: &[FileDrift]) -> Vec<String> {
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
