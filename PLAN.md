# dotsync — Plan

## Current state

13 passing + 2 pending black-box CLI tests. Ratchet clean. Commit `d727175` on main.

Working: `dotsync init`, `dotsync` (sync), `dotsync <scope> -m "msg"` (commit + cascade + sync + push), `--force`, `--output json`, drift detection, scope isolation, multi-machine via shared remote. Basic conflict pause/resume works (diamond test passes). `dotsync continue` exists and works for simple cases.

Partially working: merge cascade with conflict detection and pause/resume. The diamond test (test 1 of 3) passes. Tests 2 and 3 fail because **merge history is not preserved correctly** — resolved merges don't create the right ancestry for downstream cascades to reuse.

### Code structure after refactoring (commit `58826be`)
- `src/cascade.rs`: cascade domain model (`CascadeOutcome::{Completed, Paused}`), traversal, merge execution, `PersistedCascadeState`, `CascadeStateStore`
- `src/lib.rs`: command orchestration split into `prepare_commit_session()`, `validate_commit_scope()`, `commit_snapshot_and_apply_cascade()`, plus `continue_after_conflict()`
- `src/main.rs`: `--output json` emits JSON for success/error/conflict, `continue` command wired up, exit code 3 for conflicts

## Immediate TODO

- [x] Fix ratchet history (rewrote git history so tests go through pending → passing)
- [x] Update 3 pending tests to use `--output json` instead of parsing stderr
- [x] Write conflict message requirements (see below)
- [x] Pre-implementation refactoring: extract cascade engine into `src/cascade.rs`, split `commit_and_sync()` into phases
- [x] Diamond cascade test passing (basic pause/resume works)
- [ ] **Fix merge-history preservation** (tests 2 and 3) — see "Root cause of remaining failures" below
- [ ] `--output json` on all commands — JSON on stdout for machine consumption, human text on stderr
- [ ] Change config to be read from system path (`~/.config/dotsync/config.toml`), not repo path
- [ ] Manually verify conflict messages contain everything an agent needs

## Root cause of remaining failures

Tests 2 and 3 fail for the same reason: **the pause/resume mechanism contaminates branch ancestry.**

The current implementation creates a temporary conflicted commit and moves the bookmark to it during pause. When `continue` runs, it tries to reconstruct the "real" merge from `paused_head_hex` — but the temporary commit is already in the branch history, defeating jj's ability to reuse earlier merge resolutions.

### The correct model (confirmed with Max via DAG visualization)

**In jj, the working copy IS always a commit and can be in a conflicted state.** This is the key simplification.

- **On pause:** Do NOT create any permanent commit or move any bookmark. The working copy commit `@` simply becomes the conflicted merge result. Persist the intended merge parents (exact commit IDs) and remaining cascade steps.
- **On continue:** The user has edited `@` to resolve conflicts. Snapshot `@`'s tree, create the real merge commit with the persisted parent IDs, move the bookmark, continue the cascade.
- **A pause is workspace state + persisted intent, not history.**

### DAG visualization tool

`~/dotsync-b/render-dag.ignore.py` renders correct DAG graphs for each step of both test scenarios. Run `python3 render-dag.ignore.py` to see the full step-by-step simulation. The commit parent relationships encoded in that script are confirmed correct by Max.

Key property test 2 checks: After resolving `all→linux` (creating L2), the `linux→machine` merge (M3) is CLEAN because M2 already recorded the L1+M1 resolution — jj can reuse that merge ancestry.

Key property test 3 checks: The cascade walks ALL descendants (both linux and windows sides), resolving conflicts on each, and returns the working copy to the originating machine's branch when done.

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
- Run `dotsync abort` to undo the cascade and go back to the state before the commit
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
  "message": "scope `nonexistent` does not exist"
}
```

## Pending tests (drive the implementation)

1. `diamond_cascade_resolves_conflicts_across_multi_parent_merge` — diamond scope graph (all → a, all → b, a+b+linux → machine), conflicting changes to a and b, resolve on machine, verify
2. `recorded_conflict_resolution_survives_subsequent_cascade` — chain (all → linux → machine), commit to machine then conflicting to linux (resolve), then conflicting to all (resolve at linux, machine should be clean because first resolution is in merge history)
3. `multi_machine_cascade_resolves_other_machines_conflicts_and_returns_home` — two machines (linux/windows), scope-specific changes then conflicting commit to all, resolve other machine's branch, verify you end up on your own branch, verify other machine syncs cleanly

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
