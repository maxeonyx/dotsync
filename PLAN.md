# dotsync — Plan

## Where things stand (2026-08-12)

- Development happens in the agent-tools workspace (`tools/dotsync`); the standalone `~/dotsync` clone is retired. Current released version: check `gh release list --repo maxeonyx/dotsync` or `dotsync --version` — don't record it here, it rots.
- Command surface on main: `dotsync` (sync), `init`, `commit <scope>`, `status`, `diff`, `view`, `continue`, `abort`. `--output json` everywhere. Black-box tests via the TDD ratchet, clean.
- The edit-in-place v0.3 model shipped: no visible staging area, bare repo at `~/.local/share/dotsync/repo/`, agents edit real files in `~/` and commit selected paths to scopes.
- Live dotfiles instance: remote `git@github.com:maxeonyx/dotfiles.git`, scope graph `all` → `home`/`work`/`linux`/`windows` → intersections (`home-linux`, `work-linux`, `home-windows`) → machines (`mc-wsl-fd`, `mx-vps-fd`, `mx-xps-cy`, `maxeonyx-pc-windows`).
- Work item 1 below shipped in v0.3.13 ([PR #20](https://github.com/maxeonyx/dotsync/pull/20)): mc-wsl-fd, wedged since 2026-07-27 ([#19](https://github.com/maxeonyx/dotsync/issues/19)), was unwedged on 2026-08-12 by plain `dotsync` with zero repo surgery. Follow-up on that machine: 11 drifted files accumulated while wedged still need `dotsync commit <scope>` decisions.

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
- **The cull applies to the product surface, not just internals** (Max, 2026-08-12): "not just gurky internals, but sharp product edges too. Remember - me, and mainly agents with no memory, are the only users. So self-documentation is *critical* and backwards compatibility (on the frontend, not the backend though (ie. the repo / scope model)) is an *antipattern* on this project." When a flag, command shape, message, or JSON field is a sharp edge, redesign it outright rather than preserving it for compatibility. The repo/scope model (the backend) is where compatibility matters.
- **Method for the cull: back-chain smells to root causes.** Don't fix smells where they present; trace each to the modelling decision that produced it and fix that, accepting potentially large refactorings. Smell inventory and root-cause analysis live in the "Smells and root causes" section below.
- **Dotfiles repo history**: if a change would need to modify the live dotfiles repo history, try to find another way first, or stop and tell Max — he'll have a separate agent do only that. dotsync agents never mutate the hidden repo or the remote dotfiles history by hand.
- **Work within the agent-tools workspace process** (`../../AGENTS.md`): improve process first, loop-based development, reviewer/implementer separation (the agent that made a fix cannot attest it), and after pushing dotsync main: update the workspace submodule pointer, regenerate the umbrella version file, and observe CI — reconcile it against intent, never chase green.

## Work plan (ordered)

The 2026-08-12 design review (with Max) rewrote DESIGN.md to specify the convergence model, conflicts-as-commits, the resolution surface, the failure/offline model, and the minimum-state principle. The work below implements that design.

### 1. Recovery release (#19) — ✅ shipped in v0.3.13 ([PR #20](https://github.com/maxeonyx/dotsync/pull/20))

Four-case fetch reconciliation (local-ahead is normal), mutating commands publish pending bookmarks (even no-op commits), push before the home sync, honest push reporting (`PushReport` enum + `unpushed_scopes` JSON), a publish guard while a cascade is paused, and an honest `scope_diverged` error in place of the old wedge (divergence-as-merge itself is item 2). Also fixed en route: true divergence used to silently reset the local bookmark and delete home files with exit 0.

Live acceptance passed: plain `dotsync` pushed the July 27 dev-certs cascade from mc-wsl-fd and stopped honestly on the 16 days of accumulated home drift without stranding anything. Item numbering below is stable — items 2–6 keep their numbers because reviews, commit messages, and DESIGN notes refer to them.

### 1.5 The cull campaign — smells back-chained to root causes (2026-08-12, in progress)

Two independent smell reviews (product surface driven live in a sandbox; implementation structure) produced ranked inventories. Everything below is back-chained to the modelling decision that produced it. Delivery is in waves, each wave releasable alone; smells whose *root* fix is items 2/3/4 are tracked into those items instead of patched here.

**Two data-loss paths found (beyond the scheduled work), reproduced end-to-end:**

- **(DL-1) A machine that is merely *behind* can silently revert another machine's pushed work.** `status` on the stale machine shows the remote's change as `M` (drift); committing that path with `dotsync commit` re-records the stale content and cascades it, exit 0 everywhere. Root cause: naming a path conflates *selection* ("include this") with *authority* ("home wins"), and drift is computed one-sided against the machine tip instead of three-way against the last-synced tree. Fixed by Wave 1.
- **(DL-2) `continue` accepts an unresolved conflict.** Today's pause never materializes markers into home, so "markers gone" is vacuously true; `continue` takes home's unchanged content as the resolution and silently deletes the losing side. Root fix is item 3 (conflicts-as-commits with real materialization); interim honesty guard in Wave 0 (refuse when the conflicted home files are untouched since the pause). The item-3 note below records the severity.

**Wave 0 — deletions and sharp-edge removal (small, fast):** the `NotImplemented` dead-end + full home-directory walk (pulled forward from item 2's first-commit slot); the never-constructed `ConcurrentScopeConflict` error variant and its exit-3/JSON/render arms; `cascade.rs` scaffolding (`CascadePlan`/`CascadeProgress`/`CascadeSuccess`/`execute_cascade_plan`/single-variant `CommandOutcome`, plus the never-read `completed_scopes`→`cascaded_scopes`/`created_scopes` data); `Action::Status{force:false}` dead param and `--force` accepted-but-ignored on `status`; `--output` made global (a clap usage error must still honor the JSON contract on stdout); `NotImplemented(&str)` grab-bag split into real errors (`HOME` unset is not "not implemented"); tokio current-thread + de-async the one `async fn` with no `await`; DL-2 interim guard; commit-path validation (typo'd/`~`-prefixed/no-match paths are errors, not silent `committed 0 file(s)` with empty-string JSON fields).

**Wave 1 — one drift model (the deepest root cause). ✅ implemented on `MC-wave1-drift-authority`.** `src/drift.rs` holds the classifier (15 variants: the full cross of presence for last-synced/home/tip, collapsed by equality). The sync gate, `status`, `diff` and `commit` selection all read it; `expected_repo_changes` and its 7 sites are gone; `FileDrift` carries both sides instead of a rendered diff and `similar` renders a real unified diff at the edges; sync writes only when bytes differ; `--force` on `commit` scopes to the named selection and reports `forced_overwrites`.

Four things from the Wave 1 design did not survive contact, and are worth knowing before the next wave:

- **D5's recommended fallback was wrong.** The design said a missing/stale sync state should stand in `L := T` per path "preserving today's tested fallback exactly". It does not: it makes every file the scope holds and home does not look like a deletion made here — which is every file on a fresh `init`, and every unsynced file on a machine whose state file was lost. The shipped fallback is an *empty* last-synced side, which does reproduce the old behaviour exactly. Caught by running the binary, not by the suite.
- **The classification domain needed a 15th variant** the design's enum lacked: present in home only, absent from both the last-synced tree and the tip (`UntrackedInHome`). That is every brand-new file a commit adds, so it is not an edge case.
- **`--force` on `abort` became meaningless.** D7 kept a blanket boolean there, but once abort does the full restore DESIGN.md specifies, it always overwrites and the flag can never change the answer. `abort` now refuses `--force`, exactly as `init` already did for the same reason. Both are commands that write home without ever making the overwrite choice.
- **`--all` on `commit` was deleted** (Wave 3's item, pulled forward). Under one selection rule it was a second name for the no-paths default, and keeping two identical code paths is the incidental complexity this wave exists to remove. `CommitSelection` went with it.

**D6 remains open and nothing in Wave 1 changed it.** Committing to a scope outside this machine's scope ancestry keeps the old plain assignment (`commit_merge_base_tree` falls back to the target head), because home was never derived from such a scope and there is no version of it this machine can claim to have started from.

Original statement of the wave: Today four implementations answer "what differs between home and repo" with three different baselines: sync gates on the last-synced tree, `status`/`diff` compare the machine tip (so a routine remote advance shows as `M`+exit 1 while plain `dotsync` syncs it happily), commit has its own, and the `expected_repo_changes` suppression list threaded through 5 call sites exists only to paper over the one-sidedness. Replace all of them with a single three-way drift authority (last-synced tree / home / target tip — DESIGN's table) used by status, diff, sync, *and commit selection*. This makes DL-1 unrepresentable (a home file equal to last-synced but behind the tip is "stale, not yours" — commit refuses with a teaching message), makes deletion drift visible (was item 2's fourth bullet, moves here), deletes the suppression threading, and reconciles status/diff/sync. Absorbs former item 2.5: structured drift (`FileDrift` stops carrying a pre-rendered `diff: String`; render at the edges), real LCS diff (root of #8), write-only-when-bytes-differ. Also `--force` becomes per-path by construction: force rides the same selection list that names the commit paths, and forced overwrites are recorded in JSON (today `--force` on a commit reverts *unrelated* drifted files and the JSON says nothing).

**Wave 2 — a session, one fetch, offline degradation. ✅ implemented on `MC-wave2-session`.** `src/session.rs` holds the run: paths, the repo handle, the scope graph read out of it, and whether the run reached the remote. 18 `load_repo_direct` call sites became 2 — the session's own, and the one `init` needs because jj only shows an added remote to a freshly opened repo handle; `load_config` stopped opening repos and now runs once per run; transactions hand their committed repo back with `advance_to`, which deleted the "the push may have written an operation, re-open to see it" reload and both `let _repo = ...` discards. `view`'s four shapes became one `view()` returning a `ViewReport`, so its overview loop reads trees already in hand: 5 `git fetch` subprocesses became 1 on a 4-scope graph (locked in by a test that counts `git` invocations through a PATH shim). A fetch that cannot reach the remote is now `RemoteUnreachable` rather than a generic jj error, and the session's one fetch site catches it: read-only commands report against the last-fetched state, mutating commands build local-ahead history on it, push gained a matching `Unreachable` report, and every run says which state it answered from (a notice, plus `remote_unreachable` in JSON) — on the arm that stopped as well as the arm that finished, because every command runs inside `in_session`, which wraps the whole outcome as `Run<Result<T, E>>` so a `?` cannot carry the report past the run's own facts. N-B landed: a named directory records what this machine changed under it, adds what is new under it, and steps around what another machine changed, reporting both the files it left alone (`skipped_paths`) and the files it started tracking (`newly_tracked`); refusal stays for paths named exactly; `--force` still reaches under a named directory. The two bulk shapes are *not* equivalent and the docs no longer say they are: naming no paths records only changes to already-tracked files, so it can never add one — which is what makes a directory the way new config reaches a scope in bulk.

Things worth knowing before the next wave:

- **`init` still fails on an unreachable remote, and now cleans up after itself.** Reaching the remote is the whole of init's job, so it cannot degrade — but the half-created repo it used to leave behind made `repo already exists` the answer to the retry, which is a dead end DESIGN forbids. A failed init now removes the repo root it made, and says so when it cannot. That last path is not black-box reachable (making `remove_dir_all` fail needs the repo root to exist inside an unwritable parent, and init creates both in one run); it was verified by forcing the cleanup to fail in a local build.
- **jj does not export the type inside `GitFetchError::Subprocess`**, so dotsync cannot tell "host did not resolve" from "permission denied" from "your git is too old for this option". All of them are treated as "this run did not reach the remote" and git's own words are quoted in the notice. If that ever needs to be finer, it needs an upstream export, not string matching.
- **Read-only commands still mutate when they *can* reach the remote** — the fetch commits a transaction that moves bookmarks. That is item 4's work and was deliberately not taken on here; what changed is that there is now exactly one site to change.
- **`load_repo_direct` and `fetch_origin` are still `pub(crate)`.** Only the session and `init` may sensibly call them, but Rust cannot say "these two modules", and the alternatives — moving them into `session.rs`, or inventing a session shape for a repo that has no `all` scope yet — buy nothing or cost a lot. Left as is deliberately.
- **The review round after this wave added five fixes on top of it.** A run's remote state now reaches the arm that stopped as well as the arm that finished (`in_session` wraps the whole outcome as `Run<Result<T, E>>`, and `CliOutput` carries the fact beside the arms rather than inside two of them) — before that, the commonest offline stop, drift, advised `--force` against a snapshot of unknown age and said nothing. `-- .` no longer sweeps the whole home directory onto a scope (verified beforehand: `.ssh/id_ed25519` and `.netrc` reached the remote, exit 0, silently), and re-gate found the symlinked spelling of the same sweep, now refused with the directory walk's guards hardened behind it. A trailing separator on a named file no longer leaks `jj operation failed: invalid repo path`. A commit reports what it started tracking. And one fetch per run is now enforced for five commands rather than only `view`.
- **Symlinks: treat them as files, and do not follow them (Max, 2026-08-13).** His words: *"Yeah I guess for almost all intents we should treat symlinks as files and not follow them"*. So a link's content, as far as dotsync is concerned, is its target string: `commit` records a symlink as a symlink, sync materialises it in home as a symlink again, and no call site reads or writes *through* a link. On Windows (Max, 2026-08-13): *"don't worry about windows. we should probably reject symlinks on windows scopes"*. Commit selection's blanket refusal was the conservative placeholder for this answer, and it is what closed the sweep found in re-gate: `ln -s $HOME $HOME/selflink; dotsync commit all -m msg -- selflink/` published 72 files including `.ssh/id_ed25519`, `.netrc`, `sync-state.json` and the whole hidden `.jj` store, exit 0. Under "do not follow" that sweep stays closed for a different reason — `selflink` is one symlink entry, not a directory to walk — and `~/.config/nvim` pointing at a checkout elsewhere becomes recordable as the link it is.

- **The symlink state to implement against, verified by hand against released 0.3.19: five call sites, four policies.** `commit <path>` where the path is or traverses a link is **refused** (`CommitPathProblem::Symlink`, `commit.rs` `resolves_elsewhere`). The directory walk `commit -- dir/` **skips** the link and reports it in `skipped_paths`. The drift read behind `status`/`diff` **follows** it silently: `drift.rs` `read_home_bytes` uses `fs::metadata`, which resolves links, so the guard added for FIFOs passes a link through and `fs::read` returns the *target's* bytes. The sync write **follows** it silently: `sync.rs` `write_home_file` uses `fs::write`, which writes through the link. And repo → home turns a `TreeValue::Symlink` into a **regular file containing the target string**, because `repo.rs` `read_repo_file` returns `target.into_bytes()` and `write_home_file` writes those bytes as content. Two of those were reproduced end to end: a symlink pushed onto `all` by a plain git client and carried down by a cascade materialised in home as a regular file whose contents were the four characters `./real`; and with `toolA` recorded as a regular file and then replaced in home by a link to the managed `toolB`, `dotsync --force` wrote toolA's recorded content **into toolB**, clobbering a different managed file.

- **Six red tests for the symlink decision are committed pending (2026-08-13); nothing is implemented.** All in `tests/user_flows.rs`, each failing for its own reason, quoted here as they actually fail:
  - `a_symlink_on_a_scope_materialises_in_home_as_a_symlink` — home holds a regular file whose contents are `real.conf`. The sharpest one, and reachable without lifting the commit refusal: a plain git client puts the link on `all`, an ordinary cascade carries it down.
  - `a_sync_replaces_a_home_symlink_instead_of_writing_through_it` — after `dotsync --force`, the unmanaged file the link pointed at holds `ui = light\n` instead of its own contents. That assertion is the data loss.
  - `commit_records_a_symlink_as_a_symlink` — refused with `unusable_commit_paths`, exit 1.
  - `a_symlink_to_a_sibling_script_survives_the_round_trip_to_another_machine` — machine B holds nothing at all at the link's path, because the directory walk skipped it on machine A. Deliberately named as a directory selection so it fails on the round trip rather than on the refusal above.
  - `status_and_diff_report_a_kind_difference_between_a_link_and_a_file` — `status` answers `{"changes":[]}`, exit 0, for two paths that differ from the scope in kind. Both fixtures hold exactly the bytes the scope holds, so content comparison cannot see them.
  - `a_symlink_to_home_records_one_entry_rather_than_sweeping_home` — refused, exit 1, the same refusal as `commit_records_a_symlink_as_a_symlink`. It cannot be red for its own reason today, because that refusal is the only thing closing the leak; it is a regression guard for after T3 removes it.

- **Implementing the commit half will break three tests that currently assert the refusal**: `a_symlink_pointing_at_home_cannot_be_used_to_sweep_it`, `a_symlinked_selection_path_is_refused_whether_it_is_a_file_or_a_directory`, and `a_symlink_under_a_named_directory_is_reported_rather_than_silently_skipped`. They encode the placeholder policy, not the decided one; expect to rewrite or delete them in the same commit, and expect `SkipReason::Symlink` and `CommitPathProblem::Symlink` to go with them.

- **The Windows half of the symlink decision has no test, and is deferred to [#28](https://github.com/maxeonyx/dotsync/issues/28) (Max, 2026-08-13, as scope expansion).** It grew past "reject symlinks on Windows scopes" once Max pointed out that committing a symlink to `all` is the same problem — the constraint on content is the union of what every leaf scope it can reach is able to represent, which makes `all` the *most* constrained scope rather than the least, and brings executable bits, non-UTF-8 text files and binaries in with it. The issue holds the model, the checks he named, and the one open question (whether the constraint comes from machine leaves that actually exist, or from the OS scopes present in the graph). It depends on the symlink work below landing first: recording a symlink has to work before rejecting it on the wrong scope means anything.
- **`commit.rs` is 1285 lines and its split is unscheduled.** `commit_and_sync` alone runs to ~250, and `continue_after_conflict` repeats most of its pause-handling. Wave 3 does not touch it and items 2 and 3 rewrite parts of it; whoever gets there first should decide whether the split happens before or as part of that work, rather than it happening by accident.
- **One unexplained test flake was seen and could not be reproduced.** `selected_add_modify_and_delete_are_applied_without_touching_unselected_changes` failed once under `cargo ratchet` during this wave and passed 16 consecutive runs afterwards (10 full-suite, 12 isolated), including on the code from before the change that was in flight. Its output was lost. The harness runs several `git` subprocesses per fixture, so a transient subprocess failure is the likeliest cause; recorded here so the next person to see it knows it is not new.
- **Working offline makes item 2 more reachable, which is the point and also the cost.** Committing offline while another machine commits to the same scope leaves both sides ahead, and the next online run reports `scope_diverged` (exit 1, nothing lost, pointing at issue #17) because the convergence pass that would merge them is item 2. Verified by hand: machine A commits to `all` offline, machine B publishes to `all`, A reconnects and gets the honest divergence error with its local commit and its home file intact. Before this wave the same machine could not have made the commit at all, so the exposure is new; the state it produces is one item 2 already owes an answer for.

Original statement of the wave: `&DotsyncPaths` is passed everywhere and every helper re-opens the repo (`dotsync commit` opens it ~7 times; `dotsync view` fetches N+1 times — 10 git subprocesses for 9 scopes) and every read-only command *mutates* (fetch commits a transaction that moves bookmarks), contradicting DESIGN. Introduce a run-scoped session: open once, fetch once, and network failure degrades (read-only commands report against last-known state with a notice; mutating commands proceed offline per DESIGN) instead of `jj operation failed: fetch origin`. This is the substrate item 2's convergence loop needs; item 4's full never-mutate reporting still lands in item 4.

Original statement of N-B (found reviewing Wave 1): **the two bulk selections do not behave alike.** `dotsync commit <scope> -m msg` with no paths filters to the files this machine changed, so it steps around anything the repo moved on without home. `dotsync commit <scope> -m msg -- .config/fish/` expands the directory and then refuses the whole commit if any file under it is, say, one another machine deleted. Both are "commit what changed under here"; a directory selection should filter the same way a bare commit does, and reserve refusal for paths named individually — naming one path exactly is the claim that deserves an argument. Deferred rather than done in Wave 1 because it changes what an explicitly named path means, and the refusal machinery had just been introduced.

**Wave 3 — presentation and self-documentation coherence. ✅ implemented on `MC-wave3-presentation`.** The shipped JSON schema is documented in full, one example per command, under "JSON output contract" below — that section now describes what the code does rather than what an older design intended. What changed: `synced_output` replaced the five hand-built `json!` blocks that had drifted apart, and `scope` stopped repeating `machine_scope` on the commands that only sync; `abort` reports `paused_scope`, which is neither the scope that was aborted nor the scope the discarded commit was on. `CommitReport.recorded` is an `Option`, so a commit that records nothing — which runs no cascade and no home sync — reports `outcome: "nothing_to_commit"` with no `synced_files` at all, instead of a default-constructed `SyncReport` whose empty `machine_scope` agents were reading; `SyncReport` lost `Default` so that substitution cannot come back. `SuccessOutput` grew a `HumanOutput` enum in place of the `human`/`stdout` precedence that made every `view` arm set `human: String::new()`. `status` and `diff` answer with the same `changes` array of the same `{path, state, reason}` objects and the same header and per-file line, `diff` adding the diff; `groups` and the two counts are gone. `ErrorReport.current_state` is a `Vec<String>` — one fact per entry, joined only for humans. Exit 3 became a property of the state (`DotsyncError::is_paused_cascade`, exhaustive) rather than of the command that met it, and the table is in `--help`, in DESIGN's Commands section, and in `docs/SKILL.md`. `init` writes a commented `config.toml` and a joining machine *edits* that file instead of re-rendering it from the parsed graph. `view` validates its scope argument, so an unknown scope gets `commit`'s teaching error instead of "does not have a local bookmark"; a file that is not on a scope is `file_not_on_scope` instead of "jj operation failed"; `MissingScopeBookmark` is `ScopeNotInRepo`; the `Jj` variant keeps jj's detail in its chain but its headline and its code (`internal`) are dotsync's.

Things worth knowing before the next wave:

- **The config file is now edited, not generated, and that is load-bearing in both directions.** `config_with_scopes` adds only the scopes this machine needs and preserves everything else, because the second machine to run `init` used to delete every comment in the file — including the ones the first machine's `init` had just written. `render_config` is gone. This adds `toml_edit` as a direct dependency; it was already in the lock via `toml`, so it costs no new compilation. The consequence to remember: **the config file is no longer a pure function of the scope graph**, so anything that wants to reorganise scopes has to edit the document rather than rebuild it.
- **`view` was left with four JSON shapes under one command name.** It was not in this wave's list and fixing it properly means answering whether those are one command at all; `scopes` still changes type between two of them and `contents` is still UTF-8 lossy for binary files. Recorded in the JSON contract section so nobody mistakes it for coherent.
- **`--force` is still the only thing keeping `status`/`diff` from being the same command.** They now agree on population, objects, header and per-file rendering; the difference in output is the diff string, and the difference in behaviour is that `diff` exits 1 when it finds anything. That is a `--verbose` flag's worth of difference, which is why the open question below is filed rather than answered.
- **`N file(s)` was left alone.** The singular/plural fix went to the two messages that read as grammatical errors ("cannot commit 1 of the paths you named"); `file(s)` is a deliberate compact convention used in a dozen places and changing it is churn, not coherence.
- **Nothing was done about `commit.rs`'s size.** It is still ~1400 lines and its split is still unscheduled; this wave added `DirectoryWalk` to it. See the Wave 2 note.
- **Only the structural half of "does the headline match the outcome" was settled.** A report can no longer claim something the run did not do: the no-op commit cannot describe a sync it never ran, a stop cannot hide what it overwrote, and the exit code is derived from the state rather than chosen per command. The *behavioural* half is open and is K2 under item 3 — a `continue` that resolves a conflict outside this machine's ancestry succeeds, pushes, and then exits 1 with `drift_detected`, leaving the machine permanently "changed" in `status`. No amount of presentation work fixes that one; the run really does end in two states at once.
- **The review round after this wave added nine fixes on top of it.** The blocker: `dotsync bogus --output json` emitted nothing at all on stdout, because clap's unknown-command arm swallows the flag — empty stdout with exit 2, for the mistake an agent makes most. Then: usage payloads gained the three collections every other error carries; the drift stop's file list stopped rendering as `- ` bullets immediately below the `Correct flow:` bullets and now uses the one changed-file rendering under a heading of its own; `status` and `diff` report a paused cascade in both channels, because "no changes" was the answer they gave on a machine that could not commit at all; `not initialized` names the command that was run instead of telling everybody to rerun `dotsync status`, and both it and `repo already exists` joined the teaching-error shape; `dotsync commit -- <fifo>` no longer hangs forever (`fs::read` on a fifo blocks, and `read_home_bytes` reads every managed path on every run, so a tracked file replaced by a fifo could stop `status` too); and `view --file` on a path no scope holds says so instead of printing two headings and nothing between them.

Original statement of the wave: JSON schema unified across commands (no empty-string `scope`/`machine_scope` from default-constructed reports; four near-identical `json!` blocks collapsed; `SuccessOutput`'s implicit `human`/`stdout` precedence untangled); `status`/`diff` merged or reconciled (Wave 1 made them agree on *what* is drift; the remaining question is whether two commands are wanted at all); exit-code table made coherent and documented in `--help`; `init` generates the commented `config.toml` DESIGN calls load-bearing (today it's comment-free, so the documented scope-discovery mechanism yields nothing); DESIGN.md Commands section corrected (says "one command" then lists eight, omits `init` and `status`); `docs/SKILL.md` re-verified against actual behavior; user-facing text purged of `bookmark`/`jj` vocabulary (DESIGN: "abstracts jj away entirely").


**Tracked into later items (root fix belongs there):**

- Item 2: divergence-as-merge, push retry loop, rejection classification. (Item 2's first-commit deletion slot is consumed by Wave 0.)
- Item 3: DL-2's root fix — "resolved" must be a property of content, not of having run `continue`; the interim Wave-0 guard is deleted with the pause file. Note: the exit-3 pause JSON contract documented below **does not exist in the code today** (actual output is the generic error JSON; `conflicted_files` is stringly-joined into the message) — item 3 *builds* the contract rather than changes it.
- Item 4: read-only commands never mutate (in-memory merge reporting), `status` reports unpushed scopes.
- Item 2: **`ScopeHeads` is a mutable shadow of the repo's bookmarks, kept correct by convention.** Every place that moves a scope head has to write both `tx.repo_mut().set_local_bookmark_target(...)` and `scope_heads.update(...)` — `cascade.rs:157`, `commit.rs:371`, `commit.rs:1240`, `bootstrap.rs:251` — and nothing enforces the pair. Miss one and the in-memory head and the repo bookmark disagree for the rest of the transaction, and the cascade merges the wrong parent. That is the same shape as the push-eligibility invariant item 1's reviewer flagged: a safety property that holds because someone remembered to write one line next to another. Item 2 rewrites `cascade.rs` anyway, so it is where to either make one call do both or delete the cache and read heads from `mut_repo.view()`, which is already the authority. Riding along: the same DAG is ordered twice, by `descendants_in_topological_order` (`cascade.rs:166`, O(n²) through `ordered.contains` on a `Vec`) and by `scope_depth` (`scope_graph.rs:75`, memoised, used for display), when both could be computed once where the graph is parsed.
- Item 2: **the black-box harness wants extracting before the kill-in-the-middle matrix is written, not after.** `tests/user_flows.rs` is 6,579 lines holding three things — the harness, the scenarios, and a quote-aware word splitter (`dotsync_args`) that exists only so a test can write its command as one string. `assert!(output.status.success(), "{}", render_output(&output))` appears 148 times. The harness also carries a second jj-lib client (`load_repo_direct`, `bookmark_commit`, `seed_remote_scope_file`, `merge_remote_scope_into`, `interrupt_push_after_cascade`) that has to be kept in step with `src/repo.rs` by hand. Item 2 promises a matrix of interruption points across several flows; written into this shape that is N more copies of the preamble and the assert idiom. A `run_ok` that asserts success and returns the output, plus a builder for the `init` + seed + merge + sync preamble that a dozen tests repeat verbatim, is the cheaper order.
- Item 3: **the drift stop is still blind to a paused cascade.** Reproduced on v0.3.18: with a cascade paused at `linux` and the conflicted file still sitting in home, plain `dotsync` exits 1 with `drift_detected` and offers the two remedies it always offers — rerun with `--force`, which would overwrite the in-flight resolution, and "run `dotsync status`, then commit the intended path", which is refused with exit 3 precisely because a cascade is paused. Neither the human rendering nor the JSON payload carries `paused_cascade`, though `status`, `diff` and `view` all gained it in the Wave 3 review round. So the command an agent runs by reflex is the one still handing it two wrong answers.
- Item 4: **`view` answers neither orientation question.** The overview lists `box1` and `box2` identically with nothing saying which one is this machine, though `status` knows; and `view --file <path>` lists every scope holding the file — the owner plus every descendant, because files propagate down — when the useful answer is "owned by `all`". Reading the second correctly needs the DAG-propagation knowledge the agent was using `view` to acquire. Both are derivable (the machine scope is in sync state, the owner is the rootmost scope holding the file) and neither is computed, so `view` renders the backend instead of answering the question. Separate from the four-shapes problem recorded in the JSON contract section below.
- No item owns these two. **`DotsyncError`'s user-facing material is spread across three hand-maintained matches** — `to_error_report` and `error_current_state` in `error.rs`, `render_error_human` in `render.rs`, plus the exhaustive `is_paused_cascade` — so adding a variant means editing three or four places, and a variant whose teaching text is never reachable is possible (item 1's `ConcurrentScopeConflict` was exactly that, and Wave 0 deleted it). Putting the material on the variant — one `fn explain(&self) -> Explanation` returning the structured teaching parts — makes that unrepresentable; items 2 and 3 each edit those matches, so consolidating first means each of them edits one place instead of three. And **`dotsync commit <scope> -m "" -- <path>` is accepted, exit 0**: an empty description lands permanently in shared history with nothing to say what it was.
- Windows path-separator suspect: the directory walk (`DirectoryWalk`, formerly `collect_home_directory_files`) builds relatives with `read_dir` separators and feeds `from_internal_string` — would produce a tree entry literally named `.config\fish\config.fish` for directory-selection commits on Windows. Inferred from source, unconfirmed — needs a Windows run before item 2 multiplies the conversion sites. The same decision shows up in output: `render::display_path` is `Path::display()`, so JSON on Windows would carry backslash paths in some fields and forward slashes in others. Worth checking on the same run, though no review claimed it: `Canonical` (`commit.rs`) decides a selection path "resolves elsewhere" by comparing what `canonicalize()` returns against `home.join(relative)`, and canonicalization on Windows returns the on-disk casing while the joined path keeps whatever the agent typed — so on a case-insensitive filesystem `dotsync commit all -- .APPRC` may look like a link to somewhere else and be refused. Scope-name/`RepoPathBuf` typing (15 conversion sites, `"all"` hardcoded 8×) rides along with whichever wave touches those seams first.

### 2. The convergence pass (#17 and the heart of the design)

Replace fetch-reconciliation + cascade with the single convergence operation from DESIGN.md: per scope in topo order, new head = merge(local head, remote head, updated parent heads), skip no-ops, push in a retry loop, offline skips fetch. Kill-in-the-middle black-box tests enforce that every interruption point converges on rerun — this is the enforcement mechanism for no-dead-ends, not a one-off audit.

Deletions this item performs (expanded 2026-08-12):

- **`sync_local_bookmarks_from_remote` (`src/repo.rs:125`) is replaced wholesale, not extended** — the convergence pass is the general operation and this function is its special-cased shadow. The `scope_diverged` error goes with it: divergence stops being an error. Item 1 deliberately kept this function free of merge machinery so it deletes cleanly.
- **Push stops being a single attempt.** The retry loop (fetch, converge, push again) lands here, and rejection-kind classification finally gets a consumer: non-fast-forward → retry within the run; refused-writes → report and stop. The current "hard `GitPushError` is fatal, rejection is not" split is re-derived from the design.

### 3. Conflicts as commits

**Read this first (found in the Wave 3 review, 2026-08-12): resolving a conflict on a scope outside this machine's ancestry is broken end to end, and no test covers it.** Drive it by hand before designing anything here. What happens today: machine B pauses cascading into a scope it does not descend from, resolves the conflicted file in home, and runs `dotsync continue`. The continue *succeeds* — it writes the merge, finishes the cascade and pushes — and then exits 1 with `drift_detected`, because the resolved file in home is a change against B's own machine scope, which does not include the scope that was resolved. The headline says the run failed; the outcome is that it worked and published. Worse, the machine stays that way: `status` reports the file as changed for ever, because there is no scope this machine syncs from that holds the resolution. DESIGN specifies a mode switch for exactly this — the pause tells the agent it is resolving another machine's branch, and `continue` restores home afterwards — and none of it is implemented.

**Standing rule for any work on conflicts: exercise both an in-ancestry pause and an out-of-ancestry pause.** Every conflict test in the suite today pauses on a scope the machine descends from, which is why this survived three waves. The two are different flows with different endings, and the second one is the one that leaves a machine wedged.

Write conflicted merges as real commits; derive "paused" from conflicted heads; delete `.dotsync-paused-cascade.json`; never push conflicted heads. Materialize conflict markers into home via sync (jj's own materialization code, scope-labeled sides, base included); drift treats materialized markers as expected content. `continue` verifies markers gone, resolves at the rootmost conflicted scope, propagates via descendant rewriting. `abort` returns to the last fully cascaded machine scope tip, abandoning only unpushed conflicted commits, and reverts **all** the config files — a full sync of home, not a selective restore. Add `dotsync show conflict` rendering derived state.

Deletions and consequences (expanded 2026-08-12):

- **The pause file's whole ecosystem deletes together**: save/load/remove (`src/commit.rs:965–998`), the three pause-site writes (`:243`, `:320`, `:874`), `reject_commit_if_cascade_paused`, abort's backward bookmark restore, **and the `WithheldPausedCascade` publish guard added in item 1**. "Never push conflicted heads" replaces the guard structurally — put that eligibility check *inside* `push_scope_updates` so every call site is covered by construction (item 1's reviewer identified the current guard-in-one-caller shape as invariant-by-convention).
- **The exit-code-3 JSON pause contract changes shape** (breaking, and that's fine — frontend backwards compatibility is an antipattern here). `scopes_pending` stops existing because the cascade always completes; the payload becomes a description of conflicted heads derived from the repo.

### 4. Read-only robustness

`status`, `view`, `diff` work on any repo state — including conflicted heads and offline — and report weirdness instead of failing. Read-only commands report what convergence would do (in-memory merges), never mutate.

**Read-only commands still hard-fail on a diverged scope (found in the Wave 3 review, 2026-08-12).** `status`, `diff` and `view` exit 1 with `scope_diverged` when this machine and the remote both hold commits on a scope the other does not. The error itself is honest and was the point of item 1 — but refusing to *run* is exactly what design principle 4 forbids: read-only commands describe unusual state, they do not decline to describe anything because of it. An agent that hits this has no command left that will tell it what is going on. The fix is this item's in-memory convergence reporting: work out what convergence would do, say that the scope has diverged, and still answer the question that was asked. Until then, divergence takes away the diagnostics at the moment they are most needed. **Max's 2026-08-13 decision that `status` always exits 0 settles this**: in-memory reporting is required here, not one option among several, because there is no exit code left for `status` to stop with.

`status` must also report unpushed scopes. After item 1, a refused push is reported by the run that hit it and nowhere else: once that run's output scrolls away, a machine holding unpublished commits looks completely clean, and the honest-failure work of item 1 becomes invisible a minute later. Deferred here deliberately (found during the item 1 review) because it belongs with the rest of the read-only reporting surface.

Possibly the same family, noticed but not chased (Max, 2026-08-13): a foreign git client pushing to a scope without running a cascade leaves that change sitting on the scope where no machine ever receives it, and nothing reports it. It may belong with the unpushed-scope reporting above — both are "history exists somewhere that no machine will act on, and no command says so" — but that has not been checked.

### 5. Agent validation loop

Use the headless agent-scenario infrastructure (`tests/agent-scenarios/`) with a cheap model to validate the full UX: make a config change, run dotsync, resolve a conflict from markers, done. Add scenarios starting from an interrupted-push state and a conflicted-head state — the agent must recover using dotsync alone. Iterate on messages/UX until cheap agents reliably succeed. This is the actual product bar: the tool has failed in practice precisely when real agents met unplanned states.

### 6. Backlog triage

- PR [#15](https://github.com/maxeonyx/dotsync/pull/15) (scope lifecycle / add-scope): review against current main, land or close.
- Issues [#5](https://github.com/maxeonyx/dotsync/issues/5), [#8](https://github.com/maxeonyx/dotsync/issues/8), [#10](https://github.com/maxeonyx/dotsync/issues/10), [#11](https://github.com/maxeonyx/dotsync/issues/11): partially or fully addressed by PRs #14/#16 (`view`, `diff`, init/status UX) — verify and close or trim.
- Issues [#4](https://github.com/maxeonyx/dotsync/issues/4), [#18](https://github.com/maxeonyx/dotsync/issues/18): re-triage after steps 1–4; several items fall out of them naturally.
- **`status` and `diff` stay two commands, and the difference they have today is the right one (Max, 2026-08-13).** His words: *"I think those differences seem legit. dotsync status should be concise (doesn't print a diff) always exits 0. It's also the command I'll use myself. dotsync diff is for more detailed working usage."* So `status` is the concise one an agent (and Max) runs by reflex, prints no diff, and always exits 0; `diff` is for detailed working usage and keeps its exit 1 when it finds changes. **This decides the read-only divergence item under item 4 rather than leaving it optional**: `status` exiting 1 with `scope_diverged` on a diverged scope contradicts "always exits 0", so in-memory convergence reporting is *required* — a diverged scope has to be described in the payload and still answered with exit 0, not turned into a stop. The one exception is not a state report: `dotsync status --force` still exits 2, because that is a usage error about the flag, not something `status` found in the world.
- **Committing to a non-ancestor scope: no (Max, 2026-08-13).** His words: *"Yes that's a very good question I had never considered before! It kind of doesn't make sense - because the working copy is supposed to only move forward, and the config is supposed to stay valid, so I think let's say 'no'?"* It is currently silently accepted; the old tombstoned test `retired_non_ancestor_scope_human_error_stands_alone` suggests it was once meant to error. The replacement pattern, his words: *"a common pattern might be to write to contribute to .config/AGENTS.md in the shared ancestor (tbh I think that one has to live only in the shared ancestor) and document the pattern that the agent on the other machine / machine family should follow when adding its config contributions on it's own scope. eg. for some app X, adding config drop-ins for the home scope based on the pattern established on the shared "linux" scope by an agent acting on the work and work-linux scopes."* **This does not remove out-of-ancestry pauses.** A cascade from a shared ancestor still merges into descendants this machine is not on, so a conflict can still land on a scope this machine does not descend from — K2 under item 3 stays real, and so does the standing rule to exercise both an in-ancestry and an out-of-ancestry pause. What goes away is only the machine *choosing* such a scope as a commit target. D6 (the Wave 1 note above: `commit_merge_base_tree` falling back to the target head for a non-ancestor scope) is answered by the same decision — that case stops being representable.

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

Every command emits one JSON object on stdout when `--output json` is passed. Human-readable messages and notes go to stderr regardless, so a caller can capture the payload and still show the run's own words. Every payload below is copied verbatim from a real run in a two-machine sandbox, which is why the keys are in serde's alphabetical order rather than a readable one.

The envelope is two fields: `status` is `"ok"` or `"error"`, and `command` names the command that answered. Read `status` first: it is what separates `dotsync diff`'s exit 1 (changes found, `"ok"`) from a stop (`"error"`). Any command that could not reach the remote also carries `remote_unreachable` with git's own words, meaning the payload describes the last state this machine fetched.

### `dotsync` (sync), `init`, `continue`

```json
{"command":"sync","machine_scope":"a","overwritten_files":[],"status":"ok","synced_files":[".config/dotsync/config.toml"],"unpushed_scopes":[]}
```

One machine scope under one name: `scope` used to repeat `machine_scope` here and was deleted. `unpushed_scopes` lists scopes committed on this machine but not on the remote — the remote refused them, publishing was withheld while a cascade is paused, or the remote was out of reach. Empty means the remote has every scope commit this machine holds, including anything earlier runs left behind, which every publishing command republishes even when it has nothing of its own to add. `overwritten_files` lists home files whose contents this run discarded in favour of the repo — everything `--force` overwrote, and everything `init` and `abort` overwrote without being asked. It is the opposite direction from `commit`'s `forced_overwrites`, which is what a commit recorded *over* another machine's change, and both exist because a run that destroys something has to say so in the payload as well as in the notes.

### `abort`

```json
{"command":"abort","machine_scope":"b","overwritten_files":[".config/app.conf"],"paused_scope":"linux","status":"ok","synced_files":[".apprc",".config/app.conf",".config/dotsync/config.toml"]}
```

`abort` publishes nothing, so it has no `unpushed_scopes`. `paused_scope` is where the cascade had stopped — not the scope the discarded commit was on, and not something that was itself aborted.

### `commit`

```json
{"command":"commit","forced_overwrites":[],"machine_scope":"a","newly_tracked":[".apprc"],"outcome":"committed","scope":"all","skipped_paths":[],"status":"ok","synced_files":[".apprc",".config/dotsync/config.toml"],"unpushed_scopes":[]}
```

```json
{"command":"commit","machine_scope":"a","outcome":"nothing_to_commit","scope":"all","skipped_paths":[],"status":"ok","unpushed_scopes":[]}
```

`outcome` distinguishes the two, and they are genuinely different events: a commit with nothing to record writes no history, runs no cascade and runs no home sync, so `synced_files`, `newly_tracked` and `forced_overwrites` are absent rather than empty. `skipped_paths` holds what a named directory matched and the commit left alone, each as `{path, state, reason}` — `state` is a file state such as `stale_not_yours`, or `symlink` / `not_a_regular_file` for a path dotsync cannot record whatever its content says.

### `status` and `diff`

```json
{"changes":[{"path":".apprc","reason":"edited here since the last sync","state":"modified"}],"command":"status","incoming":[],"machine_scope":"a","status":"ok"}
```

```json
{"changes":[{"diff":"--- repo\n+++ system\n@@ -1 +1 @@\n-ui = dark\n+ui = light","path":".apprc","reason":"edited here since the last sync","state":"modified"}],"command":"diff","machine_scope":"a","status":"ok"}
```

The same population, the same objects, the same names: `diff` is `status`'s `changes` with the diff attached. `status` adds `incoming`, the files another machine changed that home has not caught up to. Neither carries a count — the arrays have lengths.

Both also carry `paused_cascade` when a cascade is paused, naming the scope it stopped at:

```json
{"changes":[{"path":".config/app.conf","reason":"edited here since the last sync","state":"modified"}],"command":"status","incoming":[],"machine_scope":"b","paused_cascade":"linux","status":"ok"}
```

Present only when there is one, like `remote_unreachable`. That machine can commit nothing and is publishing nothing, and the read-only commands are where an agent goes to find out why.

### `view`

Four shapes under one command name, one per question asked: `{scope, files}`, `{file, scopes}`, `{scope, path, contents}`, and the overview's `{scopes, files}`. This is a known sharp edge and is **not** fixed: the shapes are coherent with each other only in the envelope, `scopes` changes type between two of them, and file contents are UTF-8 lossy. Whoever takes it should decide whether these are one command at all.

### Errors (exit code 1, or 3 for a paused cascade)

```json
{"current_state":["`/etc/passwd` is an absolute path, and dotsync resolves every commit path against your home directory.","`typo.conf` matched nothing: no file exists at or under /home/you/typo.conf, and scope `all` tracks no file at or under `typo.conf`."],"drifts":[],"error":"unusable_commit_paths","forced_overwrites":[],"message":"cannot commit 2 of the paths you named","status":"error"}
```

`current_state` is a list of facts, one per thing the run found, so a caller never has to split a rendering apart on newlines; the human rendering joins them. `drifts` carries the same change objects `status` and `diff` report, with the diff, and is populated for `drift_detected`. `forced_overwrites` is what the run had already recorded over an incoming change before it stopped. All three are always present, so error handling has one shape.

Error codes in use: `not_initialized`, `repo_exists`, `invalid_scope`, `file_not_on_scope`, `scope_not_in_repo`, `scope_diverged`, `no_current_scope`, `missing_parent`, `scope_cycle`, `config_parse`, `config_edit`, `sync_state`, `drift_detected`, `unusable_commit_paths`, `stale_commit_paths`, `not_a_regular_file`, `cascade_paused`, `paused_cascade_in_progress`, `unresolved_conflict`, `pause_predates_resolution_check`, `no_paused_cascade`, `missing_hostname`, `remote_unreachable`, `home_not_set`, `non_utf8_path`, `git_submodule`, `io`, `internal`. Plus `usage` on exit 2.

### Usage errors (exit code 2)

```json
{"current_state":[],"drifts":[],"error":"usage","forced_overwrites":[],"message":"unknown command `bogus`; run `dotsync --help` for supported commands","status":"error"}
```

Emitted for clap's own parse failures too, which is why `--output` is read straight from argv both before clap runs and when clap's unknown-command arm has swallowed it.

### The conflict pause payload

Today a pause is an ordinary error payload with `error: "cascade_paused"` and the conflicted files in the message:

```json
{"current_state":["paused scope: linux"],"drifts":[],"error":"cascade_paused","forced_overwrites":[],"message":"cascade paused at scope `linux` with conflicts in .config/app.conf","status":"error"}
```

The richer contract this section used to document — `scopes_done`, `scopes_pending`, `original_scope` — **does not exist and is not being built here**: item 3 (conflicts as commits) derives the pause from conflicted heads, at which point `scopes_pending` stops being a coherent idea because the cascade always completes. Item 3 builds that contract.

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
