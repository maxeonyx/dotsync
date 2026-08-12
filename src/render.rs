use crate::UsageError;
use dotsync::{
    DotsyncError, ErrorReport, FileDrift, PushReport, RefusedCommitPath, UnreachableRemote,
};
use serde_json::json;
use similar::TextDiff;
use std::path::Path;

pub(crate) fn render_error_json(error: &ErrorReport) -> serde_json::Value {
    json!({
        "status": "error",
        "error": error.code,
        "message": error.message,
        "drifts": error.drifts.iter().map(render_drift_json).collect::<Vec<_>>(),
        "forced_overwrites": error.forced_overwrites.iter().map(|path| display_path(path)).collect::<Vec<_>>(),
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
        "state": drift.state.code(),
        "reason": drift.state.reason(),
        "diff": render_drift_diff(drift),
    })
}

/// A unified diff of what the repo holds against what home holds.
///
/// An absent side reads as empty, so a file deleted from home renders as its
/// whole content removed rather than as nothing at all. Non-UTF-8 content has
/// no line structure to diff, so it is reported rather than mangled.
pub(crate) fn render_drift_diff(drift: &FileDrift) -> String {
    let (Some(repo), Some(system)) = (
        drift_side_text(drift.repo_bytes.as_deref()),
        drift_side_text(drift.home_bytes.as_deref()),
    ) else {
        return "binary content differs".to_string();
    };

    let mut rendered = TextDiff::from_lines(&repo, &system)
        .unified_diff()
        .header("repo", "system")
        .to_string();
    // Every caller prints this as one block with `eprintln!`, so the diff's own
    // trailing newline would show up as a blank line.
    if rendered.ends_with('\n') {
        rendered.pop();
    }
    rendered
}

fn drift_side_text(bytes: Option<&[u8]>) -> Option<String> {
    match bytes {
        None => Some(String::new()),
        Some(bytes) => String::from_utf8(bytes.to_vec()).ok(),
    }
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
        DotsyncError::PausePredatesResolutionCheck { .. } => render_structured_error(
            "cannot check this conflict resolution",
            "Dotsync records a home edit on one scope, then cascades that scope through descendant scope branches so every machine receives the right final config. Where two branches changed one file differently, the cascade pauses and asks you for the merged contents.",
            "This continue flow reads each conflicted file back out of your home directory and records what it finds there as the resolution. To tell a resolution from an untouched file, it compares them against what they held when the cascade paused.",
            "It expects the paused cascade to have recorded those contents.",
            error_report
                .current_state
                .as_deref()
                .unwrap_or(&error_report.message),
            "This cascade was paused by an older dotsync, which recorded nothing to compare against. Continuing would record whatever is in home as the resolution without being able to tell whether anything was resolved, and that silently discards the other scope's version.",
            &[
                "run `dotsync abort` to discard the paused cascade; it reverts the conflicted files in home to this machine's scope state.",
                "then redo the commit that started the cascade; the pause it creates records what this check needs.",
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
            if rejected.iter().any(|rejected| rejected.is_home_root()) {
                steps.push(
                    "name the directories or files you actually mean: `dotsync commit <scope> -m \"message\" -- .config/fish/ .bashrc`. Dotsync will not sweep a whole home directory onto a scope."
                        .to_string(),
                );
            }
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
        DotsyncError::StaleCommitPaths { scope, refused } => render_structured_error(
            if refused.len() == 1 {
                "cannot commit a file this machine has not changed"
            } else {
                "cannot commit files this machine has not changed"
            },
            "Dotsync records the home files you name onto a scope branch and cascades them to every machine sharing it. Plain `dotsync` goes the other way, writing what the scopes hold back into home.",
            "This commit flow reads each path you named across three sides: what dotsync last synced to this machine, what is in home now, and what the scopes hold now.",
            "It expects the paths you name to hold a change you made in home since the last sync.",
            error_report
                .current_state
                .as_deref()
                .unwrap_or(&error_report.message),
            "Recording these would put older bytes back on the scope and cascade them, silently reverting whoever published the change that is already there.",
            &[
                "run `dotsync` to bring this machine up to date; the incoming change is written into home, and an incoming deletion removes the file.",
                "then edit the file in home if you still want a change of your own, and commit it. To bring back a file another machine deleted, recreate it in home after syncing and commit that.",
                &format!(
                    "if you really do mean to overwrite the incoming change with what is in home, rerun with `--force`: `dotsync commit {scope} -m \"message\" --force -- <paths...>`. On `commit`, `--force` applies only to the paths you name."
                ),
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
        // Every other command carries on against the last state it fetched
        // and says so in a note. `init` is the one whose whole job is to reach
        // the remote, so for it this really is a stop.
        DotsyncError::RemoteUnreachable { reason } => render_structured_error(
            "could not reach the remote",
            "Dotsync keeps your config in a hidden repo and shares it between your machines through a git remote, so every machine starts from what the others have already published.",
            "This init flow clones that remote into the hidden repo, works out which scopes this machine belongs to, and syncs them into home.",
            "It expects the remote URL you gave it to be reachable from this machine now.",
            reason,
            "Dotsync stopped rather than starting from an empty history: scopes created here would collide with the ones already on the remote the first time this machine reached it.",
            &[
                "check the remote URL, this machine's network, and your credentials for that remote.",
                "then run `dotsync init <remote-url>` again.",
            ],
        ),
        // The original failure is what there is to fix, so it renders in full;
        // the leftover directory is the extra step the retry now needs.
        DotsyncError::PartialInitLeftBehind {
            path,
            source,
            original,
        } => format!(
            "{}\n\nAlso:\nDotsync could not remove the partly created repo at {}: {source}. Delete that directory before running `dotsync init` again.",
            render_error_human(original),
            path.display()
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

/// Which state a run is reporting against, when it is not the remote's.
///
/// Printed by every command that could not fetch, before anything else it has
/// to say, because it is the frame for all of it: the drift, the scope list
/// and the commit that follows are all against the state this machine last
/// fetched rather than against the state the remote is in now.
pub(crate) fn unreachable_remote_notes(unreachable: Option<&UnreachableRemote>) -> Vec<String> {
    let Some(unreachable) = unreachable else {
        return Vec::new();
    };
    vec![
        "dotsync: could not reach the remote; reporting against the last-fetched state".to_string(),
        format!("dotsync: {}", unreachable.reason),
    ]
}

/// The machine-readable half of the same fact. Added at the one place every
/// command's JSON passes through, so no command can forget it — and only when
/// there is something to say, because a run that reached the remote and a run
/// that never needed it are the same answer to a reader of this field.
pub(crate) fn with_remote_state(
    mut json: serde_json::Value,
    unreachable: Option<&UnreachableRemote>,
) -> serde_json::Value {
    if let Some(unreachable) = unreachable {
        json["remote_unreachable"] = json!(unreachable.reason);
    }
    json
}

/// What a run overwrote under `--force`, said out loud. A forced overwrite is
/// the one thing a run can do that discards somebody else's work, so both
/// exits report it: the run that stopped afterwards, and the run that
/// finished and left the revert standing on the remote.
pub(crate) fn forced_overwrite_notes(forced_overwrites: &[std::path::PathBuf]) -> Vec<String> {
    if forced_overwrites.is_empty() {
        return Vec::new();
    }
    let mut notes = vec![format!(
        "dotsync: recorded {} file(s) over an incoming change, because you passed `--force`",
        forced_overwrites.len()
    )];
    notes.extend(
        forced_overwrites
            .iter()
            .map(|path| format!("- {}", path.display())),
    );
    notes
}

/// What a commit put on the scope for the first time.
///
/// Every machine sharing the scope will have these written into its home
/// directory by its next sync, which is a bigger thing than changing a line —
/// and a bulk selection can do it without the user having named a single one
/// of them.
pub(crate) fn newly_tracked_notes(newly_tracked: &[std::path::PathBuf]) -> Vec<String> {
    if newly_tracked.is_empty() {
        return Vec::new();
    }
    let mut notes = vec![format!(
        "dotsync: started tracking {} new file(s) on this scope",
        newly_tracked.len()
    )];
    notes.extend(listed(
        newly_tracked.iter().map(|path| path.display().to_string()),
    ));
    notes
}

/// At most a handful of lines, then a count. A commit can name hundreds of
/// files, and a note that scrolls the run's own result off the screen is worse
/// than a shorter one.
fn listed(lines: impl ExactSizeIterator<Item = String>) -> Vec<String> {
    const SHOWN: usize = 5;
    let total = lines.len();
    let mut listed = lines
        .take(SHOWN)
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>();
    if total > SHOWN {
        listed.push(format!("- ... and {} more", total - SHOWN));
    }
    listed
}

/// What a named directory matched that the commit left alone.
///
/// A bulk selection that recorded less than it matched has to say so: an agent
/// that names a directory and reads "committed" would otherwise believe a
/// change reached the scope when another machine's version is still there.
pub(crate) fn skipped_path_notes(skipped: &[RefusedCommitPath]) -> Vec<String> {
    if skipped.is_empty() {
        return Vec::new();
    }
    let mut notes = vec![format!(
        "dotsync: did not record {} file(s) under the paths you named, because this machine has not changed them",
        skipped.len()
    )];
    notes.extend(listed(skipped.iter().map(|skipped| {
        format!("{} ({})", skipped.path.display(), skipped.state.reason())
    })));
    notes.push(
        "dotsync: run `dotsync` to bring them up to date, or name one exactly to be told what happened to it."
            .to_string(),
    );
    notes
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
        PushReport::Unreachable { scopes, reason } => vec![
            format!(
                "dotsync: could not publish {} ({reason})",
                scopes.join(", ")
            ),
            "dotsync: those scopes are committed here and will be published by the next run that reaches the remote.".to_string(),
        ],
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
    notes.extend(drifts.iter().flat_map(render_drift_human));
    notes
}

pub(crate) fn render_drifts_human(drifts: &[FileDrift]) -> Vec<String> {
    drifts.iter().flat_map(render_drift_human).collect()
}

/// The path, why it is drift, and the diff. Naming the reason matters when
/// more than home moved: "deleted here" and "deleted here, and changed in the
/// repo on another machine" call for different resolutions.
fn render_drift_human(drift: &FileDrift) -> [String; 2] {
    [
        format!("- {} ({})", drift.repo_path.display(), drift.state.reason()),
        render_drift_diff(drift),
    ]
}

pub(crate) fn display_path(path: &Path) -> String {
    path.display().to_string()
}
