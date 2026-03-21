# dotsync — Plan

## Current state

12 passing + 3 pending black-box CLI tests. Ratchet clean.

Working: `dotsync init`, `dotsync` (sync), `dotsync <scope> -m "msg"` (commit + cascade + sync + push), `--force`, `--output json`, drift detection, scope isolation, multi-machine via shared remote.

Not working: real merge cascade (bookmarks move but no merge commits), conflict resolution flow, `dotsync continue`.

## Immediate TODO

- [x] Fix ratchet history (rewrote git history so tests go through pending → passing)
- [x] Update 3 pending tests to use `--output json` instead of parsing stderr
- [x] Write conflict message requirements (see below)
- [ ] Implement real merge cascade with interactive conflict resolution (`dotsync continue`)
  - Create branches for new scopes on first commit to that scope
  - Real merge commits (not bookmark moves) so conflict resolutions are preserved
  - Interactive pause on conflict: exit code 3, persisted cascade state
  - `dotsync continue` resumes cascade after user resolves conflicts
  - Cascade walks entire DAG (all machines, not just current)
  - Returns to current machine's branch when done
  - Persisted cascade state so `continue` knows where it left off
- [ ] `--output json` on all commands — JSON on stdout for machine consumption, human text on stderr
- [ ] Change config to be read from system path (`~/.config/dotsync/config.toml`), not repo path
- [ ] Manually verify conflict messages contain everything an agent needs

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
