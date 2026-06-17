# dotsync — Plan

## v0.3 direction: edit-in-place, no workspace

The fundamental model change: eliminate the user-visible staging area (`~/dotfiles`). Agents and users edit config files directly at their real locations (`~/.config/foo/bar.conf`), then run dotsync to commit selected paths to the right scope.

### Key changes

1. **No workspace/working copy.** jj-lib supports operating on a bare repo — read/write trees programmatically, never check out to disk. Store the repo at `~/.local/share/dotsync/repo/`.

2. **Edit-in-place workflow.** The source of truth flips: home directory is where edits happen, dotsync imports them into the repo. `dotsync commit <scope> -m "msg" -- <paths>` commits selected home paths to a scope.

3. **One-file-one-scope ownership (default).** Each managed file path is owned by exactly one scope. Enforced at commit time — reject if a path belongs to a different scope. Drop-in convention for machine-specific overrides (main config in parent scope, drop-in files in child scope). Escape hatch for monolithic files that can't be split.

4. **Cascade becomes mechanical.** If files can't conflict, cascade never pauses. Simplifies implementation dramatically.

5. **`dotsync status --output json`** — discovers home changes against last-synced state, groups by inferred scope. Planning surface for agents.

### What this eliminates

- `~/dotfiles/` as a visible directory (or it becomes just `.jj/` which we'd also hide)
- `.git` visible to agents (already done in v0.2.0)
- Merge conflicts during cascade (in the common case)
- The concept of "staging area" that agents must learn

### Open questions

- Migration path from existing `~/dotfiles` repos
- Scope ownership storage format (manifest on `all` scope? path-prefix rules?)
- Hunk splitting: defer as escape hatch, not primary UX
- What happens to `dotsync init`? Probably creates the hidden repo + registers the remote

## Current state (v0.2.x)

4 passing black-box CLI tests. Ratchet clean.

Working: `dotsync init`, `dotsync` (sync), `dotsync commit <scope> -m "msg"` (commit + cascade + sync + push), `--force`, `--output json` on all commands, drift detection, scope isolation, multi-machine via shared remote, merge cascade with conflict pause/resume via `dotsync continue`, full DAG traversal (all machines), return to home branch after cascade, structured JSON error/usage/drift/conflict output, rich human conflict messages with ASCII DAG rendering.

### Code structure
- `src/cascade.rs`: cascade domain model (`CascadeOutcome::{Completed, Paused}`), traversal, merge execution, `PersistedCascadeState`, `CascadeStateStore`, `ScopeDagRenderer` for human conflict messages
- `src/lib.rs`: command orchestration split into `prepare_commit_session()`, `validate_commit_scope()`, `commit_snapshot_and_apply_cascade()`, plus `continue_after_conflict()`. Structured `ErrorReport` for machine-facing errors with stable codes and drift details.
- `src/main.rs`: centralized output rendering — commands return typed payloads, one emitter handles JSON/human split. `continue` command wired up, exit code 3 for conflicts

## May 17 refactor

### Goal

Make `dotsync` the cleanest, most user-friendly, most straightforward, and most robust implementation of its core promise:

- the repo is the source of truth
- scopes are how shared vs specific config is expressed
- `dotsync` should let an agent make a change on one machine, commit it to the right ancestor scope, propagate it correctly, and materialize the right final config in each machine's home directory
- multi-machine changes to shared scopes like `all` must behave like normal repo history, never silently dropping or masking changes

The primary end-to-end thing we care about is **final config in home directories**, not internal branch mechanics. Branch/bookmark assertions matter, but they are secondary evidence. The main product test is: after a realistic series of scoped changes, do the right machines end up with the right config files in their virtual home directories?

### Background

The first real attempt to use dotsync on a single config migration exposed that the old tests were not trustworthy enough. We replaced them with black-box CLI tests and found real workflow bugs:

- plain `dotsync` was syncing dirty working-copy changes instead of rejecting them
- repeated ancestor-scope commits from a machine working copy hit incorrect drift detection on the second stage

Those bugs were fixed in the first sync-state refactor. The repo is now in a better place, but it still needs cleanup so the implementation matches the product story instead of depending on worktree accidents or overcomplicated jj state choreography.

### Current refactor priorities

- [x] Add failing black-box workflow tests for dirty sync rejection and repeated ancestor-scope commits from a machine working copy
- [x] Fix sync state handling so those black-box tests pass
- [ ] Make sync semantics fully repo → home, including deletions from home when files are removed from the repo
- [ ] Stop reading scope/config state from mutable filesystem fallbacks; read committed repo state instead
- [ ] Introduce machine-local sync metadata so dotsync knows what it last applied to a given home directory
- [ ] Improve error messages so each one stands alone and explains:
  - what dotsync is doing
  - what this flow expects
  - what current state it found
  - why it stopped
  - what correct flow to use next
- [ ] Add stronger black-box config-flow tests as the primary confidence story
- [ ] Review and harden multi-machine shared-scope behavior, especially concurrent/separate machine changes to `all`

### Primary test direction

The primary test suite should be black-box CLI flows that assert **config outcomes in virtual home directories** across scopes and machines.

Important flows to cover:

- add/update/delete config on `all`, `linux`, and machine scopes, then assert final home state
- commit a realistic sequence of changes from a machine working copy to an ancestor scope, asserting home state at each stage
- sync multiple machines with different scope paths and assert their homes differ only where they should
- multi-machine changes to `all`, including separate files, non-overlapping edits to the same file, and overlapping edits
- joining a repo after config/scopes changed elsewhere

### Conflict-flow tests

We need real black-box conflict tests, not just branch-shape assertions.

Required conflict flows:

- scope conflict flows where changes to different scopes produce a real merge conflict that must be resolved through the filesystem, followed by `dotsync continue`
- multi-machine conflict flows where different machines make conflicting changes and the pause/resolve/continue loop is exercised end-to-end

Those tests should assert, primarily, the final config in virtual home directories after resolution. Branch/bookmark state is secondary supporting evidence.

### Deletion and drift model

Deletion should be based on **previously managed paths**, not on naive absence alone.

Planned direction:

- dotsync should keep a machine-local metadata file in the home directory describing the last state it synced there
- that metadata is **not** a second config source; it is sync bookkeeping only
- the metadata file path itself should be configured in `config.toml`, because its location is part of dotsync correctness and should live with the rest of dotsync config
- the metadata file should be special-cased like internal dotsync state: never synced and never treated as managed dotfile content
- dotsync should only delete files from home when they were present in the last synced managed-path set for that home and are now absent from the desired machine-scope state
- drift detection should compare home state against the **last synced machine-scope state**, so dotsync can distinguish:
  - expected divergence because the repo advanced elsewhere since the last sync
  - unexpected local edits in home that differ from what dotsync last applied

This should make multi-machine behavior saner too: a machine should not treat "repo changed on another machine" as the same thing as "home drifted locally".

Refinement: keep the sync-state file itself minimal. It should store the last synced machine-scope revision, not duplicate managed paths, because managed paths should be derivable from the repo at that revision.

### Multi-machine review target

`dotsync` must naturally support multiple machines changing the same shared scopes by merging remote changes and then propagating those merges through descendants like any other scope change. This is not an edge case — it is a core workflow, especially for `all`.

Before trusting this workflow, we need both:

- black-box tests for machine A / machine B changes to `all`
- review of the fetch / local bookmark sync / commit / push flow to ensure remote changes cannot be silently dropped or overwritten by stale local state

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
- Or run `dotsync abort` to discard the paused cascade and restore the pre-pause state
- Note that the cascade may pause again at a later scope — this is normal, just repeat the process

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

## Test coverage (current top-level black-box flows)

Currently passing black-box workflow tests:

1. `plain_dotsync_rejects_working_copy_changes`
2. `ancestor_scope_commit_from_machine_working_copy_stays_consistent_across_stages`

This is a much better baseline than the older synthetic suite, but it is still far from enough. The next test work should expand black-box config flows across scopes, machines, deletions, and conflict resolution.

## Design decisions

- **`--output json`** on all commands. JSON is agent-facing product surface and needs black-box contract tests.
- **Conflict cascade flow** is interactive, like `git rebase` but walks the entire scope DAG:
  1. Cascade starts, walks descendants in topological order
  2. On conflict: pause, exit 3, print rich explanation + JSON
  3. User/agent edits files, runs `dotsync continue`
  4. Repeat until all scopes done
  5. End on the current machine's branch
- **Branches created on first commit to a scope** — scope must be in config, branch is created lazily.
- **All machines' branches get cascaded**, not just the current machine's path. Agent on machA may resolve conflicts on machB's branch.
- **Primary confidence signal is final home config**, not internal branch shape. Branch assertions support the home-config story; they do not replace it.

## Architecture notes

- `jj-lib` used as library (not CLI subprocess) for all jj operations
- `dotsync init <remote-url>` bootstraps: clones, detects OS + hostname, creates `all -> <os> -> <hostname>` scope branches, writes config, cascades, pushes, syncs config to system
- Current scope detected by finding which bookmark on the working copy commit is deepest in the DAG
- `DOTSYNC_OS` and `DOTSYNC_HOSTNAME` env vars override OS/hostname detection (used in tests)
- App structured as: gather inputs -> pure transforms -> side effects
- Exit code 3 = cascade paused due to conflicts (distinct from 1 = error, 2 = usage)

## Future work

- Delete the remaining filesystem-fallback/config-discovery assumptions that violate repo-is-source-of-truth
- Add deletion semantics so repo removals remove managed files from home
- Add black-box tests for multi-machine `all` changes, including explicit conflict cases and resolution with `dotsync continue`
- Add black-box tests for scoped config composition across multiple machines and multiple scope combinations
- Improve user-facing errors so each one teaches the correct mental model in isolation
- Set up `~/dotfiles` repo with scope branches per DESIGN.md
- Create the `dotfiles` opencode skill
- Populate dotfiles from configs in ~/SETUP-LOG.ignore.md
- Initial salvage from legacy `maxeonyx/dotfiles`: keep `.config/gitignore`, `.config/user-dirs.dirs`, and `mkc.fish`; drop legacy `shit.fish`, `mkclone.fish`, old bash `dotsync`, and stale fish/IPython/on-topic config
- Later: subsume Max's opencode config itself into dotfiles
