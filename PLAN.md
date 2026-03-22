# dotsync — Plan

## Current state

18 passing black-box CLI tests. Ratchet clean. Clippy clean.

Working: `dotsync init`, `dotsync` (sync), `dotsync <scope> -m "msg"` (commit + cascade + sync + push), `--force`, `--output json` on all commands, drift detection, scope isolation, multi-machine via shared remote, merge cascade with conflict pause/resume via `dotsync continue`, full DAG traversal (all machines), return to home branch after cascade, structured JSON error/usage/drift/conflict output, rich human conflict messages with ASCII DAG rendering.

### Code structure
- `src/cascade.rs`: cascade domain model (`CascadeOutcome::{Completed, Paused}`), traversal, merge execution, `PersistedCascadeState`, `CascadeStateStore`, `ScopeDagRenderer` for human conflict messages
- `src/lib.rs`: command orchestration split into `prepare_commit_session()`, `validate_commit_scope()`, `commit_snapshot_and_apply_cascade()`, plus `continue_after_conflict()`. Structured `ErrorReport` for machine-facing errors with stable codes and drift details.
- `src/main.rs`: centralized output rendering — commands return typed payloads, one emitter handles JSON/human split. `continue` command wired up, exit code 3 for conflicts

## Immediate TODO

- [x] Fix ratchet history (rewrote git history so tests go through pending → passing)
- [x] Update 3 pending tests to use `--output json` instead of parsing stderr
- [x] Write conflict message requirements (see below)
- [x] Pre-implementation refactoring: extract cascade engine into `src/cascade.rs`, split `commit_and_sync()` into phases
- [x] Diamond cascade test passing (basic pause/resume works)
- [x] Fix merge-history preservation (tests 2 and 3) — pause is now workspace state + persisted intent, not history
- [x] `--output json` on all commands — JSON on stdout for machine consumption, human text on stderr. Structured error codes for usage, drift, and runtime errors. Rich human conflict messages with ASCII DAG.
- [ ] Change config to be read from system path (`~/.config/dotsync/config.toml`), not repo path
- [ ] Manually verify conflict messages contain everything an agent needs (initial implementation done — needs Max's eye)

## Key design decision: pause model

**In jj, the working copy IS always a commit and can be in a conflicted state.** This is the key simplification.

- **On pause:** Do NOT create any permanent commit or move any bookmark. The working copy commit `@` simply becomes the conflicted merge result. Persist the intended merge parents (exact commit IDs) and remaining cascade steps.
- **On continue:** The user has edited `@` to resolve conflicts. Snapshot `@`'s tree, create the real merge commit with the persisted parent IDs, move the bookmark, continue the cascade.
- **A pause is workspace state + persisted intent, not history.**

### DAG visualization tool

`render-dag.ignore.py` (in any clone) renders correct DAG graphs for each step of both test scenarios. Run `python3 render-dag.ignore.py` to see the full step-by-step simulation. The commit parent relationships are confirmed correct.

## Conflict message requirements (human-readable, verified manually)

The conflict message is the most important piece of text in dotsync. It's what an AI agent sees when a cascade pauses. It must teach the agent the entire mental model from scratch, because the agent may have no prior context about dotsync's scope system.

The message MUST contain all of the following:

### Context (why this is happening)
- **What dotsync is doing**: propagating a config change through scope branches so all machines stay in sync
- **Why there are multiple branches**: different machines/OSes share some config and have some unique config; scopes organize this into a branch hierarchy
- **Why this conflicts**: the same file was changed differently on two branches that now need to be merged

### Current state (where we are in the cascade)
- **The scope DAG** rendered as ASCII art, with markers showing:
  - Which scope the original commit was on
  - Which scopes have been cascaded successfully (done)
  - Which scope is currently conflicted (paused here)
  - Which scopes are still pending
- **The conflicted scope name**
- **The conflicted files** with paths relative to repo root
- **Which scopes' changes are colliding** (e.g. "merging changes from `all` into `linux`")

### Instructions (what to do)
- Edit the conflicted files in `~/dotfiles/` to resolve the conflict (remove conflict markers, keep the desired content)
- Run `dotsync continue` to resume the cascade
- Note that the cascade may pause again at a later scope — this is normal, just repeat the process
- (`dotsync abort` is planned but not yet implemented)

### Agent-specific guidance
- The scope being resolved may be a different machine's branch — this is expected and necessary
- After the cascade completes, you'll be back on your machine's branch
- Don't run other dotsync commands while a cascade is in progress

## JSON output contract (`--output json`)

All commands emit JSON on stdout when `--output json` is passed. Human-readable messages go to stderr regardless.

### Conflict pause (exit code 3)
```json
{
  "status": "conflict",
  "scope": "mx-xps-cy",
  "conflicted_files": [".shellrc", ".config/fish/config.fish"],
  "scopes_done": ["linux"],
  "scopes_pending": ["mx-xps-cy", "hyprland"],
  "original_scope": "all",
  "machine_scope": "mx-xps-cy"
}
```

### Success (exit code 0)
```json
{
  "status": "ok",
  "command": "commit",
  "scope": "all",
  "synced_files": [".gitconfig", ".shellrc"],
  "machine_scope": "mx-xps-cy"
}
```

### Error (exit code 1)
```json
{
  "status": "error",
  "error": "invalid_scope",
  "message": "scope `nonexistent` does not exist"
}
```

Stable error codes include: `invalid_scope`, `drift_detected`, `no_paused_cascade`, `not_initialized`, etc. Drift errors include a `drifts` array with per-file details.

### Usage error (exit code 2)
```json
{
  "status": "error",
  "error": "usage",
  "message": "missing required argument: -m <message>"
}
```

## Test coverage (18 tests)

Cascade conflict tests (all passing):
1. `diamond_cascade_resolves_conflicts_across_multi_parent_merge` — diamond scope graph, conflicting changes, resolve on machine
2. `recorded_conflict_resolution_survives_subsequent_cascade` — chain with merge history preservation
3. `multi_machine_cascade_resolves_other_machines_conflicts_and_returns_home` — two machines, cross-machine conflict resolution

JSON output contract tests (all passing):
4. `json_usage_error_is_emitted_for_missing_commit_message` — usage errors emit structured JSON
5. `json_continue_without_pause_reports_structured_error` — runtime errors have stable codes
6. `json_drift_error_includes_drift_details` — drift errors include per-file detail

## Design decisions

- **`--output json`** on all commands. Tests use JSON to check mechanical state. Human-readable messages are the default and are verified manually, not in automated tests.
- **Conflict cascade flow** is interactive, like `git rebase` but walks the entire scope DAG:
  1. Cascade starts, walks descendants in topological order
  2. On conflict: pause, exit 3, print rich explanation + JSON
  3. User/agent edits files, runs `dotsync continue`
  4. Repeat until all scopes done
  5. End on the current machine's branch
- **Branches created on first commit to a scope** — scope must be in config, branch is created lazily.
- **All machines' branches get cascaded**, not just the current machine's path. Agent on machA may resolve conflicts on machB's branch.

## Architecture notes

- `jj-lib` used as library (not CLI subprocess) for all jj operations
- `dotsync init <remote-url>` bootstraps: clones, detects OS + hostname, creates `all -> <os> -> <hostname>` scope branches, writes config, cascades, pushes, syncs config to system
- Current scope detected by finding which bookmark on the working copy commit is deepest in the DAG
- `DOTSYNC_OS` and `DOTSYNC_HOSTNAME` env vars override OS/hostname detection (used in tests)
- App structured as: gather inputs -> pure transforms -> side effects
- Exit code 3 = cascade paused due to conflicts (distinct from 1 = error, 2 = usage)

## Future work

- Set up `~/dotfiles` repo with scope branches per DESIGN.md
- Create the `dotfiles` opencode skill
- Populate dotfiles from configs in ~/SETUP-LOG.ignore.md
