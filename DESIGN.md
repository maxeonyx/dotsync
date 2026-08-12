# dotsync — Design Story

**This document describes the target design, not the shipped state.** Things described here in the present tense may not be built yet — conflicts-as-commits and the convergence pass are the current examples. `PLAN.md` tracks what exists, what is in progress, and in what order the rest lands; when the two disagree about what dotsync does today, `PLAN.md` is right.

## The problem

You have config files scattered across `~/` on multiple machines. Some config is universal (`.gitconfig`), some is OS-specific (hyprland on linux), some is machine-specific (wallpaper paths). You want a single repo that is the source of truth for all of it, and you want AI agents — your primary method of editing config — to be able to maintain it naturally.

Dotfile management breaks down along several axes:

1. **Syncing** — copying files between a repo and the live system is tedious and error-prone. People forget, things drift, and nobody notices until something breaks on a fresh machine.

2. **Multiple versions** — the same *kind* of config (e.g. "shell config") might differ between linux and windows, or between your laptop and your server. Most dotfile tools either ignore this (one branch, one machine) or punt to symlink farms with conditionals baked into the files themselves.

3. **Contributing changes** — when an agent (or human) edits a config file, getting that change into the right place in the repo should be frictionless. If it's not, agents make mistakes and humans stop doing it, and the repo rots.

4. **Agent-first** — AI agents are the primary editors of config. The system must be simple enough that a skill description can fully explain the workflow — agents can't handle complex mental models or ambiguous "which file do I edit?" situations.

## Why not existing tools?

- **Bare git repo with `$HOME` as worktree**: elegant but terrifying. Every `git clean` or careless `git checkout` can nuke your home directory. `git status` shows thousands of untracked files. Agents would need to be told "never run git clean" — a footgun.

- **Symlink managers (stow, etc.)**: stow creates symlinks from a package directory into `~/`. Multi-machine is DIY — you pick which "packages" to install per machine, but there's no built-in scoping. Agents don't know which package a file belongs to without being told.

- **Chezmoi**: the closest existing tool to what we want. It handles multi-machine via Go templates embedded in config files (`{{ if eq .chezmoi.os "linux" }}`), has a proper `diff`/`apply` workflow, and supports secrets. It would work. But templates mean the file in the repo is *not* the file on the system — agents editing config need to understand both the config syntax and the template syntax. Our approach puts plain files in the repo, and uses git's merge machinery for scoping instead of templates. This means an agent edits exactly the file that ends up on the system, with no indirection.

- **Naive branching**: some people use git branches for per-machine config. This works until you need to propagate a universal change to all machines — you're manually cherry-picking or rebasing across N branches, re-solving the same conflicts. dotsync automates the propagation and uses merge commits to preserve conflict resolutions.

## The scope DAG

The core insight: machines aren't the only unit of variation. There are *scopes* — overlapping categories that a machine belongs to. A linux laptop with hyprland belongs to scopes "all", "linux", and "hyprland". A windows desktop belongs to "all" and "windows".

These scopes form a directed acyclic graph (DAG):

```
        all
       /   \
    linux   windows
      |
   hyprland
      |
   mx-xps-cy
```

Each scope is a branch. A scope branch merges from its parent(s). So `linux` merges from `all`, `hyprland` merges from `linux`, and `mx-xps-cy` (a machine) merges from `hyprland`.

A machine is just a leaf scope — there's nothing structurally special about it. The only difference is that a machine scope is the one whose files get synced to the live system. dotsync knows which scope is current from machine-local sync state and the configured scope graph, not from a user-visible checkout.

### Why not a single branch with directory-based scoping?

We considered organizing files by scope in directories:

```
common/.gitconfig
linux/.config/hypr/hyprland.conf
mx-xps-cy/.config/hypr/hyprpaper.conf
```

This avoids branching complexity entirely, but it breaks down when the same file needs per-scope tweaks. If `hyprland.conf` is 95% the same on two machines but needs 3 lines different, you'd have to duplicate the entire file into each machine's directory. With branches, git's merge machinery handles this naturally — the common parts live on a shared ancestor, and per-machine tweaks are commits on the machine branch.

Directory-based scoping also means the repo doesn't mirror `~/`, which breaks the simplicity of "repo path = path under home dir."

### Why merges, not rebases?

We considered rebase-based propagation: when `all` gets a new commit, rebase `linux` onto it, then rebase `hyprland` onto `linux`, etc. This gives linear history but has a fatal flaw: **conflict resolutions are lost on every rebase.** If `hyprland.conf` has a merge conflict between `linux` and `mx-xps-cy`, you'd re-solve it every time any ancestor changes.

Git has `rerere` (reuse recorded resolution) which remembers conflict resolutions, but it stores them locally in `.git/rr-cache` — they don't transfer to new clones. A fresh machine setup would have no resolution memory and would immediately hit conflicts that were already solved.

With merge-based propagation, conflict resolutions live in merge commits, which are part of the repo history. Every clone gets them. An agent can read the history to understand what happened. The cost is merge commits in the log, but that's a feature — each merge commit is a record of "this scope incorporated these changes from its parent."

### Why scopes can have multiple parents

Initially we modeled machines as a separate concept: a machine "includes" a set of scopes. But this was an unnecessary distinction — a machine is just a scope that happens to merge from multiple parents. The data model is simpler when everything is a scope: `mx-xps-cy = { parents = ["hyprland"] }` is the same shape as `hyprland = { parents = ["linux"] }`.

Multiple parents also handle edge cases naturally. If a hypothetical machine needs both `hyprland` and `server` scopes that share no lineage beyond `all`, the machine scope just lists both as parents.

## Repo structure

The hidden repo mirrors `~/`. A repo path `.config/fish/config.fish` corresponds to `~/.config/fish/config.fish`. No path mapping, no translation layer. This is critical for agent usability — an agent told "edit the fish config" edits the obvious live file in home, then dotsync imports that selected home path into the right scope.

Files are implicitly tracked by existing in the repo. There is no whitelist file. If a file is in the repo on the current branch, it gets synced. If you don't want a file synced, don't put it in the repo. This eliminates an entire class of "forgot to add to the whitelist" bugs.

The only config file is the scope graph:

```toml
# .config/dotsync/config.toml

[scopes]
all = {}
linux = { parents = ["all"] }
hyprland = { parents = ["linux"] }
windows = { parents = ["all"] }
mx-xps-cy = { parents = ["hyprland"] }
mx-pc-win = { parents = ["windows"] }
```

This lives on the `all` branch (since every machine needs the full graph).

## Sync and commit direction

Plain sync is always repo -> system. The repo is the durable source of truth, and `dotsync` with no scope materializes the current machine scope into `~/`.

Commits are home -> repo for selected paths only. Users and agents edit files at their real home locations, inspect `dotsync status`, then run `dotsync commit <scope> -m "message" -- <paths...>` to record the selected home files to the appropriate scope. After committing, dotsync cascades that scope through descendants, syncs the current machine home, and pushes.

If a managed home file differs from the repo outside a commit flow, that's drift. dotsync warns and shows a diff. The user (or agent) decides whether to overwrite the system file or investigate.

### Why not fully bidirectional?

Bidirectional sync requires conflict resolution between the repo and the system, which is a fundamentally different (and harder) problem than git merge conflicts. It also makes the mental model ambiguous: "which is the source of truth?" With unidirectional sync, the answer is always "the repo."

dotsync still *reads* system files — it diffs them against the repo, reports status, and imports selected files during an explicit scoped commit. But it never treats arbitrary home drift as something to sync automatically. A repo update and a local home edit are different events, and the command shape makes the user choose which home paths belong in which scope.

The cost: to contribute a home change, you must name the scope and paths explicitly. That explicitness is the safety boundary that replaces a visible checkout or a broad "sync everything from home" mode.

An open question: some config files may end up with sections that shouldn't be checked in (e.g. secrets injected by an application). We don't have a strategy for this yet. Hopefully it doesn't come up, but if it does we'll need something — possibly `.gitignore` patterns for sections, or splitting the file.

### Drift detection and sync state

dotsync tracks a minimal machine-local sync state file recording which machine scope was last synced and at which commit. This enables two things:

1. **Deletion semantics** — when a file is removed from the repo, dotsync can detect that it was previously synced to home and should be removed. Without state, dotsync couldn't distinguish "this file was never managed" from "this file was managed and was removed."

2. **Drift attribution** — comparing home state against the last-synced revision rather than repo HEAD distinguishes "repo advanced elsewhere" from "home drifted locally." A plain sync can then accept legitimate remote updates without treating them as local drift, while still stopping before overwriting files that changed in home since the last sync.

The three-way comparison (last-synced tree `L`, home `H`, new tip `T`) classifies every file situation without special cases. Presence and equality across the three sides is the whole domain, so every situation lands in exactly one class:

| Class | `L` / `H` / `T` | Behavior |
|---|---|---|
| in sync | all three identical | nothing to do |
| incoming add | absent / absent / present | not drift — sync writes it |
| incoming update ("stale, not yours") | present / equal to `L` / changed | not drift — sync writes it; **`commit` refuses it** |
| incoming delete | present / equal to `L` / absent | not drift — sync removes it from home |
| edit drift | present / changed / equal to `L` | blocks; commit records it |
| edit drift, removed from the repo | present / changed / absent | blocks; commit records it |
| deletion drift | present / absent / equal to `L` | blocks; commit records the deletion |
| deletion drift, tip also changed | present / absent / changed | blocks; commit records the deletion |
| diverged edit | present / changed / changed differently | blocks sync; commit merges the two, pausing on conflict |
| already applied | present / changed / changed to the same bytes | not drift — this run's own commit, or a crashed run's writes |
| untracked collision | absent / present / present, differing | blocks — home holds content dotsync has never seen |
| untracked | absent / present / absent | not managed; only `commit` cares |
| converged deletion | present / absent / absent | nothing to do |

The row that carries the most weight is **incoming update**: home holds exactly what was last synced, and the tip has moved on. A two-sided comparison of home against the tip cannot tell it apart from a local edit, so `status` reports it as a change and a `commit` naming that path re-records the older bytes and cascades them — silently reverting whoever published the change. Naming the class is what makes that unrepresentable: `status` files it under incoming rather than changed, and `commit` refuses it, pointing at plain `dotsync`.

Deleting a managed file from home is drift like any other: it shows in `status` and `diff`, blocks sync, and is recorded to a scope with `dotsync commit <scope> -- <path>`. Files added on other machines flow in frictionlessly because they were never in this machine's last-synced tree.

When there is no usable sync state — a fresh machine, a deleted state file, or one naming a revision this repo does not have — the last-synced side is empty rather than assumed. Dotsync then removes nothing from home and reads no missing file as a deletion, because it has no record of putting anything there; what it can still judge from home and the tip alone, it still judges.

**`--force` has two shapes, because the commands asking it do not all have something to scope the answer to.** Plain `dotsync` and `continue` name no paths, so their `--force` is blanket: overwrite every drifted file. `commit` names paths, and its `--force` rides that same list — it overrides the refusal for exactly those paths, takes home's side for them, and leaves every other drifted file alone. `dotsync commit linux -m msg --force -- .bashrc` overwrites `.bashrc` and nothing else; `dotsync --force` overwrites everything. Paths recorded on that authority are reported as `forced_overwrites`. `init` and `abort` refuse the flag: neither ever makes the choice, because `init` has nothing of yours to overwrite and `abort` exists to discard home edits.

The sync state file path is configured in `config.toml` under `[sync] state_path` and lives in the home directory (not the repo). It is never synced as a managed dotfile.

An earlier design rejected state tracking as unnecessary complexity. That was wrong — deletion semantics require it. The cost is one small JSON file per machine; the benefit is correct file removal and a path toward smarter drift handling.

## The jj decision

dotsync uses [jj (Jujutsu)](https://github.com/jj-vcs/jj) rather than raw git. The key reason: **jj can manipulate branches without touching the working copy.**

When you contribute a change, it needs to be committed on the right scope branch — not necessarily this machine's leaf scope. With git, this requires a checkout or worktree for the target branch, plus careful staging around unrelated home edits. If you have multiple edited files going to different scopes, this becomes a nightmare of stash juggling or visible workspaces.

With jj, dotsync creates a commit directly on the target scope's branch and merges descendant branches in the hidden repo, all without exposing a checkout to the user. Home remains the editing surface; the hidden repo remains implementation detail.

jj is also git-compatible — the repo is a valid git repo, pushable to GitHub, cloneable with git. jj is just a better local interface for the graph manipulation dotsync needs.

One risk: jj is newer and less well-known than git. AI agents may not have strong intuitions for jj commands and concepts. dotsync therefore abstracts jj away entirely: agents only interact with `dotsync` commands and never run `jj` directly.

**Requirement: dotsync must never depend on the jj CLI binary at runtime.** jj is linked in as a library (jj-lib); user machines do not have and must not need jj installed. The library link is functional, not just packaging: dotsync needs operations the CLI doesn't expose, like computing a merge in memory to report would-be conflicts without creating commits or moving bookmarks. Known caveat: jj-lib's supported fetch/push mechanism shells out to a `git` subprocess, so a `git` binary on PATH is currently a runtime dependency for network operations. That's acceptable for now and recorded here so nobody assumes full self-containment.

## The convergence model

Scope branches are normal repo history, and multiple machines write to them concurrently. That makes bookmark divergence a **routine event, not an edge case**. Walk through the common case: machine A commits to `all`, and the cascade creates a merge commit on every descendant scope — the entire DAG — then pushes. Machine B, which hasn't fetched yet, commits to `linux`; its cascade moves `linux` and everything below it. When B next talks to the remote, half a dozen scope bookmarks have diverged — local and remote each have commits the other lacks — from two innocent, non-overlapping edits. Any design that treats divergence as an error fails on the second machine, every time.

So dotsync's core operation is a **convergence pass**: for each scope in topological order, the new head is the merge of {local head, remote head, updated parent-scope heads}, skipping commits where nothing changed. This one operation subsumes what would otherwise be three separate mechanisms:

- local behind remote → the merge is trivially the remote head (fast-forward)
- local ahead of remote → the merge is trivially the local head (unpushed work; push it when pushing)
- diverged → a real merge commit, pausing on file conflicts exactly like any cascade merge
- parent scope advanced → the ordinary cascade merge

Every state a machine can be in — mid-crash, post-failed-push, freshly offline-edited — is just an input to the next convergence pass. There is no separate "recovery."

**Pull first, always.** Every mutating command opens with fetch + convergence, so remote changes are integrated *before* new work builds on top of them — never discovered mid-flow after edits and merges are already in progress. Commit is then: converge, add the new commit, converge again (to cascade it), push.

**Push is a loop, not a step.** A rejected push isn't an error; it means another machine pushed first. Fetch, converge, push again. Push happens immediately after history is created — before the home sync — so a sync-side stop (like drift) never strands committed history unpushed.

**Read-only commands never mutate.** `status`, `diff`, and `view` don't move bookmarks, create commits, or touch home. They fetch (when online) and *report* what convergence would do — including "pulling would conflict on these files in scope X" — computed as in-memory merges via jj-lib. Only `dotsync` (sync), `commit`, and `continue` actually converge.

**Offline is just deferred convergence.** If fetch fails due to network, dotsync skips it and proceeds against last-known remote state. Local history builds up ahead of the remote — which is a normal convergence input, handled the next time the machine is online. There is no offline mode and no queue.

## Conflict resolution in home

There is deliberately no visible working copy — a working copy next to the live config would mean three copies of everything. But the live config directory **is the working copy for all intents and purposes**, and it gets the full working-copy treatment. The user can never move it backward or sideways to another version or scope (inspection is done via `dotsync view`); it only ever goes forward. And when a merge conflicts, the conflict is materialized where the user works.

### Conflicts are commits, not a paused mode

jj's defining feature is that conflicts are first-class objects inside commits: a merge commit can be created with a conflicted tree, and descendants inherit the conflict through their own merges until it is resolved. dotsync leans on this fully. **The cascade never pauses structurally** — every convergence pass completes in one atomic transaction, writing every merge commit, conflicted or not. "Paused" is not a stored mode; it is a derived observation: *one or more local scope heads have conflicted trees.* The conflicted heads act as the queue of pending resolution work.

An earlier design stored pause intent in a machine-local state file (merge parent ids, remaining cascade steps, pre-pause heads) and refused to create conflicted history. That was a holdover from the working-copy era and created a class of dead ends: the file was written outside the repo transaction (crash = half-cascaded bookmarks with no record), it was invisible (no command displayed it), and it was the only copy of the intent. With conflicts in history, every piece of that state is derivable: the conflicted scope and files from the head trees, the merge parents and description from the conflicted commit itself, and nothing else is needed because there are no "remaining steps" — the cascade already completed around the conflict.

**Principle: keep exactly the minimum required state.** The only machine-local state is the sync state file — machine scope and last-synced revision — because those are per-machine facts that shared history cannot contain. Anything derivable from the repo must be derived, never cached in a side file. Derived state is automatically correct after a crash; stored state is a fresh opportunity to be wrong.

### The resolution surface

- **Conflicts materialize into home via ordinary sync.** A conflict anywhere in this machine's scope ancestry propagates down into the machine scope's tree, so syncing writes standard conflict markers (`<<<<<<<` / `|||||||` / `=======` / `>>>>>>>`, base included, sides labeled with scope names rather than commit ids) into the affected home files — using jj's own conflict-materialization code. Agents have deep priors on this exact format. While markers are materialized, drift detection treats them as the expected home content.
- **`dotsync show conflict` renders the conflict state at any time**: DAG position, the rootmost conflicted scope, which scopes' changes are colliding, the conflicted files, and the instructions. Because it renders derived state rather than a stored record, it is automatically correct after any crash, on any machine. `status` points here whenever conflicts exist.
- **`continue` verifies the markers are gone**, applies the resolution at the rootmost conflicted scope, and propagates it: descendant merges that inherited the same conflict resolve automatically through jj's descendant rewriting. `commit` refuses while any local scope head is conflicted, pointing at the resolution flow.
- **Conflicts outside this machine's ancestry** (e.g. a cascade from `all` conflicting only in the `windows` subtree while this machine is linux) don't appear in home naturally. `show conflict` still reports them, and the affected home path serves as a temporary resolution buffer — with the mode-switch stated loudly: "this file temporarily contains the conflicted merge for scope `windows`; it is not your machine's config; after `continue` or `abort` it will be restored to your machine's version."
- **Conflicted heads are not pushed.** They stay local-ahead — a normal convergence state — until resolved; everything non-conflicted still pushes. This keeps the shared remote free of conflict encodings (which plain git tooling renders poorly) at the cost that only the machine holding the conflict can resolve it.
- **`abort` goes back to the last fully cascaded machine scope tip.** It abandons the unpushed conflicted commits, returns the affected scope bookmarks to their last non-conflicted positions, and reverts **all** the config files — a full sync of home to the machine scope's last fully cascaded tip, not a selective restore. Conflict markers and the home edit that caused the aborted commit are both gone from home afterward; that's the point of abort. Pushed history is never touched — conflicted heads are never pushed, so everything abandoned is local-only. Clean remote integration discarded along the way costs nothing: the next convergence pass re-derives it.

## Failure model: no dead ends

Every state dotsync can produce — including states produced by crashing at the worst possible moment, a failed push, or another machine racing — must be a state that dotsync commands alone can diagnose and recover from. If a run is interrupted anywhere, the remedy is "run dotsync again" (or `continue`/`abort` for a paused cascade). Never repo surgery.

This is mostly a corollary of the convergence model: interrupted work leaves local-ahead or diverged bookmarks, and those are ordinary convergence inputs. The remaining obligations are ordering and atomicity: persist pause intent in the same effective step as the history it describes, push as soon as history exists, and keep read-only commands working on any state (they report weirdness; they don't refuse to run because of it).

## Commands

There is one command: `dotsync`.

**`dotsync`** (no arguments): Pull and converge scope branches (merging remote changes and cascading, pausing on conflicts), sync repo -> system, push. It does not import home edits; use `dotsync status` and `dotsync commit <scope> -m "message" -- <paths...>` when home changes should be recorded.

**`dotsync commit <scope> -m "message" <path>...`**: Commit the selected home-relative file/directory paths to the named scope branch, merge cascade through all descendant scopes, sync repo -> system, push to remote.

**`dotsync commit <scope> --all -m "message"`**: Commit every changed managed file for that scope. It does not scan all of home for unrelated new files; new paths are intentionally opted into with explicit path arguments.

**`dotsync diff`**: Show line-oriented diffs for managed home files that differ from the current machine scope. This is read-only and exits 1 when drift is present so scripts and agents can distinguish clean from dirty state.

**`dotsync view`**: Show a read-only overview of checked-in scope and file state. With `--scope <scope>`, show the managed file tree visible on that scope. With `--file <path>`, show the scopes where that file exists. With both `--scope <scope>` and `--file <path>`, print that file as it exists on that scope.

**`dotsync show conflict`**: Re-render the current paused cascade: DAG position, paused scope, colliding scopes, conflicted files, and resolution instructions. Works at any time while a pause exists, for agents that lost the original output.

**`dotsync continue`**: Continue a paused cascade after the conflict markers in the affected home files have been edited away to the resolved contents. Refuses if markers remain.

**`dotsync abort`**: Abort a paused cascade, restore scope branches to their pre-pause revisions, clear the pause marker, and sync the current machine home back to the restored repo state.

Syncing and commit forms diff system files against the repo before syncing. If any system file has drifted from what the repo expects, dotsync stops, shows the diff, and warns. `--force` still shows the diffs but proceeds anyway — so you always see what's being overwritten, even if you've chosen not to stop for it.

### Why one command?

Earlier designs had separate `dotsync` (sync), `dotsync commit` (commit + cascade), and `dotsync push` (push to remote). But these are always done together — there's no useful state where you've committed but not cascaded, or cascaded but not synced. Collapsing them into one command means fewer steps to forget, and agents only need to know one invocation.

## Agent skill

dotsync includes an agent skill (`dotfiles`) that triggers whenever any home directory config file is edited. The skill tells agents:

1. Edit files directly in `~/` at their real locations
2. Run `dotsync status` to see changed managed files
3. Read `.config/dotsync/config.toml` from the `all` scope to see available scopes — the config file contains comments explaining what each scope is for and guiding scope selection
4. Choose the root-est appropriate scope for the change
5. Run `dotsync commit <scope> -m "description" -- <paths...>` when done

This is the mechanism that makes the system agent-friendly. The tool itself is simple plumbing — the skill is what makes agents use the plumbing correctly. The comments in the config file are load-bearing: they're how agents learn "hyprland stuff goes on `hyprland`, not `linux`."

## What dotsync is NOT

- **Not a package manager.** Package lists can be tracked as files in the repo, but dotsync doesn't install anything.
- **Not a secret manager.** Don't put secrets in the repo. The repo is private but treat it as public.
- **Not a system config manager.** Files outside `~/` are out of scope. System-level config (like `/etc/systemd/logind.conf`) is tracked in notes but managed manually.
- **Not a bootstrapper.** Setting up a fresh machine (installing dotsync and git, running `dotsync init` the first time) is a manual process. dotsync is for steady-state maintenance.
