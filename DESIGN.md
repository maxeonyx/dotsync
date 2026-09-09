# dotsync — Design Story

This document describes dotsync as it is: the model, why it is that model, and
the contract its commands keep. `PLAN.md` is where anything not yet built is
written down.

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

A machine is just a leaf scope — there's nothing structurally special about it. The only difference is that a machine scope is the one whose files get synced to the live system. dotsync knows which scope is this machine's from the hostname, not from a user-visible checkout, and the scope it names has to be a leaf: config on a scope reaches every machine below it, so a scope something else hangs off cannot be one machine's own.

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

There is no config file. The graph is the repo's own structure: a scope is a
branch, and the commit that created it is written onto the heads of its parents
— so a scope's ancestors are the scopes whose creation commits its history
holds, and its parents are the nearest of those. The creation commit's message
names the scope, and says what belongs on it where the name does not say so
already.

Nothing else on the remote is a scope. The remote is a git remote and anything
with git can push to it; a branch that was not created as a scope is not one,
and dotsync neither reads, cascades nor publishes it.

## The state space

Everything above this section describes what dotsync _does_. This section describes what can _exist_.

It is worth a section of its own because a design that specifies only workflows leaves every implementation site to invent the missing states for itself, and independent inventions have no reason to agree. The standard the rest of this section is held to is **make invalid states unrepresentable**, which is only achievable if the valid states are written down first.

Written abstractly, on purpose. What the code calls each of these is a separate, explicitly non-authoritative mapping at the end of the section, so that renaming a type or restructuring a module is never a change to this document.

### Three things, and only three

1. **Home** — what this machine has right now at the managed paths.
2. **The repo** — what every machine should have, layered by scope. Config is data inside it. A conflict is not: it is what merging two of its heads produces, and nothing stores one.
3. **The mark** — which commit this machine last materialized into home. One id.

The mark is the one people leave out, so here is why it is not optional. Take `.config/app.conf`, where home holds `setting = "b"` and the machine scope holds `setting = "a"`. Those two facts alone do not tell you what to do next. If the mark says this machine last wrote `"a"` into home, then somebody edited the file afterwards, and syncing over it throws that edit away. If the mark says this machine last wrote `"b"`, then the repo moved on somewhere else and syncing over it is the entire job. Identical observations of home and the repo, opposite correct actions. The mark is the only thing that separates them.

That is also why the classification of local changes in the next section is a three-way comparison rather than a comparison of home against the repo. The "last-synced tree `L`" in that table is the tree of the commit the mark names.

The scope graph is not a fourth thing either. It is (2) read structurally
rather than from any file's contents, so there is nothing to keep in step with
it: a scope dotsync can name is a scope the cascade can act on, by
construction.

Everything in this section is small, and that is the point. The essential complexity of the product is a DAG of config scopes, a rule for which scope a change belongs on, and a cascade that layers them down to each machine. jj has no opinion about any of it. Bookmarks, commits and conflict representation are _not_ fundamental — they are how (2) happens to be stored, and are free to change.

### A scope head has three states

Scopes are branches, so a scope has a head. That head is in exactly one of:

- **absent** — the repo holds no head for this scope, which for a machine means its own scope was created and something outside dotsync has since moved or removed the branch.
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

A reading aid, not a specification: refactoring may move any of it without an edit to this document, and where the two disagree the abstract states above are right.

| State | Where it lives today |
| --- | --- |
| Home | the filesystem, at the managed paths — which jj reads and writes as its working copy, through dotsync's `WorkingCopy` implementation |
| The repo | the hidden jj repo at `~/.local/share/dotsync/repo/` |
| The mark | the parent of this machine's working-copy commit, in jj's own view (`wc_commit_ids`, keyed by the machine scope's name) |
| A scope head, three states | jj's `RefTarget`, which is a merge of optional commit ids — absent, single, or contested |
| The scope graph | derived in `scope_graph::derive` from the bookmarks and the ancestry of their creation commits |
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

When a sync stops on a conflict it touches nothing: home is one coherent derivation of the mark, and a home written partly from the mark and partly from the tip would make any single answer to "what did this machine last sync?" a lie. The stop presents the base and both sides in dotsync's own output (never as markers in the live file), and the way out is to write the resolved content into the file at its real path and run `dotsync continue` — or `dotsync discard <path>` to take the repo's side. Nothing about the stop is stored; a rerun recomputes the same merge from the same three trees.

On a machine with no working-copy record yet — a fresh `init`, or the first run after upgrading from a release that kept a state file — the working-copy commit is created as an empty-diff child of the machine scope's bookmark, and whatever home actually holds surfaces as ordinary local changes on the first snapshot. Nothing is removed from home and no missing file is read as a deletion, because there is no record of having put anything there.

**A local change ends in one of two ways, and both name paths.** `dotsync commit <scope> -- <paths>` makes it everybody's; `dotsync discard <paths>` decides against it, writing the scope's version into home instead. Deleting the file yourself is neither, because a deletion is a local change too — home would come back empty rather than canonical, which is why discarding needs a command of its own. Naming a path that holds no change of yours is a stop rather than a run that discarded nothing: discarding cannot be undone, and a mistyped path is likelier than a change of mind. What a run discarded is reported as `overwritten_files`, the same field `init` and `abort` use, because all three take the head's side of something.

## The jj decision

dotsync uses [jj (Jujutsu)](https://github.com/jj-vcs/jj) rather than raw git. The key reason: **jj can manipulate branches without touching the working copy.**

When you contribute a change, it needs to be committed on the right scope branch — not necessarily this machine's leaf scope. With git, this requires a checkout or worktree for the target branch, plus careful staging around unrelated home edits. If you have multiple edited files going to different scopes, this becomes a nightmare of stash juggling or visible workspaces.

With jj, dotsync creates a commit directly on the target scope's branch and merges descendant branches in the hidden repo, all without exposing a checkout to the user. Home remains the editing surface; the hidden repo remains implementation detail.

jj is also git-compatible — the repo is a valid git repo, pushable to GitHub, cloneable with git. jj is just a better local interface for the graph manipulation dotsync needs.

One risk: jj is newer and less well-known than git. AI agents may not have strong intuitions for jj commands and concepts. So **hide jj from the user interface**: agents interact only with `dotsync` commands, never run `jj` directly, and never need to learn what a bookmark is.

That is an instruction about the interface, and it is emphatically not one about the code: **do not narrow jj's types — or, where a narrower type is genuinely wanted, prove it can hold everything the wider one could.** The cheapest way to abstract a rich type is to read the one case you care about out of it at the boundary, and every site downstream then has to invent an answer for the cases that were dropped. A bookmark position is the example that cost the most: jj models it as absent, single _or_ contested, and a helper answering only the single case turned "contested" into "this scope is not in the repo", which is false.

The same rule from the other direction: wherever dotsync builds its own model of something jj already models, dotsync's copy is the lossier one — content without its kind loses the executable bit and the symlink case; a cache of scope heads has to be kept in step by convention. "The state space" above lists the states these copies would have to hold, and its last table is the map to the jj types that already hold them.

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

**Read-only commands report; they never decide.** `status`, `diff`, and `view` move no scope bookmark, publish nothing, and never write to home. They fetch (when online) and _report_ what convergence would do — including "pulling would conflict on these files in scope X" — by running the convergence pass itself in a transaction nothing commits. Only `dotsync` (sync), `commit`, and `continue` converge for real.

Two things they do write, worth stating rather than claiming otherwise:

- Importing what the fetch brought fast-forwards a scope's local bookmark onto a position the remote already published. That is the one case of convergence that creates nothing — the merge is trivially the remote's head — so it takes no decision away from the next run, and there is no state a read-only command can leave behind that a plain `dotsync` would not have reached anyway. Not doing it needs the run to answer from a fetched view it never records, which is a change to how a run holds the repo rather than to what convergence is.
- Predicting the pass on a machine that has unconverged work writes the merges it predicts into the object store, where no bookmark and no operation points at them. They are the same objects the next writing run creates, and they cost what jj's own snapshots cost.

**Offline is just deferred convergence.** If fetch fails due to network, dotsync skips it and proceeds against last-known remote state. Local history builds up ahead of the remote — which is a normal convergence input, handled the next time the machine is online. There is no offline mode and no queue.

## Conflict resolution in home

There is deliberately no visible working copy — a working copy next to the live config would mean three copies of everything. But the live config directory **is the working copy for all intents and purposes**, and it gets the full working-copy treatment. The user can never move it backward or sideways to another version or scope (inspection is done via `dotsync view`); it only ever goes forward. And when a merge conflicts, the conflict is put in front of whoever resolves it — in dotsync's own output, never in the file itself; see "The resolution surface" below.

### "Paused" is derived, not stored

A merge that holds a file two sides changed differently is not history. The pass stops at it, writes nothing for that scope or anything below it, and the run stops with the conflict in front of whoever has to resolve it. So "paused" is not a stored mode and not a commit either: it is the answer to _where would the pass stop_, and it is computed by running the pass in a transaction nothing commits. The same commits give the same merge, so a resolution shows up the moment it is written, a crash leaves nothing behind to be stale, and a read-only command cannot describe the machine differently from the run that follows it.

Three paused states exist, and each is derived from a different place:

- **a scope's merge** — from the pass;
- **home against this machine's own scope head** — from home, the mark and the head, which is the same three-way merge every sync computes;
- **a `commit` whose own merge conflicted** — from the one record dotsync keeps beside the repo, because that merge has home as one of its two sides. Recomputing it after the answer is written finds nothing conflicted, and the scope and the message the run was carrying only ever existed in its arguments.

**Principle: keep exactly the minimum required state.** Machine-local state is jj's own working-copy record — the view's working-copy commit entry (the machine scope and the mark, per-machine facts that shared history cannot contain) and the working copy's freshness record — plus one record of a run that stopped: where every scope stood before it wrote anything, which is where `abort` returns them to, and the commit it was making if that is what stopped it. Each of those is a fact about a run rather than about the repo, which is why no amount of reading the repo recovers it. Anything that *is* derivable from the repo must be derived, never cached in a side file. Derived state is automatically correct after a crash; stored state is a fresh opportunity to be wrong.

### The resolution surface

When a merge conflicts, the conflict is put in front of the agent that has to
resolve it: every conflicted file, with both sides _and_ the base, each labeled
with the scope it came from rather than with a commit id. The base is in
because a conflict _is_ a base plus two sides — see "The state space" above —
and jj carries all three, so leaving it out would mean discarding a part
dotsync already holds. Max: _"Yes the base is supposed to be included."_

**Nothing is written into home.** Conflict markers in a live config file are
broken config: the file stops being valid for exactly as long as the pause
lasts, so the application it configures reads a broken file precisely while
somebody is fixing it. The versions therefore exist only in dotsync's output —
which is why `dotsync view` prints them again, for the agent whose session
ended or whose terminal scrolled.

That leaves "I am done" as the one thing dotsync cannot find out for itself:
home reads identically before the agent starts and after it decides to keep the
version already there. So `continue` exists, and it carries the decision.

- **`continue` is the pass again with an answer supplied.** It recomputes the
  merge that stopped, takes home's bytes at exactly the paths that merge could
  not resolve, and hands those to the pass — so the resolution is recorded on
  the scope that stopped, and everything below it merges a parent that moved,
  which is ordinary convergence. There is no remaining cascade to remember: the
  pass finds what is left by looking. An answer that does not cover a second
  conflict leaves that merge conflicted, and the pass stops there as it would
  have anyway, which is what keeps home's bytes off a conflict nobody has been
  shown. `commit` refuses while a merge is waiting, pointing at the resolution
  flow.
- **"Resolved" is a property of the content, and refusing markers is the whole
  of it.** `continue` refuses a file that still holds conflict markers, since
  markers recorded as the merged contents cascade to every descendant and reach
  every other machine's live config. It refuses nothing else. In particular an
  unchanged file is a resolution — the agent read both versions and kept the one
  already there — so the pause says as much, and the tempting "unchanged means
  unresolved" check is the thing to avoid: it is silently wrong for exactly the
  agent that did the work properly. Markers are detected by a start line *and*
  an end line, because a lone run of seven `=` or `-` characters is ordinary
  config.
- **Conflicts outside this machine's ancestry** (e.g. a cascade from `all`
  conflicting only in the `windows` subtree while this machine is linux) don't
  appear in home naturally, and resolving one borrows the home path as a scratch
  buffer for somebody else's config. The pause states the switch loudly — "this
  machine is `mx-xps-cy`, which does not descend from `windows`, so what you are
  resolving is not this machine's config; `mx-xps-cy`'s own version comes back
  after `continue` or `abort`" — because without it the agent reads another
  machine's settings as its own.

  The borrowing has to end, and that is one rule rather than a mode: **`continue`
  lets the scope decide the conflicted paths again**, taking the head's side at
  exactly those paths and carrying every other local change as usual. Where the
  merge was on this machine's own path the resolution has just cascaded into its
  scope, so the head's side *is* what the agent typed and nothing moves. Where it
  was another machine's, home gets its own config back — without which the
  resolution stays a local change that no scope this machine syncs from can ever
  settle, so `status` reports it for ever. The run says which of the two
  happened, because a sync that discards home content and does not say why reads
  as lost work.
- **A pause publishes nothing**, and `dotsync continue` publishes the lot. The scopes the pass converged before it stopped stay local-ahead, which is an ordinary convergence state, and the read-only commands name them so that a machine holding history back does not read as a machine with nothing to say.

  All or nothing, because the cause of a cascade conflict is a commit on a scope that merged cleanly: `dotsync commit all` collides at `linux`, and `all` is the scope that is fine. Publish `all` and no local command can take it back, so every later run re-derives the same conflict and `dotsync abort` can never clear it. Withholding keeps abort able to undo what the run did, and keeps the shared remote free of conflict encodings that plain git tooling renders poorly — at the cost that only the machine holding the conflict can resolve it.
- **`abort` takes back what the run that stopped committed, and puts home back.** Every scope it moved returns to where that run found it, and home is synced to the machine scope's restored tip — reverting **all** the config files, not a selective restore. Whatever the pause put into home, and the home edit that caused the aborted commit, are both gone from home afterward; that's the point of abort. Nothing published is touched, because a pause publishes nothing, so everything discarded is local-only. Clean remote integration discarded along the way costs nothing: the next convergence pass re-derives it.

  **Abort is not a way out of every conflict, and says so when it is not.** When the colliding change arrived from the remote instead of from this machine, there was nothing of this machine's in the way: home goes back, the merge still stops in the same place, and the run reports that and exits with the code that means a merge is waiting. Only a resolution ends that one. Which is not a dead end — `continue` is the way through, and abort names it.

## Failure model: no dead ends

Every state dotsync can produce — including states produced by crashing at the worst possible moment, a failed push, or another machine racing — must be a state that dotsync commands alone can diagnose and recover from. If a run is interrupted anywhere, the remedy is "run dotsync again" (or `continue`/`abort` for a paused cascade). Never repo surgery.

This is mostly a corollary of the convergence model: interrupted work leaves local-ahead or diverged bookmarks, and those are ordinary convergence inputs, and a merge that stops is recomputed rather than remembered. The remaining obligations are ordering and atomicity: push as soon as history exists, and keep read-only commands working on any state (they report weirdness; they don't refuse to run because of it).

## Commands

The steady-state command is `dotsync`, and it is the one an agent runs by reflex. The others exist because they answer questions `dotsync` cannot: how to join a remote in the first place, what changed here, and what to do when a cascade pauses. `dotsync` itself never splits into commit/cascade/push steps — see "Why one command?" below.

**`dotsync init <remote-url> [--parent <scope>]...`**: Clone the remote into the hidden repo, create this machine's own scope, and sync it into home. The only command that requires the remote to be reachable: it is the whole of its job.

Joining a remote that already has scopes means naming the parents, and naming one that is not there is a stop that lists the ones that are. This is not a convenience: a hostname cannot tell a `home-linux` from a `work-linux`, the graph is append-only, and a scope guessed into the wrong place cannot be moved afterwards. A remote with no scopes on it has nothing to choose from, so that machine gets the root scope `all`, a scope named for its OS, and its own leaf under that — the one shape dotsync can justify without being told.

A machine whose scope already exists adopts it, and refuses `--parent`, because where a scope hangs was decided when it was created.

**`dotsync create-scope <name> --parent <scope>... [-m "what belongs here"]`**: Create a scope holding everything its parents hold. This is the whole of what can happen to the graph: nothing renames, reparents or deletes a scope, which is what lets the graph be structural. Machines join a scope with `init --parent`, so a scope created now is for the machines that join under it. Rearranging a graph is open (PLAN §2.7).

**`dotsync`** (no arguments): Pull and converge scope branches (merging remote changes and cascading, pausing on conflicts), sync repo -> system, push. It does not import home edits; use `dotsync status` and `dotsync commit <scope> -m "message" -- <paths...>` when home changes should be recorded.

**`dotsync commit <scope> -m "message" <path>...`**: Commit the selected home-relative file/directory paths to the named scope branch, merge cascade through all descendant scopes, sync repo -> system, push to remote. It refuses a named path whose home content is not a change made on this machine — see the classification above. **The scope must be one this machine belongs to** — its own machine scope or an ancestor of it. Committing to a scope this machine does not descend from is refused (Max, 2026-08-13): home only ever moves forward, and the config it holds is supposed to stay valid, so there is no version of another machine's branch that this machine can claim to have started from. To contribute to a machine family you are not on, put the shared material and the pattern for it on the common ancestor, and leave it to an agent running on that family to add its own drop-ins on its own scope. Note this is only about _choosing_ a commit target: a cascade from a shared ancestor still merges into descendant scopes this machine is not on, so conflicts outside this machine's ancestry remain a normal event — see "Conflict resolution in home".

**`dotsync commit <scope> -m "message"`** (no paths): Commit every managed file this machine has changed, which is exactly the set `dotsync status` lists as changes. It does not scan all of home for unrelated new files; new paths are intentionally opted into with explicit path arguments.

**`dotsync status`**: List managed files this machine has changed, and separately the files another machine changed that home has not caught up to. Read-only, and exits 0 either way.

It also reports three things that are true of the machine rather than of home, because each of them describes a machine that is not doing what "no changes" implies: a paused cascade, since that machine can commit nothing until it is resolved; scopes whose head this machine and the remote have each moved, since the next writing run merges them; and scopes committed here that the remote has never seen, since a refused push is otherwise reported by the run that hit it and nowhere else. `diff` and `view` report all three too — `status` is the one an agent runs by reflex, and a fact that only one of the three carries is a fact nobody finds.

**`dotsync diff`**: Show line-oriented diffs for managed home files with local changes. Read-only, and exits 1 when local changes are present so scripts and agents can distinguish clean from dirty state. A file the repo has moved on from while home stayed put is not a local change, so a machine that is merely behind exits 0 — the same answer `status` and plain `dotsync` give.

**`dotsync discard <path>...`**: Throw away the local change at each path you name and write the scope's version there instead, then sync as usual. Every path has to be one of the changes `status` lists; anything else is a stop.

**`dotsync view`**: Show a read-only overview of checked-in scope and file state, marking which scope is this machine. With `--scope <scope>`, show the managed file tree visible on that scope. With `--file <path>`, show the scopes where that file exists and which one owns it — the rootmost, since the rest have it from the cascade. With both, print that file as it exists on that scope.

While a merge is waiting, the overview also reprints it in full: every conflicted file, both sides and the base. That is the only copy there is — nothing is written into home and neither side is on a scope this machine syncs from — so an agent whose session ended has somewhere to ask.

**`dotsync continue`**: Continue a paused cascade once the conflict has been resolved, recording the resolved contents on the scope whose merge stopped and publishing everything the pause held back. Refuses a resolution that still holds conflict markers.

**`dotsync abort`**: Discard what the run that stopped committed, restoring every scope it moved to where that run found it, and sync the current machine home back to the restored state. Says so, and stops, if the merge is still waiting afterwards — which is what a conflict that came from the remote does.

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | The command did what it says. |
| 1 | It did not, or `dotsync diff` found changes. |

Two, because what kind of stop it was is a question with more than two answers and the payload is where it is answered: `error` names the kind, including `cascade_paused` for the one state with a remedy of its own. `status` separates the two meanings of 1 — `"error"` for a stop, `"ok"` for the changes `diff` found, which is the one non-zero exit that is not a stop. Max: _"I frankly don't really care about exit codes."_

### Why one command?

Earlier designs had separate `dotsync` (sync), `dotsync commit` (commit + cascade), and `dotsync push` (push to remote). But these are always done together — there's no useful state where you've committed but not cascaded, or cascaded but not synced. Collapsing them into one command means fewer steps to forget, and agents only need to know one invocation.

## Agent skill

dotsync ships an agent skill (`docs/SKILL.md`) that triggers whenever a home
config file is edited: edit in `~/`, run `dotsync status`, choose the root-est
scope that owns the change, commit it. The tool is plumbing; the skill is what
makes agents use the plumbing correctly, and it is the reason the command
surface stays small enough to describe in a page.

What a scope is for is ordinary commentary, useful when the name is not
self-evident and carrying nothing when it is (Max: _"I don't think the scope
comments are 'load bearing' lol? they're pretty obvious"_). A scope called
`hyprland` says what belongs on it by being called `hyprland`. So whoever
creates a scope may say what it is for, in the creation commit, and `dotsync
view` shows it — one sentence that cannot drift from the scope it describes.

## The JSON contract (`--output json`)

Every command prints one JSON object on stdout. Notes go to stderr in every
format — what a run overwrote, what it could not publish, what it carried — but
the *headline* does not: `dotsync --output json` writes nothing to stderr where
the same run in human mode says `synced 2 file(s) for mx-xps-cy`.

The envelope is two fields. `status` is `"ok"` or `"error"`, and `command`
names the command that answered. Read `status` first: it is what separates
`dotsync diff`'s exit 1 (changes found, `"ok"`) from a stop (`"error"`). Any
command that could not reach the remote also carries `remote_unreachable` with
git's own words, meaning the payload describes the last state this machine
fetched.

Every payload from a read-only command carries three facts about the machine
rather than about the question asked — `diverged_scopes`, `unpushed_scopes` and
`machine_scope`, plus `paused_cascade` when a merge is waiting — because a fact
carried by two of the three commands is a fact nobody finds.

**The commands that move home** — `dotsync`, `init`, `continue`, `abort`,
`discard` — answer in one shape:

```json
{"carried_changes":[],"command":"discard","machine_scope":"goof-b","overwritten_files":[".apprc"],"status":"ok","synced_files":[".apprc"],"unpushed_scopes":[]}
```

`overwritten_files` is home content this run discarded in favour of the repo,
and `carried_changes` is home content it merged around and left standing — the
two halves of what a sync did to your edits. `unpushed_scopes` lists scopes
committed here that the remote does not have. `abort` adds `paused_scope`,
where the run that stopped had stopped.

**`commit`** says which of its two outcomes it had, because they are different
events: one wrote history and synced home, the other did neither and so has no
`synced_files` or `newly_tracked` at all rather than empty ones.

```json
{"command":"commit","machine_scope":"goof-a","newly_tracked":[".apprc"],"outcome":"committed","scope":"all","skipped_paths":[],"status":"ok","synced_files":[".apprc"],"unpushed_scopes":[]}
```

`newly_tracked` is what this commit put on the scope for the first time — every
machine sharing it will have those written into its home directory.
`skipped_paths` is what a named directory matched and the commit left alone,
each as `{path, state, reason}`.

**`status` and `diff`** are the same answer, and `diff` is `status`'s `changes`
with a diff attached to each:

```json
{"changes":[{"path":".apprc","reason":"edited here since the last sync","state":"modified"}],"command":"status","diverged_scopes":[],"incoming":[],"machine_scope":"goof-b","status":"ok","unpushed_scopes":[]}
```

`status` adds `incoming`, the files another machine changed that home has not
caught up to. Neither carries a count; the arrays have lengths.

**`view`** answers in four shapes, one per question asked: `{scopes, files}`
for the overview, `{scope, files}`, `{file, scopes, owner}`, and `{scope, path,
contents}`. `owner` is the rootmost scope holding the file, which is the one it
was committed to. At a pause the overview also carries `conflicts`, the same
objects the stop printed. This is a known sharp edge: the shapes are coherent
with each other only in the envelope, `scopes` changes type between two of
them, and `contents` is UTF-8 lossy.

**A stop** carries the kind, the message, the facts found, and the conflicted
files:

```json
{"conflicts":[],"current_state":["`nope.conf` matched nothing: no file exists at or under /home/you/nope.conf, and scope `all` tracks no file at or under `nope.conf`."],"error":"unusable_commit_paths","message":"cannot commit the path you named","status":"error"}
```

`current_state` is a list of facts, one per thing the run found, so a caller
never has to split a rendering apart on newlines. `conflicts` carries every
version of every file a merge could not resolve: `{path, versions:[{role,
label, contents}]}`, base first. Both are always present, so error handling has
one shape — including for `error: "usage"`, which is what a command line dotsync
could not parse answers with.

`error` names the kind. The one worth branching on is `cascade_paused`: a merge
is waiting, and the remedy is to resolve it and run `dotsync continue`, or to
run `dotsync abort`. `paused_cascade` names the scope beside it, under the name
the read-only commands use.

## What dotsync is NOT

- **Not a package manager.** Package lists can be tracked as files in the repo, but dotsync doesn't install anything.
- **Not a secret manager.** Don't put secrets in the repo. The repo is private but treat it as public.
- **Not a system config manager.** Files outside `~/` are out of scope. System-level config (like `/etc/systemd/logind.conf`) is tracked in notes but managed manually.
- **Not a bootstrapper.** Setting up a fresh machine (installing dotsync and git, running `dotsync init` the first time) is a manual process. dotsync is for steady-state maintenance.
