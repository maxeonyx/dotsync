use crate::UsageError;
use dotsync::{CascadePause, ErrorReport, FileDrift};
use serde_json::json;
use std::path::Path;

pub(crate) fn render_conflict_json(conflict: &CascadePause) -> serde_json::Value {
    json!({
        "status": "conflict",
        "scope": conflict.scope,
        "conflicted_files": conflict.conflicted_files,
        "scopes_done": conflict.scopes_done,
        "scopes_pending": conflict.scopes_pending,
        "original_scope": conflict.original_scope,
        "machine_scope": conflict.machine_scope,
        "parent_scopes": conflict.parent_scopes,
        "scope_dag": conflict.scope_dag,
    })
}

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

pub(crate) fn render_error_human(error: &ErrorReport) -> String {
    match error.code {
        "commit_selection_required" => render_structured_error(
            "commit selection required",
            "Dotsync keeps your dotfiles repo as the source of truth and commit mode records only the change you explicitly assign to a scope.",
            "This commit flow records selected working-copy paths on the chosen scope, cascades them through descendants, then syncs the resulting machine-scope state into home.",
            "Commit mode expects either explicit repo-relative file/directory paths or `--all`.",
            error
                .current_state
                .as_deref()
                .unwrap_or("commit mode requires explicit file/directory paths or --all"),
            "Dotsync stopped because implicitly committing the whole working tree is no longer allowed.",
            &[
                "Pass one or more repo-relative paths to say exactly what should be committed.",
                "Use `--all` only when you intentionally want every working-copy change committed.",
            ],
        ),
        "conflicting_commit_selection" => render_structured_error(
            "conflicting commit selection",
            "Dotsync commit mode requires explicit intent about which working-copy changes belong to the scoped commit.",
            "This commit flow accepts either explicit paths or `--all` for the commit selection.",
            "Commit mode expects one selection style, not both at once.",
            error.current_state.as_deref().unwrap_or(&error.message),
            "Dotsync stopped because `--all` and explicit paths conflict: one means the whole tree, the other means a subset.",
            &[
                "Keep the explicit paths if you want a selective commit.",
                "Remove the paths and use only `--all` if you intentionally want the whole working tree.",
            ],
        ),
        "commit_selection_empty" => render_structured_error(
            "empty commit selection",
            "Dotsync commit mode records only the paths you selected into the target scope, leaving unrelated dirty paths alone.",
            "This flow matches your explicit selection against current working-copy changes before creating the scoped commit.",
            "It expects the selection to cover at least one changed repo path.",
            error.current_state.as_deref().unwrap_or(&error.message),
            "Dotsync stopped because nothing in your selection would be committed.",
            &[
                "Choose paths that actually changed in ~/dotfiles.",
                "If you intended to commit every change, rerun with `--all`.",
            ],
        ),
        "commit_path_outside_repo" => render_structured_error(
            "commit path outside repo",
            "Dotsync commit mode only records paths from the dotfiles repo, because the repo is the source of truth.",
            "This flow resolves each selected path against ~/dotfiles before building the scoped commit.",
            "It expects every selected path to stay inside the repo root.",
            error.current_state.as_deref().unwrap_or(&error.message),
            "Dotsync stopped because one selected path points outside the repo and cannot be part of a repo-backed commit.",
            &[
                "Pass repo-relative paths from inside ~/dotfiles.",
                "If you meant a home path, translate it to the matching path inside ~/dotfiles first.",
            ],
        ),
        "commit_path_missing" => render_structured_error(
            "commit path missing",
            "Dotsync commit mode records only real repo paths from the current working-copy change set.",
            "This flow resolves your selected path against ~/dotfiles and the current dirty paths before creating the commit.",
            "It expects each selected path to exist in the repo or be a currently deleted repo path.",
            error.current_state.as_deref().unwrap_or(&error.message),
            "Dotsync stopped because one selected path does not correspond to any current repo path change.",
            &[
                "Check the path spelling and make sure the file or directory is inside ~/dotfiles.",
                "If you are selecting a deletion, pass the deleted repo-relative path exactly.",
            ],
        ),
        "config_base_scope_only" => render_structured_error(
            "config is base-scope only",
            "Dotsync stores the scope DAG in `.config/dotsync/config.toml`, so that file defines how every scope relates to every other scope.",
            "This commit flow allows that config file to be changed only on the base scope `all`.",
            "It expects non-`all` scope commits to avoid the scope-model config path entirely.",
            error.current_state.as_deref().unwrap_or(&error.message),
            "Dotsync stopped because committing the scope-model config on another scope would make branch-local scope definitions possible, which breaks the product model.",
            &[
                "Commit `.config/dotsync/config.toml` on `all`.",
                "Keep non-`all` scope commits focused on ordinary dotfiles, not scope-model changes.",
            ],
        ),
        "fetch_would_overwrite_local_bookmark" => render_structured_error(
            "fetch would overwrite local bookmark",
            "Dotsync fetches remote scope bookmarks before syncing or preparing a scoped commit so each machine sees published scope history from the shared repo.",
            "This fetch flow may fast-forward a local bookmark when the remote simply advances it, but it must not rewrite local bookmark history.",
            "It expects every remote bookmark update to either match the local bookmark or move it forward; dotsync must not move a local bookmark backward or sideways or lose unpublished local state.",
            error.current_state.as_deref().unwrap_or(&error.message),
            "Dotsync stopped because this remote update would move a local bookmark backward or sideways, which would discard or bypass unpublished local state.",
            &[
                "Publish or intentionally discard the local-only bookmark state before syncing.",
                "If the remote bookmark was rewritten intentionally, reconcile that history explicitly instead of letting dotsync reset the local bookmark.",
            ],
        ),
        "dirty_working_copy" => render_structured_error(
            "dirty working copy",
            "Dotsync keeps your dotfiles repo as the source of truth for your home-directory config and syncs committed repo state into the live system.",
            "Plain `dotsync` is sync-only: it checks the repo state for your machine scope and copies that committed state into your home directory.",
            "Plain `dotsync` expects a clean working copy with no uncommitted repo edits.",
            error
                .current_state
                .as_deref()
                .unwrap_or("working copy has uncommitted changes"),
            "Dotsync stopped because it cannot safely sync changes that have not been assigned to a scope and committed into the scope DAG.",
            &[
                "Put the change in the root-est appropriate scope with `dotsync <scope> -m \"message\"`.",
                "Use plain `dotsync` only after the repo working copy is clean.",
            ],
        ),
        "drift_detected" => render_structured_error(
            "drift detected",
            "Dotsync keeps your dotfiles repo as the source of truth for your home-directory config: the repo is the source of truth, and dotsync syncs committed repo state into the live system.",
            "This sync flow compares managed files in your home directory against the repo version for this machine scope before copying anything.",
            "Sync expects managed files in your home directory to already match the repo, unless you intentionally choose to overwrite drift.",
            "Drifted files are listed below with diffs.",
            "Dotsync stopped before overwriting local drift so you can inspect what would be replaced.",
            &[
                "If the repo is correct, rerun with `dotsync --force` to overwrite the drift after reviewing the diffs.",
                "If the live file is the change you wanted, recreate that change in ~/dotfiles on the correct scope and then run `dotsync <scope> -m \"message\"`.",
            ],
        ),
        "no_paused_cascade" => render_structured_error(
            "nothing to resume",
            "Dotsync manages scoped dotfile changes by committing to one scope and cascading that change through descendant scopes when needed.",
            "`dotsync continue` resumes a merge cascade after you resolve a paused conflict.",
            "`dotsync continue` expects a previously paused cascade waiting for resolution.",
            error
                .current_state
                .as_deref()
                .unwrap_or("no cascade is currently paused"),
            "Dotsync stopped because there is nothing to resume.",
            &[
                "Use `dotsync continue` only after a previous cascade paused on conflicts.",
                "That paused cascade usually comes from an earlier `dotsync <scope> -m \"message\"` run.",
                "Otherwise start a new change with `dotsync <scope> -m \"message\"` or run plain `dotsync` for sync-only mode.",
            ],
        ),
        "cascade_in_progress" => render_structured_error(
            "already paused",
            "Dotsync manages dotfiles by recording a change on one scope and cascading it through related descendant scopes until every affected machine scope is updated.",
            "This commit flow records the working-copy change on the selected scope, continues any required cascade, then syncs the final machine-scope state into home.",
            "This flow expects no earlier cascade to still be paused.",
            &error.message,
            "Dotsync stopped because another cascade is already paused and starting a new dotsync command now would mix two incomplete flows.",
            &[
                "resolve the paused scope in ~/dotfiles and then run `dotsync continue`.",
                "Do not start another dotsync command until that resume finishes.",
            ],
        ),
        "invalid_scope" => render_structured_error(
            "invalid scope",
            "Dotsync stores dotfiles in a scope DAG so shared config can live on shared ancestor scopes and machine-specific config can stay isolated on leaf scopes.",
            "This commit flow records your working-copy change on the scope you name and then cascades it through descendant scopes.",
            "It expects the scope you name to exist in the configured scope DAG.",
            &error.message,
            "Dotsync stopped because it cannot place this change onto a scope that is not configured.",
            &[
                "choose a real configured scope from the DAG.",
                "Pick the root-est appropriate ancestor scope that should own the change.",
            ],
        ),
        "scope_not_ancestor" => render_structured_error(
            "not an ancestor",
            "Dotsync uses a scope DAG so each machine inherits shared config from ancestor scopes and keeps unrelated branch lineages separate.",
            "This commit flow records your working-copy change onto one scope in your current machine's lineage and then cascades it downward.",
            "It expects the chosen scope to be the current machine scope or one of its ancestors.",
            &error.message,
            "Dotsync stopped because committing to a non-ancestor scope would let this machine write into an unrelated branch lineage.",
            &[
                "choose `mx-pc-win` or one of its ancestors instead.",
                "If the change really belongs to another lineage, make it from a machine in that lineage.",
            ],
        ),
        "sync_state" => render_structured_error(
            "invalid sync state",
            "Dotsync keeps the repo as the source of truth and uses a local sync-state file to remember which machine scope was last synced here and which revision that sync used.",
            "This sync flow reads that local state to know which prior managed files may need removal and which machine scope should be treated as authoritative for this home.",
            "It expects that state file, if present, to be valid and readable; it expects that state file, if present, to be valid.",
            &error.message,
            "Dotsync stopped because it cannot safely decide what prior sync state to trust.",
            &[
                "fix or delete the bad sync-state file and rerun the command.",
                "After that, let dotsync recreate valid sync state from a successful sync.",
            ],
        ),
        _ => format!("dotsync: {}", error.message),
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

pub(crate) fn render_conflict_human(conflict: &CascadePause) -> String {
    let conflicted_files = conflict
        .conflicted_files
        .iter()
        .map(|path| format!("- {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    let colliding_scopes = conflict.parent_scopes.join(", ");

    format!(
        "dotsync: cascade paused due to conflicts\n\nDotsync is propagating a config change through the scope branch DAG so shared changes reach every affected machine. Different scopes exist because some config is shared across all machines, some is shared by subsets like an OS or desktop environment, and some is machine-specific. This pause happened because the same file was changed differently on branches that now need to be merged.\n\nScope DAG:\n{}\n\nPaused at scope: {}\nMerging changes from {} into {}\nConflicted files:\n{}\n\nWhat to do next:\n- Edit the conflicted files in ~/dotfiles/ and remove the conflict markers, keeping the content you want in this paused scope.\n- Run `dotsync continue` to resume the cascade.\n- The cascade may pause again on a later scope; if it does, repeat this process.\n\nAgent notes:\n- The scope you are resolving may belong to another machine; that is expected because dotsync cascades through every affected descendant scope.\n- When the cascade finishes, dotsync returns you to your machine scope: {}.\n- Do not run other dotsync commands while this cascade is paused.\n- `dotsync abort` is planned but is not implemented yet in this build.",
        conflict.scope_dag,
        conflict.scope,
        colliding_scopes,
        conflict.scope,
        conflicted_files,
        conflict.machine_scope,
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
