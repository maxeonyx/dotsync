use crate::{HumanOutput, SuccessOutput};
use dotsync::{
    ConflictedFile, Explanation, FileChange, FileDrift, FileState, PushReport, SkipReason,
    SkippedCommitPath, SyncReport, UnreachableRemote,
};
use serde_json::json;
use similar::TextDiff;
use std::path::{Path, PathBuf};

/// Output for a command whose job is to bring home up to its machine scope:
/// `init`, plain `dotsync`, `continue`, and `abort`. They differ in what they
/// did to get there and in nothing else they report, so they answer in one
/// shape rather than in four copies of it.
///
/// `push` is `None` only for `abort`, which is the one of the four that
/// publishes nothing — so a command that does publish cannot quietly omit what
/// it left unpublished.
pub(crate) fn synced_output(
    command: &str,
    headline: String,
    sync: &SyncReport,
    push: Option<&PushReport>,
) -> SuccessOutput {
    let mut json = json!({
        "status": "ok",
        "command": command,
        "machine_scope": sync.current_scope,
        "synced_files": display_paths(&sync.synced_paths),
        // The one thing a sync does that cannot be undone. A drift that
        // reached this report is a drift the run was allowed to overwrite —
        // anything else stopped it — so this is exactly the home content this
        // run discarded. Named for what happened to the file rather than for
        // the command, because `discard`, `init` and `abort` all do it.
        "overwritten_files": display_paths(
            &sync.drifts.iter().map(|drift| drift.repo_path.clone()).collect::<Vec<_>>(),
        ),
        // The opposite of `overwritten_files`: home content this run kept and
        // merged around. Reported because the run succeeded *and* left this
        // machine holding uncommitted work, and an agent reading only the
        // headline would have no way to know the second half.
        "carried_changes": changes_json(&sync.carried_changes),
    });
    if let Some(push) = push {
        json["unpushed_scopes"] = json!(push.unpushed_scopes());
    }
    let mut notes = carried_change_notes(&sync.carried_changes);
    notes.extend(success_notes(&sync.drifts, push));
    SuccessOutput {
        json,
        human: HumanOutput::Message(headline),
        notes,
        exit_code: 0,
    }
}

/// What a sync merged around and left in home. Said out loud on a run that
/// worked, because "synced 4 file(s)" and exit 0 otherwise reads as "this
/// machine agrees with its scopes now", and it does not.
fn carried_change_notes(carried: &[FileChange]) -> Vec<String> {
    if carried.is_empty() {
        return Vec::new();
    }
    let mut notes = vec![format!(
        "dotsync: carried {} local change(s) through the sync; they are still only in home",
        carried.len()
    )];
    notes.extend(
        carried
            .iter()
            .map(|change| render_change_line(&change.path, change.state)),
    );
    notes.push(
        "dotsync: commit them with `dotsync commit <scope> -m \"message\" -- <path>`, or run `dotsync status` to see them again."
            .to_string(),
    );
    notes
}

pub(crate) fn display_paths(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|path| display_path(path)).collect()
}

/// What a read-only command says about a cascade it found paused.
///
/// A note rather than part of the answer, so it reaches a caller in both
/// output formats and arrives before the answer it qualifies: on a machine
/// with a paused cascade, "no changes" is true and misleading at once, because
/// nothing can be committed and nothing is being published.
pub(crate) fn paused_cascade_notes(paused_cascade: Option<&String>) -> Vec<String> {
    let Some(scope) = paused_cascade else {
        return Vec::new();
    };
    vec![
        format!("dotsync: a cascade is paused at scope `{scope}`; this machine cannot commit and is publishing nothing until it is resolved"),
        "dotsync: edit the conflicted files in home to the merged contents you want and run `dotsync continue`, or run `dotsync abort` to discard the cascade.".to_string(),
    ]
}

/// What a read-only command says about a scope it found contested.
///
/// A note for the same reason a paused cascade is one: it qualifies the answer
/// rather than being it. What the reader has to know is that the answer
/// describes a state the next writing run will change — a contested head is an
/// input to a merge that has not happened yet.
pub(crate) fn diverged_scope_notes(scopes: &[String]) -> Vec<String> {
    if scopes.is_empty() {
        return Vec::new();
    }
    vec![
        format!(
            "dotsync: {} diverged: this machine and the remote each hold commits the other does not",
            quoted_scopes(scopes)
        ),
        "dotsync: the next `dotsync`, `dotsync commit` or `dotsync continue` merges them, and this answer describes the state before that merge.".to_string(),
    ]
}

/// What a read-only command says about scopes this machine holds and the
/// remote has never seen.
///
/// A note for the same reason the other two are: it qualifies the answer
/// rather than being it. "No changes" is true of home and false of the
/// machine, because the work is committed here and nowhere else.
pub(crate) fn unpushed_scope_notes(scopes: &[String]) -> Vec<String> {
    if scopes.is_empty() {
        return Vec::new();
    }
    vec![
        format!(
            "dotsync: {} committed here and not on the remote",
            quoted_scopes(scopes)
        ),
        "dotsync: the next `dotsync` publishes them; until it does, no other machine can see this work.".to_string(),
    ]
}

fn quoted_scopes(scopes: &[String]) -> String {
    scopes
        .iter()
        .map(|scope| format!("`{scope}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One changed file, for a machine. The same object wherever dotsync reports a
/// file that differs from what the scopes hold: `status`, `diff`, and the
/// drift a run stopped on. `state` is the code to branch on; `reason` is the
/// same thing in words, so nothing has to keep a table of codes to read it.
pub(crate) fn change_json(path: &Path, state: FileState) -> serde_json::Value {
    json!({
        "path": display_path(path),
        "state": state.code(),
        "reason": state.reason(),
    })
}

pub(crate) fn changes_json(changes: &[FileChange]) -> Vec<serde_json::Value> {
    changes
        .iter()
        .map(|change| change_json(&change.path, change.state))
        .collect()
}

/// The same object a changed file gets, because "why is this path not in the
/// commit" is the same question `status` answers about a file — and a reason
/// that is about the path rather than its content is still a reason.
pub(crate) fn skipped_paths_json(skipped: &[SkippedCommitPath]) -> Vec<serde_json::Value> {
    skipped
        .iter()
        .map(|skipped| {
            json!({
                "path": display_path(&skipped.path),
                "state": skipped.reason.code(),
                "reason": skipped.reason.explain(),
            })
        })
        .collect()
}

/// One changed file, for a person: a marker to scan for, the path, and the
/// reason in words so the marker never has to be guessed at. `status` and
/// `diff` are answering the same question, so they say it the same way.
pub(crate) fn render_change_line(path: &Path, state: FileState) -> String {
    format!(
        "  {} {} ({})",
        change_marker(state),
        display_path(path),
        state.reason()
    )
}

fn change_marker(state: FileState) -> &'static str {
    match state {
        FileState::EditedInHome | FileState::EditedInHomeButRemovedFromRepo => "M",
        // A kind difference is the same shape of change as an edit — home holds
        // something the scope does not — so it reads as one.
        FileState::KindDiffersFromScope => "M",
        // So is an edit the repo has moved under: home holds a change of this
        // machine's own, and the reason line is where "and so does the repo"
        // belongs, because nothing about it needs resolving.
        FileState::DivergedEditThatMerges => "M",
        FileState::DeletedInHome | FileState::DeletedInHomeTipAlsoChanged => "D",
        FileState::DivergedEdit | FileState::IncomingNewCollidesWithUntrackedHome => "C",
        // The same marker, for the same reason: two versions of this file are
        // waiting for somebody to choose. This one is on a scope rather than
        // between home and a scope.
        FileState::AwaitingMerge => "C",
        FileState::IncomingNew => "A",
        FileState::StaleNotYours => "U",
        FileState::RemovedFromRepo => "R",
        // Not reported: `status` and `diff` only ever render drift and incoming
        // changes.
        FileState::UntrackedInHome
        | FileState::IncomingNewAlreadyMatchesHome
        | FileState::AlreadyApplied
        | FileState::InSync
        | FileState::RemovedEverywhere
        | FileState::AbsentEverywhere => " ",
    }
}

pub(crate) fn render_error_json(explanation: &Explanation) -> serde_json::Value {
    let mut json = json!({
        "status": "error",
        "error": explanation.code,
        "message": explanation.message,
        "conflicts": explanation.conflicts.iter().map(render_conflict_json).collect::<Vec<_>>(),
        "current_state": explanation.current_state,
    });
    // Present only when the run met the state, under the name `status`, `diff`
    // and `view` already answer with — an agent that reads it off a successful
    // report reads it off a stop the same way.
    if let Some(scope) = &explanation.paused_cascade {
        json["paused_cascade"] = json!(scope);
    }
    json
}

/// One file a merge could not resolve, with every version of it: the version
/// both sides changed, then each side, each under the name of where it came
/// from.
///
/// Every version is here rather than a rendered three-way diff, because an
/// agent resolving a conflict writes the merged file — and what it needs for
/// that is the content, not a description of how the content differs. The
/// versions are not in home, so this and the human rendering beside it are the
/// only place they are.
pub(crate) fn render_conflict_json(file: &ConflictedFile) -> serde_json::Value {
    json!({
        "path": display_path(&file.path),
        "versions": file.versions.iter().map(|version| json!({
            "role": version.role.code(),
            "label": version.label,
            // `null` rather than an empty string: a version that does not hold
            // the file at all is one side having added it or deleted it, which
            // is not the same fact as it being empty.
            "contents": version.contents.as_ref().map(|bytes| String::from_utf8_lossy(bytes)),
        })).collect::<Vec<_>>(),
    })
}

/// The same versions for a person. Contents are printed unindented under a
/// header naming the version, so a line can be copied out of one of them into
/// the file in home without picking indentation back off it.
pub(crate) fn render_conflicts_human(files: &[ConflictedFile]) -> Vec<String> {
    let mut lines = Vec::new();
    for file in files {
        let path = display_path(&file.path);
        lines.push(match file.state {
            Some(state) => render_change_line(&file.path, state),
            // A merge home is not part of: there is nothing to say about the
            // file beyond which one it is, and every version of it follows.
            None => format!("  C {path}"),
        });
        for version in &file.versions {
            lines.push(format!(
                "--- {path} | {}: {} ---",
                version.role.code(),
                version.label
            ));
            lines.push(match &version.contents {
                Some(bytes) => String::from_utf8_lossy(bytes)
                    .trim_end_matches('\n')
                    .to_string(),
                None => "(this version does not have the file)".to_string(),
            });
        }
    }
    lines
}

/// A changed file with the two sides shown. Exactly the object `status`
/// reports for the same file, plus the diff — which is the whole of what
/// `diff` adds to `status`.
pub(crate) fn render_drift_json(drift: &FileDrift) -> serde_json::Value {
    let mut json = change_json(&drift.repo_path, drift.state);
    json["diff"] = json!(render_drift_diff(drift));
    json
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

/// A stop, rendered for a person: the teaching block if the error has one, and
/// its one line if it does not.
///
/// A formatter and nothing else. What each stop says is the error's own, so
/// there is no per-variant knowledge here to fall out of step with the payload
/// beside it.
pub(crate) fn render_error_human(explanation: &Explanation) -> String {
    let Some(teaching) = &explanation.teaching else {
        return format!("dotsync: {}", explanation.message);
    };
    let correct_flow = teaching
        .next_steps
        .iter()
        .map(|step| format!("- {step}"))
        .collect::<Vec<_>>()
        .join("\n");
    // The facts a stop found, as one block. No facts means the error's own
    // message is all there is to say.
    let current_state = match explanation.current_state.is_empty() {
        true => explanation.message.clone(),
        false => explanation.current_state.join("\n"),
    };

    format!(
        "dotsync: {}\n\nWhat dotsync does:\n{}\n\nThis flow:\n{}\n\nExpected:\n{}\n\nCurrent state found:\n{current_state}\n\nWhy dotsync stopped:\n{}\n\nCorrect flow:\n{correct_flow}",
        teaching.summary,
        teaching.what_dotsync_does,
        teaching.this_flow,
        teaching.expected,
        teaching.why_stopped,
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
pub(crate) fn skipped_path_notes(skipped: &[SkippedCommitPath]) -> Vec<String> {
    if skipped.is_empty() {
        return Vec::new();
    }
    let mut notes = vec![format!(
        "dotsync: did not record {} file(s) under the paths you named",
        skipped.len()
    )];
    notes.extend(listed(skipped.iter().map(|skipped| {
        format!("{} ({})", skipped.path.display(), skipped.reason.explain())
    })));
    if skipped
        .iter()
        .any(|skipped| matches!(skipped.reason, SkipReason::NotChangedHere(_)))
    {
        notes.push(
            "dotsync: run `dotsync` to bring those up to date, or name one exactly to be told what happened to it."
                .to_string(),
        );
    }
    notes
}

/// Notes printed to stderr alongside a successful run: what was overwritten,
/// and what did not reach the remote. `push` is `None` only for commands that
/// do not publish at all, so a publishing command cannot quietly omit this.
pub(crate) fn success_notes(drifts: &[FileDrift], push: Option<&PushReport>) -> Vec<String> {
    let mut notes = push.map(push_notes).unwrap_or_default();
    notes.extend(notes_for_drifts(drifts));
    notes
}

pub(crate) fn push_notes(push: &PushReport) -> Vec<String> {
    match push {
        PushReport::UpToDate => Vec::new(),
        PushReport::Refused { scopes, rejection } => vec![
            format!(
                "dotsync: the remote refused {} ({})",
                scopes.join(", "),
                rejection.reason()
            ),
            "dotsync: those scopes are committed here but not published, so the remote does not have this change yet. The next run will try again.".to_string(),
        ],
        PushReport::Unreachable { scopes, reason } => vec![
            format!(
                "dotsync: could not publish {} ({reason})",
                scopes.join(", ")
            ),
            "dotsync: those scopes are committed here and will be published by the next run that reaches the remote.".to_string(),
        ],
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
    notes.extend(render_drifts_human(drifts));
    notes
}

/// Changed files with their two sides shown: the same line `status` and `diff`
/// give each file, then the diff under it.
///
/// One renderer, because these are the same files those commands report. A
/// drift stop used to render them as `- path (reason)`, which is also how the
/// teaching errors render their instruction bullets — so the files a run
/// stopped on read as more things to do.
pub(crate) fn render_drifts_human(drifts: &[FileDrift]) -> Vec<String> {
    drifts
        .iter()
        .flat_map(|drift| {
            [
                render_change_line(&drift.repo_path, drift.state),
                render_drift_diff(drift),
            ]
        })
        .collect()
}

pub(crate) fn display_path(path: &Path) -> String {
    path.display().to_string()
}
