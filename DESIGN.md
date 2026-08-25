# dotsync — Design Story

**This document describes the target design, not the shipped state.** Things described here in the present tense may not be built yet — conflicts-as-commits and the convergence pass are the current examples. `PLAN.md` tracks what exists, what is in progress, and in what order the rest lands; when the two disagree about what dotsync does today, `PLAN.md` is right.

## The problem

You have config files scattered across `~/` on multiple machines. Some config is universal (`.gitconfig`), some is OS-specific (hyprland on linux), some is machine-specific (wallpaper paths). You want a single repo that is the source of truth for all of it, and you want AI agents — your primary method of editing config — to be able to maintain it naturally.

Dotfile management breaks down along several axes:

1. **Syncing** — copying files between a repo and the live system is tedious and error-prone. People forget, things drift, and nobody notices until something breaks on a fresh machine.

2. **Multiple versions** — the same _kind_ of config (e.g. "shell config") might differ between linux and windows, or between your laptop and your server. Most dotfile tools either ignore this (one branch, one machine) or punt to symlink farms with conditionals baked into the files themselves.

3. **Contributing changes** — when an agent (or human) edits a config file, getting that change into the right place in the repo should be frictionless. If it's not, agents make mistakes and humans stop doing it, and the repo rots.

4. **Agent-first** — AI agents are the primary editors of config. The system must be simple enough that a skill description can fully explain the workflow — agents can't handle complex mental models or ambiguous "which file do I edit?" situations.

## Why not existing tools?

- **Bare git repo with `$HOME` as worktree**: elegant but terrifying. Every `git clean` or careless `git checkout` can nuke your home directory. `git status` shows thousands of untracked files. Agents would need to be told "never run git clean" — a footgun.

- **Symlink managers (stow, etc.)**: stow creates symlinks from a package directory into `~/`. Multi-machine is DIY — you pick which "packages" to install per machine, but there's no built-in scoping. Agents don't know which package a file belongs to without being told.

- **Chezmoi**: the closest existing tool to what we want. It handles multi-machine via Go templates embedded in config files (`{{ if eq .chezmoi.os "linux" }}`), has a proper `diff`/`apply` workflow, and supports secrets. It would work. But templates mean the file in the repo is _not_ the file on the system — agents editing config need to understand both the config syntax and the template syntax. Our approach puts plain files in the repo, and uses git's merge machinery for scoping instead of templates. This means an agent edits exactly the file that ends up on the system, with no indirection.

- **Naive branching**: some people use git branches for per-machine config. This works until you need to propagate a universal change to all machines — you're manually cherry-picking or rebasing across N branches, re-solving the same conflicts. dotsync automates the propagation and uses merge commits to preserve conflict resolutions.

## The scope DAG

The core insight: machines aren't the only unit of variation. There are _scopes_ — overlapping categories that a machine belongs to. A linux laptop with hyprland belongs to scopes "all", "linux", and "hyprland". A windows desktop belongs to "all" and "windows".

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

A machine is just a leaf scope — there's nothing structurally special about it. The only difference is that a machine scope is the one whose files get synced to the live system. dotsync knows which scope is this machine's from the hostname, not from a user-visible checkout.

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

**Symlinks are treated as files, and are never followed** (Max, 2026-08-13: "for almost all intents we should treat symlinks as files and not follow them"). A link's content is its target string, so `commit` records a symlink as a symlink and sync writes it back into home as a symlink with the same target. Dotsync never reads the file a link points at, and never writes through a link — a home path that is a link where the repo holds a regular file is a difference in _kind_, reported as a change and replaced by sync rather than written through. This keeps `~/.config/nvim -> ~/src/nvim-config` recordable as what it is, and it keeps `commit -- selflink/` (a link to home) one entry rather than a walk of the whole home directory. Windows probably rejects symlinks on its scopes; that half is undecided in detail.

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

## The state space

Everything above this section describes what dotsync _does_. This section describes what can _exist_.

That distinction is worth a section of its own because a design which only ever specifies workflows leaves each implementation site to fill the gap on its own. Concretely: a scope's head can be in three states, this document never wrote down that it has states at all, and six places in the code each ended up deciding independently what the third one meant. They produced five different answers, two of which were to silently skip the scope. Nobody was careless — there was no definition to conform to, so each site invented one, and five independent inventions had no reason to agree.

The standard the rest of this section is held to is **make invalid states unrepresentable**, and that is only achievable if the valid states are written down first.

Written abstractly, on purpose. What the code calls each of these is a separate, explicitly non-authoritative mapping at the end of the section, so that renaming a type or restructuring a module is never a change to this document.

### Three things, and only three

1. **Home** — what this machine has right now at the managed paths.
2. **The repo** — what every machine should have, layered by scope. Config, conflicts and pause state are all data inside this.
3. **The mark** — which commit this machine last materialized into home. One id.

The mark is the one people leave out, so here is why it is not optional. Take `.config/app.conf`, where home holds `setting = "b"` and the machine scope holds `setting = "a"`. Those two facts alone do not tell you what to do next. If the mark says this machine last wrote `"a"` into home, then somebody edited the file afterwards, and syncing over it throws that edit away. If the mark says this machine last wrote `"b"`, then the repo moved on somewhere else and syncing over it is the entire job. Identical observations of home and the repo, opposite correct actions. The mark is the only thing that separates them.

That is also why the classification of local changes in the next section is a three-way comparison rather than a comparison of home against the repo. The "last-synced tree `L`" in that table is the tree of the commit the mark names.

`config.toml` is not a fourth thing. It is a managed file, living in home and on the `all` scope, which is exactly why editing it and committing it propagates like any other config. Its one genuinely special property is a self-reference: the scope graph is needed in order to compute the layering, and it is stored inside the layered thing. That works only because it lives on `all`, the root, which can be read without knowing the graph first.

Everything in this section is small, and that is the point. The essential complexity of the product is a DAG of config scopes, a rule for which scope a change belongs on, and a cascade that layers them down to each machine. jj has no opinion about any of it. Bookmarks, commits and conflict representation are _not_ fundamental — they are how (2) happens to be stored, and are free to change.

### A scope head has three states

Scopes are branches, so a scope has a head. That head is in exactly one of:

- **absent** — the repo holds no head for this scope. A scope named in `config.toml` that was never created is here.
- **exactly one commit** — the ordinary state.
- **contested** — two machines moved it and it currently holds two candidate values at once. Neither is "the" head.

Contested is the state that was missing. It is not exotic and it is not a corruption: "The convergence model" below argues that two machines writing to one scope is a routine event rather than an edge case, and contested is simply what that event looks like before it has been converged. A repo holding a contested `linux` is a healthy repo that has been told two things and has not yet been asked to reconcile them.

Two consequences follow, and both matter:

- **Divergence is not an error class.** A contested head is an input to a merge, not a condition to refuse on. Any command that treats "I cannot read a single commit id out of this head" as "this scope is not in the repo" is stating something false — the scope is in the repo, and it is contested.
- **Absent and contested are different states and must never share a representation.** They have nothing in common except that neither of them is a single commit id, and collapsing them is what produced the five answers.

### A managed path has a kind, not just content

"What is at this path" is a kind, and then whatever content that kind carries. The kinds are: absent, regular file (which also carries whether it is executable), symlink (whose content is its target string, never the file it points at), and directory.

Kind is not decoration on top of content, and treating a managed path as bytes alone loses real states. Two paths holding identical bytes can still differ: `.config/app.conf` as a regular file and `.config/app.conf` as a symlink whose target happens to be the same string are different things, and one is not a sync of the other. A shell script that is executable in the repo and not executable in home differs in a way that decides whether it runs. And `.config/app.conf` becoming the directory `.config/app.conf/main.conf` is an ordinary thing for an application to ask of its user, representable only if absent-versus-file-versus-directory is part of the model.

So a difference of kind is a difference: `status` and `diff` report it, and sync replaces rather than writes through. That is the same rule "Repo structure" states for symlinks, generalised to the reason behind it.

### A conflict is a base plus two sides

A conflict is a first-class object with three parts: the base — the content both sides started from — and the two sides themselves. It is not one side with the other discarded, and it is not a file with `<<<<<<<` in it.

The markers are a _rendering_ of the object, not the object. That matters twice over. It is why the base can be shown at all, since a rendering can include a part that a two-sided model would have had nowhere to put. And it is why dotsync can put the conflict in front of the resolver in its own output — base and both sides, labeled — without ever writing markers into a live config file; see "The resolution surface" below.

### A non-authoritative map to the code

The names below are where these states lived in the code as of v0.4.0. This table is a reading aid, not a specification. Refactoring is free to move any of it without an edit to this document, and where it disagrees with the abstract states above, the abstract states are right and this table is stale.

| State | Where it lives today |
| --- | --- |
| Home | the filesystem, at the managed paths — which jj reads and writes as its working copy, through dotsync's `WorkingCopy` implementation |
| The repo | the hidden jj repo at `~/.local/share/dotsync/repo/` |
| The mark | the parent of this machine's working-copy commit, in jj's own view (`wc_commit_ids`, keyed by the machine scope's name) |
| A scope head, three states | jj's `RefTarget`, which is a merge of optional commit ids — absent, single, or contested |
| The kind of a managed path | jj's `TreeValue`, whose `File` variant carries the executable bit and whose `Symlink` variant carries a target |
| A conflict | jj's own conflict representation, which is natively a base plus both sides |

Every row is jj's own type or jj's own storage, apart from home itself. That is deliberate, and "The jj decision" below explains why building a parallel model of any of them makes it lossier.

## Sync and commit direction

Plain sync is always repo -> system. The repo is the durable source of truth, and `dotsync` with no scope materializes the current machine scope into `~/`.

Commits are home -> repo for selected paths only. Users and agents edit files at their real home locations, inspect `dotsync status`, then run `dotsync commit <scope> -m "message" -- <paths...>` to record the selected home files to the appropriate scope. After committing, dotsync cascades that scope through descendants, syncs the current machine home, and pushes.

If a managed home file differs from what this machine last synced, that's a local change. `status` and `diff` report it, a plain sync carries it forward untouched (it stays reported afterwards), and `commit` is how it stops being local. A sync stops only when a local change and an incoming change collide in the same file — see "Local changes and the mark" below.

### Why not fully bidirectional?

Bidirectional sync requires conflict resolution between the repo and the system, which is a fundamentally different (and harder) problem than git merge conflicts. It also makes the mental model ambiguous: "which is the source of truth?" With unidirectional sync, the answer is always "the repo."

dotsync still _reads_ system files — it diffs them against the repo, reports status, and imports selected files during an explicit scoped commit. But it never treats an arbitrary home change as something to publish automatically. A repo update and a local home edit are different events, and the command shape makes the user choose which home paths belong in which scope.

The cost: to contribute a home change, you must name the scope and paths explicitly. That explicitness is the safety boundary that replaces a visible checkout or a broad "sync everything from home" mode.

An open question: some config files may end up with sections that shouldn't be checked in (e.g. secrets injected by an application). We don't have a strategy for this yet. Hopefully it doesn't come up, but if it does we'll need something — possibly `.gitignore` patterns for sections, or splitting the file.

### Local changes and the mark

Home is jj's working copy, through dotsync's own `WorkingCopy` implementation over the managed paths. Each run snapshots home into a working-copy commit whose parent is the mark, so the two per-machine facts live in jj's own view — which scope is this machine's (the workspace name) and which commit home last materialized (the parent) — and move atomically with the history they describe. This enables two things:

1. **Deletion semantics** — a file in the mark's tree but gone from the repo was materialized here and should be removed; a file never in the mark's tree was never managed.

2. **Attribution** — comparing home against the mark rather than the tip distinguishes "repo advanced elsewhere" from "home changed locally," so a sync accepts remote updates without calling them local changes, and carries local changes without publishing them.

A sync is one three-way merge — `merge(home, mark, tip)` — computed in memory and materialized only if it resolves, and only whole. The classification `status`, `diff`, `commit` and the sync all read is the per-path view of that same merge (last-synced tree `L`, home `H`, new tip `T`), so nothing can call a path conflicted that the sync then merges. Equality compares kind as well as content, per "A managed path has a kind" above. Presence and equality across the three sides is the whole domain, so every situation lands in exactly one class:

| Class | `L` / `H` / `T` | Behavior |
| --- | --- | --- |
| in sync | all three identical | nothing to do |
| incoming add | absent / absent / present | not a local change — sync writes it |
| incoming update ("stale, not yours") | present / equal to `L` / changed | not a local change — sync writes it; **`commit` refuses it** |
| incoming delete | present / equal to `L` / absent | not a local change — sync removes it from home |
| edit | present / changed / equal to `L` | a local change: reported, carried by sync, recorded by commit |
| edit, removed from the repo | present / changed / absent | same |
| deletion | present / absent / equal to `L` | a local change; sync does not put the file back; commit records the deletion |
| deletion, tip also changed | present / absent / changed | a delete/modify conflict — sync stops whole and presents it |
| diverged edit, combining | present / changed / changed differently, merging cleanly | both true at once: an incoming change sync merges in, and a local change that stays reported |
| diverged edit, colliding | present / changed / changed differently, conflicting | sync stops whole and presents base and both sides |
| already applied | present / changed / changed to the same bytes | nothing to do — this run's own commit, or a crashed run's writes |
| untracked collision | absent / present / present, differing and conflicting | sync stops whole — home holds content dotsync has never seen |
| untracked | absent / present / absent | not managed; only `commit` cares |
| converged deletion | present / absent / absent | nothing to do |

The row that carries the most weight is **incoming update**: home holds exactly what was last synced, and the tip has moved on. A two-sided comparison of home against the tip cannot tell it apart from a local edit, so `status` reports it as a change and a `commit` naming that path re-records the older bytes and cascades them — silently reverting whoever published the change. Naming the class is what makes that unrepresentable: `status` files it under incoming rather than changed, and `commit` refuses it, pointing at plain `dotsync`.

When a sync stops on a conflict it touches nothing: home is one coherent derivation of the mark, and a home written partly from the mark and partly from the tip would make any single answer to "what did this machine last sync?" a lie. The stop presents the base and both sides in dotsync's own output (never as markers in the live file), and the way out is to write the resolved content into the file at its real path and run `dotsync continue` — or `dotsync --force` to take the repo's side. Nothing about the stop is stored; a rerun recomputes the same merge from the same three trees.

On a machine with no working-copy record yet — a fresh `init`, or the first run after upgrading from a release that kept a state file — the working-copy commit is created as an empty-diff child of the machine scope's bookmark, and whatever home actually holds surfaces as ordinary local changes on the first snapshot. Nothing is removed from home and no missing file is read as a deletion, because there is no record of having put anything there.

**`--force` has two shapes, because the commands asking it do not all have something to scope the answer to.** Plain `dotsync` and `continue` name no paths, so their `--force` is blanket: materialize the repo's side whole, dropping every local change. `commit` names paths, and its `--force` rides that same list — it overrides the refusal for exactly those paths, takes home's side for them, and leaves every other local change alone. `dotsync commit linux -m msg --force -- .bashrc` overwrites `.bashrc` and nothing else; `dotsync --force` overwrites everything. Paths recorded on that authority are reported as `forced_overwrites`. `init` and `abort` refuse the flag: neither ever makes the choice, because `init` has nothing of yours to overwrite and `abort` exists to discard home edits.

## The jj decision

dotsync uses [jj (Jujutsu)](https://github.com/jj-vcs/jj) rather than raw git. The key reason: **jj can manipulate branches without touching the working copy.**

When you contribute a change, it needs to be committed on the right scope branch — not necessarily this machine's leaf scope. With git, this requires a checkout or worktree for the target branch, plus careful staging around unrelated home edits. If you have multiple edited files going to different scopes, this becomes a nightmare of stash juggling or visible workspaces.

With jj, dotsync creates a commit directly on the target scope's branch and merges descendant branches in the hidden repo, all without exposing a checkout to the user. Home remains the editing surface; the hidden repo remains implementation detail.

jj is also git-compatible — the repo is a valid git repo, pushable to GitHub, cloneable with git. jj is just a better local interface for the graph manipulation dotsync needs.

One risk: jj is newer and less well-known than git. AI agents may not have strong intuitions for jj commands and concepts. So **hide jj from the user interface**: agents interact only with `dotsync` commands, never run `jj` directly, and never need to learn what a bookmark is.

That is an instruction about the interface. It is not an instruction about the code, and it is worth saying so explicitly, because an earlier version of this document said only that dotsync "abstracts jj away entirely" and that turned out to read as both. The cheapest way to abstract a rich type is to narrow it where you first touch it — read the one case you care about out of it and pass that along — so that is what happened. A bookmark position, which jj models as absent, single, _or_ contested, was read out through a helper that answers only the single case; the five sites downstream then each decided what the missing answer meant, and one of them says the scope is not in the repo when in fact the scope is in the repo and contested. The message is wrong because the type it was derived from could not hold the truth.

The corrected instruction: **hide jj from the user; do not narrow jj's types in the code — or, where a narrower type is genuinely wanted, prove it can hold everything the wider one could.**

The same rule read from the other direction: wherever dotsync builds its own model of something jj already models, dotsync's copy is the lossier one. A managed file's content carried without its kind is a parallel copy of jj's tree value with the executable bit and the symlink case dropped. A cache of scope heads is a parallel copy of the repo's bookmarks that has to be kept in step by convention. "The state space" above lists the states these copies have to be able to hold, and its last table is the map from those states to the jj types that already hold them.

**Requirement: dotsync must never depend on the jj CLI binary at runtime.** jj is linked in as a library (jj-lib); user machines do not have and must not need jj installed. The library link is functional, not just packaging: dotsync needs operations the CLI doesn't expose, like computing a merge in memory to report would-be conflicts without creating commits or moving bookmarks. Known caveat: jj-lib's supported fetch/push mechanism shells out to a `git` subprocess, so a `git` binary on PATH is currently a runtime dependency for network operations. That's acceptable for now and recorded here so nobody assumes full self-containment.

## The convergence model

Scope branches are normal repo history, and multiple machines write to them concurrently. That makes bookmark divergence a **routine event, not an edge case**. Walk through the common case: machine A commits to `all`, and the cascade creates a merge commit on every descendant scope — the entire DAG — then pushes. Machine B, which hasn't fetched yet, commits to `linux`; its cascade moves `linux` and everything below it. When B next talks to the remote, half a dozen scope bookmarks have diverged — local and remote each have commits the other lacks — from two innocent, non-overlapping edits. Any design that treats divergence as an error fails on the second machine, every time.

So dotsync's core operation is a **convergence pass**: for each scope in topological order, the new head is the merge of {local head, remote head, updated parent-scope heads}, skipping commits where nothing changed. This one operation subsumes what would otherwise be three separate mechanisms:

- local behind remote → the merge is trivially the remote head (fast-forward)
- local ahead of remote → the merge is trivially the local head (unpushed work; push it when pushing)
- diverged → a real merge commit, pausing on file conflicts exactly like any cascade merge
- parent scope advanced → the ordinary cascade merge

Every state a machine can be in — mid-crash, post-failed-push, freshly offline-edited — is just an input to the next convergence pass. There is no separate "recovery."

**Pull first, always.** Every mutating command opens with fetch + convergence, so remote changes are integrated _before_ new work builds on top of them — never discovered mid-flow after edits and merges are already in progress. Commit is then: converge, add the new commit, converge again (to cascade it), push.

**Push is a loop, not a step.** A rejected push isn't an error; it means another machine pushed first. Fetch, converge, push again. Push happens immediately after history is created — before the home sync — so a sync-side stop (a conflict with home) never strands committed history unpushed.

**Read-only commands never mutate.** `status`, `diff`, and `view` don't move bookmarks, create commits, or touch home. They fetch (when online) and _report_ what convergence would do — including "pulling would conflict on these files in scope X" — computed as in-memory merges via jj-lib. Only `dotsync` (sync), `commit`, and `continue` actually converge.

**Offline is just deferred convergence.** If fetch fails due to network, dotsync skips it and proceeds against last-known remote state. Local history builds up ahead of the remote — which is a normal convergence input, handled the next time the machine is online. There is no offline mode and no queue.

## Conflict resolution in home

There is deliberately no visible working copy — a working copy next to the live config would mean three copies of everything. But the live config directory **is the working copy for all intents and purposes**, and it gets the full working-copy treatment. The user can never move it backward or sideways to another version or scope (inspection is done via `dotsync view`); it only ever goes forward. And when a merge conflicts, the conflict has to be put in front of whoever resolves it — whether that means writing it into the files they work in is the one open question here, and "The resolution surface" below is where it is left open.

### Conflicts are commits, not a paused mode

jj's defining feature is that conflicts are first-class objects inside commits: a merge commit can be created with a conflicted tree, and descendants inherit the conflict through their own merges until it is resolved. dotsync leans on this fully. **The cascade never pauses structurally** — every convergence pass completes in one atomic transaction, writing every merge commit, conflicted or not. "Paused" is not a stored mode; it is a derived observation: _one or more local scope heads have conflicted trees._ The conflicted heads act as the queue of pending resolution work.

An earlier design stored pause intent in a machine-local state file (merge parent ids, remaining cascade steps, pre-pause heads) and refused to create conflicted history. That was a holdover from the working-copy era and created a class of dead ends: the file was written outside the repo transaction (crash = half-cascaded bookmarks with no record), it was invisible (no command displayed it), and it was the only copy of the intent. With conflicts in history, every piece of that state is derivable: the conflicted scope and files from the head trees, the merge parents and description from the conflicted commit itself, and nothing else is needed because there are no "remaining steps" — the cascade already completed around the conflict.

**Principle: keep exactly the minimum required state.** The only machine-local state is jj's own working-copy record — the view's working-copy commit entry (the machine scope and the mark, per-machine facts that shared history cannot contain) and the working copy's freshness record beside the repo. Anything derivable from the repo must be derived, never cached in a side file. Derived state is automatically correct after a crash; stored state is a fresh opportunity to be wrong.

### The resolution surface

**Settled**: when a merge conflicts, the conflict is put in front of the agent that has to resolve it, showing both sides _and_ the base, with the sides labeled by scope name rather than commit id. The base is in because a conflict _is_ a base plus two sides — see "The state space" above — and jj carries all three, so omitting it would mean discarding a part dotsync already holds. Max, on that: _"Yes the base is supposed to be included. I'm sure JJ supports that."_

**Where: the preference is to present the conflict without writing anything into home** (Max, 2026-08-19, "the overwhelming preference"). The reason: conflict markers in home are broken config. While `<<<<<<<` / `|||||||` / `=======` / `>>>>>>>` markers sit in a live config file, that file is not valid config, so the application it configures reads a broken file for exactly as long as the pause lasts — the machine is broken precisely while the conflict is being fixed. The alternative — materializing markers into the affected home files through ordinary sync, in the standard format agents have deep priors on — remains the fallback if the preference fails validation.

**The preference is validated empirically by the agent validation loop** (PLAN item 3) — by watching a real agent resolve a real conflict, not by argument here. Not because the preference is in doubt, but because conflicts are fundamental to the tool, and "a real agent can reliably resolve one from what it is shown" is the bar the presentation has to clear. Because the conflict is already a real object with all three parts, this is a rendering choice over that object rather than a design still to be invented, and nothing above this paragraph changes whichever way it goes.

The bullets below say which of them assume an answer.

- **If markers are materialized**, a conflict anywhere in this machine's scope ancestry propagates down into the machine scope's tree, so ordinary sync writes it into the affected home files. Drift detection then treats those markers as the expected home content while they are there — which is what stops a forced sync from replacing a resolution in progress with the unresolved file.
- **`dotsync show conflict` renders the conflict state at any time**: DAG position, the rootmost conflicted scope, which scopes' changes are colliding, the conflicted files, and the instructions. Because it renders derived state rather than a stored record, it is automatically correct after any crash, on any machine. `status` points here whenever conflicts exist.
- **`continue` applies the resolution at the rootmost conflicted scope and propagates it**: descendant merges that inherited the same conflict resolve automatically through jj's descendant rewriting. It refuses a resolution that still holds conflict markers, since markers recorded as the merged contents would cascade to every descendant and reach every other machine. `commit` refuses while any local scope head is conflicted, pointing at the resolution flow. Whether `continue` exists at all rides on the same experiment — see below.
- **Conflicts outside this machine's ancestry** (e.g. a cascade from `all` conflicting only in the `windows` subtree while this machine is linux) don't appear in home naturally. `show conflict` reports them either way. The mode switch has to be stated loudly whichever presentation wins — "this is the conflicted merge for scope `windows`; it is not your machine's config; after `continue` or `abort` your machine's version comes back" — because without it the agent reads another machine's config as its own. If markers do go into home, the affected home path is the temporary resolution buffer, and that sentence is what keeps it legible as one.

#### Whether `continue` survives

Under the preferred answer it survives; the reasoning is worth writing down so the experiment's result can be applied without re-deriving it.

If markers are materialized, "the agent is done" is legible from the file itself: the markers are gone. `continue` is then the agent restating a fact dotsync can check for itself, and it deletes.

If they are not materialized, home reads identically before the agent starts and after it decides to keep its own side unchanged. "I am done" becomes exactly the thing dotsync cannot find out on its own, and `continue` survives on the standing rule that every write command carries a decision only the user can make. The tempting shortcut — treat an unchanged file as unresolved — is the thing to avoid: it is silently wrong for the agent that legitimately resolved the conflict by keeping its own side.

- **Conflicted heads are not pushed.** They stay local-ahead — a normal convergence state — until resolved; everything non-conflicted still pushes. This keeps the shared remote free of conflict encodings (which plain git tooling renders poorly) at the cost that only the machine holding the conflict can resolve it.
- **`abort` goes back to the last fully cascaded machine scope tip.** It abandons the unpushed conflicted commits, returns the affected scope bookmarks to their last non-conflicted positions, and reverts **all** the config files — a full sync of home to the machine scope's last fully cascaded tip, not a selective restore. Whatever the pause put into home, and the home edit that caused the aborted commit, are both gone from home afterward; that's the point of abort. Pushed history is never touched — conflicted heads are never pushed, so everything abandoned is local-only. Clean remote integration discarded along the way costs nothing: the next convergence pass re-derives it.

## Failure model: no dead ends

Every state dotsync can produce — including states produced by crashing at the worst possible moment, a failed push, or another machine racing — must be a state that dotsync commands alone can diagnose and recover from. If a run is interrupted anywhere, the remedy is "run dotsync again" (or `continue`/`abort` for a paused cascade). Never repo surgery.

This is mostly a corollary of the convergence model: interrupted work leaves local-ahead or diverged bookmarks, and those are ordinary convergence inputs. The remaining obligations are ordering and atomicity: persist pause intent in the same effective step as the history it describes, push as soon as history exists, and keep read-only commands working on any state (they report weirdness; they don't refuse to run because of it).

## Commands

The steady-state command is `dotsync`, and it is the one an agent runs by reflex. The others exist because they answer questions `dotsync` cannot: how to join a remote in the first place, what changed here, and what to do when a cascade pauses. `dotsync` itself never splits into commit/cascade/push steps — see "Why one command?" below.

**`dotsync init <remote-url>`**: Clone the remote into the hidden repo, work out this machine's OS and machine scopes, create them if the remote does not have them yet, and sync the resulting machine scope into home. The only command that requires the remote to be reachable: it is the whole of its job. It writes the scope graph to `.config/dotsync/config.toml`.

**`dotsync`** (no arguments): Pull and converge scope branches (merging remote changes and cascading, pausing on conflicts), sync repo -> system, push. It does not import home edits; use `dotsync status` and `dotsync commit <scope> -m "message" -- <paths...>` when home changes should be recorded.

**`dotsync commit <scope> -m "message" <path>...`**: Commit the selected home-relative file/directory paths to the named scope branch, merge cascade through all descendant scopes, sync repo -> system, push to remote. It refuses a named path whose home content is not a change made on this machine — see the classification above. **The scope must be one this machine belongs to** — its own machine scope or an ancestor of it. Committing to a scope this machine does not descend from is refused (Max, 2026-08-13): home only ever moves forward, and the config it holds is supposed to stay valid, so there is no version of another machine's branch that this machine can claim to have started from. To contribute to a machine family you are not on, put the shared material and the pattern for it on the common ancestor, and leave it to an agent running on that family to add its own drop-ins on its own scope. Note this is only about _choosing_ a commit target: a cascade from a shared ancestor still merges into descendant scopes this machine is not on, so conflicts outside this machine's ancestry remain a normal event — see "Conflict resolution in home".

**`dotsync commit <scope> -m "message"`** (no paths): Commit every managed file this machine has changed, which is exactly the set `dotsync status` lists as changes. It does not scan all of home for unrelated new files; new paths are intentionally opted into with explicit path arguments.

**`dotsync status`**: List managed files this machine has changed, and separately the files another machine changed that home has not caught up to. Says so when a cascade is paused, because that machine can commit nothing until it is resolved. Read-only, and exits 0 either way.

**`dotsync diff`**: Show line-oriented diffs for managed home files with local changes. Read-only, and exits 1 when local changes are present so scripts and agents can distinguish clean from dirty state. A file the repo has moved on from while home stayed put is not a local change, so a machine that is merely behind exits 0 — the same answer `status` and plain `dotsync` give.

**`dotsync view`**: Show a read-only overview of checked-in scope and file state. With `--scope <scope>`, show the managed file tree visible on that scope. With `--file <path>`, show the scopes where that file exists. With both `--scope <scope>` and `--file <path>`, print that file as it exists on that scope.

**`dotsync show conflict`** _(not implemented — PLAN item 3)_: Re-render the current paused cascade: DAG position, paused scope, colliding scopes, conflicted files, and resolution instructions. Works at any time while a pause exists, for agents that lost the original output.

**`dotsync continue`** _(existence conditional — see "Whether `continue` survives")_: Continue a paused cascade once the conflict has been resolved, recording the resolved contents at the rootmost conflicted scope. Refuses a resolution that still holds conflict markers.

**`dotsync abort`**: Abort a paused cascade, restore scope branches to their pre-pause revisions, clear the pause marker, and sync the current machine home back to the restored repo state.

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | The command did what it says. |
| 1 | dotsync stopped, or `dotsync diff` found changes. Under `--output json` the payload's `status` separates the two: `"error"` for a stop, `"ok"` for the changes `diff` found. |
| 2 | The command line was wrong. |
| 3 | A paused cascade is waiting: resolve the conflict and run `dotsync continue`, or run `dotsync abort` to discard it. |

3 is a property of the state, not of the command that met it: the run that creates a pause, a `commit` that runs into one, and a `continue` that finds nothing resolved all exit 3, because they all have the same remedy. Only `diff` ever exits non-zero without having stopped, and it does so because a script needs to tell clean from dirty without parsing.

`--force` takes the repo's side instead of stopping, and reports every local change it discarded — so you always see what was overwritten, even having chosen not to stop for it. On `commit`, `--force` covers only the paths that commit named; see "Local changes and the mark" above.

### Why one command?

Earlier designs had separate `dotsync` (sync), `dotsync commit` (commit + cascade), and `dotsync push` (push to remote). But these are always done together — there's no useful state where you've committed but not cascaded, or cascaded but not synced. Collapsing them into one command means fewer steps to forget, and agents only need to know one invocation.

## Agent skill

dotsync includes an agent skill (`dotfiles`) that triggers whenever any home directory config file is edited. The skill tells agents:

1. Edit files directly in `~/` at their real locations
2. Run `dotsync status` to see changed managed files
3. Read `.config/dotsync/config.toml` from the `all` scope to see available scopes
4. Choose the root-est appropriate scope for the change
5. Run `dotsync commit <scope> -m "description" -- <paths...>` when done

This is the mechanism that makes the system agent-friendly. The tool itself is simple plumbing — the skill is what makes agents use the plumbing correctly.

An earlier version of this document claimed the comments in `config.toml` are load-bearing — that they are how an agent learns "hyprland stuff goes on `hyprland`, not `linux`". They are not (Max, 2026-08-14: _"I don't think the scope comments are 'load bearing' lol? they're pretty obvious"_). A scope called `hyprland` says what belongs on it by being called `hyprland`, and step 4 above is the actual rule. Comments there are ordinary commentary, useful when a scope's name is not self-evident and carrying nothing when it is. Read as a requirement, that claim is what produced machinery to generate a comment for every scope at `init` and to preserve comments when a joining machine edits the file; nothing else depends on that machinery existing.

## What dotsync is NOT

- **Not a package manager.** Package lists can be tracked as files in the repo, but dotsync doesn't install anything.
- **Not a secret manager.** Don't put secrets in the repo. The repo is private but treat it as public.
- **Not a system config manager.** Files outside `~/` are out of scope. System-level config (like `/etc/systemd/logind.conf`) is tracked in notes but managed manually.
- **Not a bootstrapper.** Setting up a fresh machine (installing dotsync and git, running `dotsync init` the first time) is a manual process. dotsync is for steady-state maintenance.
