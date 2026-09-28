# dotsync — Plan

`DESIGN.md` describes dotsync as it is. This file is for what is not built: what is ahead, what is deliberately deferred, and the standing constraints any of it is done under.

## Where things stand

- The fleet is readable and rearrangeable from any machine (2026-09-28). Every (scope, path) has a standing — inherited, added, overridden, removed — and `scopes`, `files`, `show` and `diff <scope> [<scope>]` read it for any scope, before joining too. Every write is pins laid down by the convergence pass; `move` and `drop` place repo content on any scope, and every write reports and can preview (`--dry-run`) its effect per machine. `init` keeps home content and keeps its clone when it cannot join. `view` is gone.
- The next-gen rewrite is done. Home is jj's working copy, the scope graph is derived from the repo's own structure, one convergence pass moves every scope bookmark, "paused" is recomputed rather than stored, and each error carries its own explanation. Two files exist per machine: the home files, and the hidden repo — plus one record of a run that stopped, which is a fact about that run rather than about the repo.
- Development happens in the agent-tools workspace (`tools/dotsync`). For the current version, run `dotsync --version` or check the releases; don't record it here, it rots.
- Live fleet: remote `git@github.com:maxeonyx/dotfiles.git`, eleven scopes, `all` → `home`/`work`/`linux`/`windows` → intersections → machines (`mc-wsl-fd`, `mx-xps-cy`, `maxeonyx-pc-windows`).
- **mc-wsl-fd is the only machine upgraded.** `mx-xps-cy` and `maxeonyx-pc-windows` are on ≤0.4.6 and every command on them exits 1 with `config missing on all scope` until their binary is upgraded, because `config.toml` was deleted from the `all` scope on 2026-09-08. That is Max's own decision rather than an open question: upgrade the binary, then run `dotsync`.
- One test is red on purpose: `read_only_commands_leave_the_scope_bookmarks_where_they_found_them`. What moves the bookmark is jj's import of what the fetch brought, which is the fast-forward case of convergence — the case that creates nothing. Meeting it needs a run that answers from a fetched view it never records, which is a change to how a run holds the repo. Whoever wants it should take it as its own piece.

## Standing constraints

- **Prioritise conceptual simplification and the removal of bad modelling.** When a change supersedes a mechanism, delete the old mechanism in the same change rather than layering. Anything robustly wrong or unnecessary is in scope to remove.
- **The cull applies to the product surface, not just internals** (Max, 2026-08-12): _"not just gurky internals, but sharp product edges too. Remember - me, and mainly agents with no memory, are the only users. So self-documentation is critical and backwards compatibility (on the frontend, not the backend though (ie. the repo / scope model)) is an antipattern on this project."_ Redesign a sharp flag, message or JSON field outright. The repo/scope model is where compatibility matters.
- **Back-chain smells to root causes.** Don't fix a smell where it presents; trace it to the modelling decision that produced it and fix that, accepting large refactorings.
- **Never hand-mutate the hidden repo or the dotfiles history.** If a change would need to rewrite the live dotfiles history, find another way or stop and tell Max.
- **Make invalid states unrepresentable. Make bugs impossible by construction. Relentlessly identify and destroy incidental complexity** — while keeping enough essential complexity (Max).

## Ahead

### Agent validation loop

Use the headless agent-scenario infrastructure (`tests/agent-scenarios/`) with a cheap model to validate the full UX: make a config change, run dotsync, resolve a conflict from what the stop printed, done. Include the fleet episodes: audit every machine for config worth sharing, promote from the second machine, narrow config pushed too broad, join a new machine choosing its parent from what it can read. Add scenarios starting from an interrupted-push state and a conflicted-head state — the agent must recover using dotsync alone.

This is the actual product bar: the tool has failed in practice precisely when real agents met unplanned states. It is also what settles the one open presentation question. Conflicts are presented in dotsync's own output and never written into home, because markers in a live config file are broken config for as long as the pause lasts (Max, 2026-08-19, "the overwhelming preference") — but "a real agent can reliably resolve one from what it is shown" is the bar that presentation has to clear, and only watching one do it says whether it does. If it does not, the fallback is materialising markers through ordinary sync, and `continue` would delete with it, because "the markers are gone" is then legible from the file.

### Graph changes

Creation and deletion are still the whole graph surface, so an existing machine cannot be moved onto a new scope, a scope cannot be renamed, and a scope something hangs off cannot be deleted. Max (2026-09-28) asked for the episodes to be worked through before anything is designed. They are below, then what they require, then a design that meets them. None of it is built.

**Episodes** (actor is an agent on the machine named unless said):

| # | Goal | Today | What it needs |
| --- | --- | --- | --- |
| G1 | New machine joins under an existing scope | `init`, reading the fleet first | done |
| G2 | New machine needs a scope that does not exist yet (`hyperv` under `home-linux`) | `create-scope`, then `init --parent` | done |
| G3 | `mx-manjaro` moves under a new `hyperv` scope it will share with a second VM | impossible | the machine's leaf gets new parents; its own layer (what it adds, overrides, removes) survives; its effective config changes only by what `hyperv` holds |
| G4 | A machine is named after its hostname and should not be (`mx-manjaro` → `mx-hv-mj`) | `DOTSYNC_HOSTNAME` set in every shell, for ever | a machine name chosen at `init` and remembered; renaming a leaf |
| G5 | A shared scope was created in the wrong place (`hyprland` under `all` instead of `linux`) | impossible | reparent an inner scope; every scope below keeps its own layer |
| G6 | An intermediate scope is no longer useful (`home-windows` has one machine) | refused: it has children | delete it, with a stated choice for its own layer: pushed into the scopes below (no machine changes) or gone (the machines that lose it listed) |
| G7 | Two scopes turn out to mean the same thing (`home` and `home-linux` on a fleet with no home-windows left) | impossible | G6 with the absorbing choice |
| G8 | A scope is renamed (`work` → `nzx`) | impossible | G5 with a new name; machines named by it follow |
| G9 | Any of the above, seen from another machine | — | the next fetch converges onto it like any other head that moved; a machine holding unpublished work on a rebuilt scope does not resurrect the old shape |
| G10 | Any of the above, before doing it | — | the per-machine effect, which for a pure rearrangement is usually "no machine's config changes" — and that emptiness is the check |

**What they require:**

1. **A machine's name is a stored fact.** The working-copy record already holds it (the workspace is named for the machine scope), but every run re-derives it from the hostname, so a hostname change orphans the machine and G3/G4 cannot re-point it. `init --name`, and reading the name back from the working-copy record.
2. **Reparenting is rebuilding.** An edge is a creation commit's ancestry and commits do not change, so a scope gets new parents by getting a new creation commit on them, with its own layer replayed on top (a three-way merge: base what it inherited, ours what it held, theirs what it will inherit). Every scope below it is rebuilt the same way, in cascade order, so each child's new creation commit descends from its parent's: that is what keeps the derived graph a function of current history. The old creation commits end up reachable from no head, so they stop meaning anything. For a leaf (G3, G4) that is one scope.
3. **A head contested between two creations converges to the newer.** Another machine that cascades into a rebuilt scope before it has seen the rebuild leaves the old lineage on one side of a contested head — the same shape as a head with a deletion on one side, and settled the same way: the rebuild is somebody's decision, and the other side is a cascade merge nobody asked for. Only the scope's own machine commits to a leaf from home, so for leaves the losing side carries nothing of anybody's.
4. **Deleting a scope with children is rebuilding its children onto its parents**, plus the choice of where its own layer goes (G6/G7) — absorbing into the children is the default because it changes no machine.
5. **Every graph change is planned and previewed like any write**: pins and creation commits in one transaction, the per-machine effect before and after, `--dry-run`.

**Proposed surface:** `dotsync reparent <scope> --parent <scope>...` (G3, G5), `dotsync rename <scope> <name>` (G4, G8; on a machine scope, run from that machine), `delete-scope` accepting a scope with children given `--absorb` or `--drop` (G6, G7), and `init --name`. All of it reports the effect, and a rearrangement whose effect is not empty says so before anything is written.

Open: whether a rename of another machine's leaf should be possible at all (that machine would find its name gone and need `init --name` to follow — requirement 1 makes that recoverable, not automatic), and whether "newer" in requirement 3 is decided by commit timestamp or by something that cannot be skewed.

### Windows

- **What a scope may hold is constrained by what its machines can represent** ([#28](https://github.com/maxeonyx/dotsync/issues/28)). Symlinks, executable bits, non-UTF-8 text: the constraint is the union over every leaf a scope reaches, which makes `all` the _most_ constrained scope rather than the least. The issue holds the model and the one open question — whether the constraint comes from machine leaves that exist, or from the OS scopes in the graph.
- **Path separators are unconfirmed on Windows.** The directory walk builds relative paths with `read_dir` separators and feeds `from_internal_string`, which would record `.config\fish\config.fish` as a tree entry name; `render::display_path` is `Path::display()`, so JSON would carry backslashes in some fields and forward slashes in others; and `canonicalize` returns the on-disk casing, so on a case-insensitive filesystem `dotsync commit all -- .APPRC` may look like a link to somewhere else and be refused. All inferred from source. Needs one Windows run.

### Smaller, unowned

- **Untracked config is found by hand.** `status` lists changes to managed files, so Max's procedure has agents compare `~/.config` and `~/.local/bin` against `dotsync files --scope <this machine>` themselves. A read that lists the unmanaged files under named directories, and says which of them another scope already holds, would make that step a command.
- **A path's recent history is not readable.** "Deleted on purpose because nothing uses it" lives in a commit message on another scope; an agent pulling config in from other machines can revive something another machine retired (7e6e1f3 against 1ac9129 in the live fleet). Messages of the commits that last changed a path, per scope, would carry it.
- **`show` of non-UTF-8 content in JSON is lossy.** `utf8: false` says so; nothing in the fleet needs more yet.
- **A descendant that removed a file conflicts on every later edit of it above.** A removal is a tombstone: each change to the file on a parent is a modify/delete merge on the scope that removed it. `files --own` makes these visible (`D`); nothing yet makes them cheaper.
- **A conflict's side labels name the scope whose head each input is, not the scope the change was made on.** A change committed to `all` is presented as `` side: scope `linux` `` when `linux`'s head is the commit that came down from `all`. Correct and confusing at once.
- **Convergence merge commit ids are not reproducible, and could be.** `machine_signature` stamps `Timestamp::now()`, so predicting the pass twice writes two objects for the same merge. Deriving a convergence merge's timestamp from its inputs would make it byte-identical on every machine that computes it — which would also mean two machines converging the same scopes independently never diverge.
- **Auto-update** — Max, 2026-08-19: he wouldn't mind implementing it at some point. Workspace-level, since every tool would want it.

### Backlog triage

- PR [#15](https://github.com/maxeonyx/dotsync/pull/15) (scope lifecycle / add-scope): predates the rewrite. Review against current `main`, land or close.
- Issues [#4](https://github.com/maxeonyx/dotsync/issues/4), [#5](https://github.com/maxeonyx/dotsync/issues/5), [#8](https://github.com/maxeonyx/dotsync/issues/8), [#10](https://github.com/maxeonyx/dotsync/issues/10), [#11](https://github.com/maxeonyx/dotsync/issues/11), [#18](https://github.com/maxeonyx/dotsync/issues/18): all written before the rewrite and several are fixed by it. Re-triage against what ships today, then close or trim.
