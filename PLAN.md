# dotsync — Plan

## Where things stand (2026-08-11)

- v0.3.12 released and installed on mc-wsl-fd. Development happens in the agent-tools workspace (`tools/dotsync`); the standalone `~/dotsync` clone is retired.
- Command surface on main: `dotsync` (sync), `init`, `commit <scope>`, `status`, `diff`, `view`, `continue`, `abort`. `--output json` everywhere. ~69 black-box tests, ratchet clean.
- The edit-in-place v0.3 model shipped: no visible staging area, bare repo at `~/.local/share/dotsync/repo/`, agents edit real files in `~/` and commit selected paths to scopes.
- Live dotfiles instance: remote `git@github.com:maxeonyx/dotfiles.git`, scope graph `all` → `home`/`work`/`linux`/`windows` → intersections (`home-linux`, `work-linux`, `home-windows`) → machines (`mc-wsl-fd`, `mx-vps-fd`, `mx-xps-cy`, `maxeonyx-pc-windows`).
- mc-wsl-fd has been wedged since 2026-07-27 by the failure described below ([#19](https://github.com/maxeonyx/dotsync/issues/19)).

## The recurring failure, told once properly

This is the third time dotsync has wedged a machine badly enough to threaten a dotfiles repo rebuild. All three times, the shape was the same. The 2026-07-27 incident is the cleanest example, so here it is step by step:

1. An agent added a fish tool and ran `dotsync commit linux -m "Add dev-certs: ..."`. This worked: commit `0e766ad` landed on `linux`, and the cascade merged it into all five descendant scopes. The jj transaction committed, so the local scope bookmarks all moved.
2. The run then failed somewhere between that transaction and the push. We don't know exactly where (plausibly the drift check inside the post-cascade sync, or a transient SSH failure at push — push is the last step, `src/commit.rs:336`). Whatever it was, the failure itself was recoverable in principle: the local repo was perfectly coherent, just ahead of the remote by one pushed-nothing changeset.
3. But dotsync's fetch reconciliation (`sync_local_bookmarks_from_remote`, `src/repo.rs`) treats *any* local bookmark that is not an ancestor of its remote as a fatal "fetch would overwrite local bookmark" error. A local bookmark that is **ahead** of the remote — the completely normal state of having an unpushed commit — falls into that bucket.
4. Every dotsync command fetches first, including read-only `status`, `view`, and `diff`. So from that moment, every single dotsync invocation on the machine failed with the same error.
5. No dotsync command can fix it. The error text says "publish or intentionally discard the local-only bookmark state" — but dotsync itself provides no way to do either. The only way out is raw jj/git surgery on the hidden repo, which is exactly the thing dotsync exists to make unnecessary, and which agents reliably get wrong (that's what caused the previous rebuild).

The specific bug is #19. But the pattern is the design gap:

**dotsync can create states that dotsync then refuses to operate on.**

## Design principle: no dead ends

Every state dotsync can produce — including states produced by crashing at the worst possible moment, and states produced by other machines pushing concurrently — must be a state that dotsync commands alone can diagnose and recover from. If a run is interrupted anywhere, the remedy must be "run dotsync again" (or `dotsync continue` / `dotsync abort` for a paused cascade). Never "go do repo surgery."

Concretely:

1. **Local-ahead is normal, not an error.** Per-bookmark fetch reconciliation has exactly four cases: (a) local == remote → nothing; (b) local behind → fast-forward; (c) local ahead → keep local, push when the command pushes; (d) truly diverged → this is a merge, not a wall.
2. **Divergence is a merge, not a wall.** DESIGN.md already says multi-machine changes to shared scopes are a core workflow. A diverged scope bookmark should be merged through the existing cascade/conflict machinery — pause with a real base/ours/theirs explanation if it conflicts (#17), resume with `dotsync continue`.
3. **Mutating commands self-heal.** Plain `dotsync` and `dotsync commit` should push any pending local-ahead bookmarks. An interrupted push heals on the next run of anything.
4. **Read-only commands never hard-fail on state.** `status`, `view`, and `diff` describe unusual state (unpushed scopes, divergence, paused cascade); they don't refuse to run because of it. They should also degrade gracefully when the remote is unreachable.
5. **Push order minimizes the stranded window.** Push scope updates as soon as the cascade transaction lands, before the home sync (which can legitimately stop on drift). A drift stop must not strand unpushed commits.
6. **Idempotent resume.** For every mutation sequence (commit tx → push → sync → sync-state save), every interruption point gets a defined convergent rerun. Kill-in-the-middle tests enforce this.

## Priorities and constraints (Max, 2026-08-12)

- **Prioritise conceptual simplification opportunities & the removal of bad modelling & incidental complexity. Anything that is robustly wrong or unnecessary is explicitly in scope to remove.** In practice: when a work item supersedes a mechanism, delete the old mechanism in the same item rather than layering; every review pass explicitly hunts for removal opportunities; known scaffolding (dead-end `NotImplemented` error, full home-dir scan in the commit path, the index-paired fake diff, the pause state file) gets deleted at the first item that touches it.
- **Dotfiles repo history**: if a change would need to modify the live dotfiles repo history, try to find another way first, or stop and tell Max — he'll have a separate agent do only that. dotsync agents never mutate the hidden repo or the remote dotfiles history by hand.
- **Work within the agent-tools workspace process** (`../../AGENTS.md`): improve process first, loop-based development, reviewer/implementer separation (the agent that made a fix cannot attest it), and after pushing dotsync main: update the workspace submodule pointer, regenerate the umbrella version file, and observe CI — reconcile it against intent, never chase green.

## Work plan (ordered)

The 2026-08-12 design review (with Max) rewrote DESIGN.md to specify the convergence model, conflicts-as-commits, the resolution surface, the failure/offline model, and the minimum-state principle. The work below implements that design.

### 1. Recovery release (#19) — do this first

Fix the four-case fetch reconciliation (local-ahead is normal, never an error), make mutating commands push pending local-ahead bookmarks, and reorder push before the home sync in the commit flow. Black-box tests: simulate a failed push (e.g. remote temporarily unwritable), assert the machine is not wedged (status works, next plain `dotsync` pushes and converges).

This is a targeted unwedge, not the full convergence pass — small, releasable, and verification is unusually good: mc-wsl-fd is live-wedged in exactly this state. After releasing and installing, plain `dotsync` on this machine should push the July 27 dev-certs cascade, sync, and leave `dotsync status` clean — with zero manual repo surgery. That's the acceptance test.

### 2. The convergence pass (#17 and the heart of the design)

Replace fetch-reconciliation + cascade with the single convergence operation from DESIGN.md: per scope in topo order, new head = merge(local head, remote head, updated parent heads), skip no-ops, push in a retry loop, offline skips fetch. Kill-in-the-middle black-box tests enforce that every interruption point converges on rerun — this is the enforcement mechanism for no-dead-ends, not a one-off audit.

### 3. Conflicts as commits

Write conflicted merges as real commits; derive "paused" from conflicted heads; delete `.dotsync-paused-cascade.json`; never push conflicted heads. Materialize conflict markers into home via sync (jj's own materialization code, scope-labeled sides, base included); drift treats materialized markers as expected content. `continue` verifies markers gone, resolves at the rootmost conflicted scope, propagates via descendant rewriting. `abort` returns to the last fully cascaded machine scope tip, abandoning only unpushed conflicted commits. Add `dotsync show conflict` rendering derived state.

### 4. Read-only robustness

`status`, `view`, `diff` work on any repo state — including conflicted heads and offline — and report weirdness instead of failing. Read-only commands report what convergence would do (in-memory merges), never mutate.

`status` must also report unpushed scopes. After item 1, a refused push is reported by the run that hit it and nowhere else: once that run's output scrolls away, a machine holding unpublished commits looks completely clean, and the honest-failure work of item 1 becomes invisible a minute later. Deferred here deliberately (found during the item 1 review) because it belongs with the rest of the read-only reporting surface.

### 5. Agent validation loop

Use the headless agent-scenario infrastructure (`tests/agent-scenarios/`) with a cheap model to validate the full UX: make a config change, run dotsync, resolve a conflict from markers, done. Add scenarios starting from an interrupted-push state and a conflicted-head state — the agent must recover using dotsync alone. Iterate on messages/UX until cheap agents reliably succeed. This is the actual product bar: the tool has failed in practice precisely when real agents met unplanned states.

### 6. Backlog triage

- PR [#15](https://github.com/maxeonyx/dotsync/pull/15) (scope lifecycle / add-scope): review against current main, land or close.
- Issues [#5](https://github.com/maxeonyx/dotsync/issues/5), [#8](https://github.com/maxeonyx/dotsync/issues/8), [#10](https://github.com/maxeonyx/dotsync/issues/10), [#11](https://github.com/maxeonyx/dotsync/issues/11): partially or fully addressed by PRs #14/#16 (`view`, `diff`, init/status UX) — verify and close or trim.
- Issues [#4](https://github.com/maxeonyx/dotsync/issues/4), [#18](https://github.com/maxeonyx/dotsync/issues/18): re-triage after steps 1–4; several items fall out of them naturally.

## Key design decision: conflicts are commits (2026-08-12 revision)

The old pause model ("don't create conflicted history; persist merge intent in a state file") is retired — it was a holdover from the v0.2 working-copy era and created dead ends. The current model is specified in DESIGN.md ("The convergence model" and "Conflict resolution in home"): the cascade always completes atomically, conflicted merges are real commits, conflicted heads are the queue of pending resolution work, "paused" is derived state, conflicted heads are never pushed, and the only machine-local state is sync-state.json. The `.dotsync-paused-cascade.json` file is to be removed as part of implementing this.

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
- Edit the conflicted files at their home locations to resolve the conflict (remove conflict markers, keep the desired content)
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
  "machine_scope": "mx-xps-cy",
  "unpushed_scopes": []
}
```

`unpushed_scopes` is emitted by every command that publishes (`init`, sync, `commit`, `continue`) and lists scopes that are committed on this machine but not on the remote — because the remote refused them, or because publishing was withheld while a cascade is paused. Empty means the remote has every scope commit this machine holds: not just the work this run created, but anything earlier runs left unpublished, which every publishing command republishes even when it has nothing of its own to add. `abort` does not publish and does not emit the field.

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

## Architecture notes

- `jj-lib` used as a library (not CLI subprocess) for all jj operations; jj CLI is not required on user machines
- Bare repo at `~/.local/share/dotsync/repo/`, no workspace/working copy; trees are read and written programmatically
- `dotsync init <remote-url>` bootstraps: clones, detects OS + hostname, creates scope branches, writes config, cascades, pushes, syncs config to system
- Machine scope comes from machine-local sync state (`.config/dotsync/sync-state.json` by default), not from a visible checkout
- `DOTSYNC_OS` and `DOTSYNC_HOSTNAME` env vars override OS/hostname detection (used in tests)
- App structured as: gather inputs → pure transforms → side effects
- Exit code 3 = cascade paused due to conflicts (distinct from 1 = error, 2 = usage)
- **Primary confidence signal is final home config**, not internal branch shape. Branch assertions support the home-config story; they do not replace it.

## Hard-won knowledge

- `git_target` file in `.jj/repo/store/` controls where jj-lib finds the git backend. Relative path. Non-colocated: `git`. Colocated: `../../../.git`.
- The hidden repo's git store can be inspected read-only with `git --git-dir ~/.local/share/dotsync/repo/.jj/repo/store/git ...` — invaluable for diagnosis, never for mutation.
- jj-lib API: `RepoLoader` opens a repo without a workspace. `Store::write_file()`, `MergedTreeBuilder`, `MutableRepo::new_commit()` are all workspace-independent.
- Build/test via devenv: `devenv shell cargo ratchet`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.
- Install after release: see AGENTS.md (gh release download + copy to `/usr/local/sbin` or `~/.local/bin` — note mc-wsl-fd currently uses `~/.local/bin/dotsync`, and an old 0.2.1 may still exist at `/usr/local/sbin/dotsync`).
- `render-dag.ignore.py` (local-only helper, recreate if needed) rendered step-by-step DAG simulations of cascade scenarios when validating the pause model.

## History (condensed)

- v0.2.x: hidden colocated repo era; first real-use bugs found and fixed via black-box tests; conflict pause/continue built.
- v0.3.0–0.3.2: the edit-in-place pivot (bare repo, no staging area), `status`, agent-scenario test infra. One-file-one-scope ownership was considered and **rejected** — it fights the branch-merge model.
- v0.3.3–0.3.12: command surface split (`commit <scope>` instead of bare scope arg), `diff`, `view`, `abort`, init/status UX overhaul, structured teaching-style error rendering (PRs #12–#14, #16).
- 2026-06-16: dotfiles repo rebuilt from scratch after a bookmark-divergence wedge (see `~/dotfiles/TASK-dotsync-rebuild.ignore.md` for the full story and the materialized-view rebuild process).
- 2026-07-27: mc-wsl-fd wedged by an interrupted commit push (#19) — the incident that produced the "no dead ends" principle above.
