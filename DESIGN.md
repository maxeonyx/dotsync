# dotsync — Design Story

## The problem

You have config files scattered across `~/` on multiple machines. Some config is universal (`.gitconfig`), some is OS-specific (hyprland on linux), some is machine-specific (wallpaper paths). You want a single repo that is the source of truth for all of it, and you want AI agents to be able to maintain it without special knowledge.

Manual dotfile management breaks down along several axes:

1. **Syncing** — copying files between a repo and the live system is tedious and error-prone. People forget, things drift, and nobody notices until something breaks on a fresh machine.

2. **Multiple versions** — the same *kind* of config (e.g. "shell config") might differ between linux and windows, or between your laptop and your server. Most dotfile tools either ignore this (one branch, one machine) or punt to symlink farms with conditionals baked into the files themselves.

3. **Contributing changes** — when you edit a config file, getting that change into the right place in the repo should be frictionless. If it's not, you stop doing it, and the repo rots.

4. **Agent usability** — AI agents edit config files. If the dotfiles system has a complex mental model, agents will get it wrong. The system must be simple enough that a skill description can fully explain the workflow.

## Why not existing tools?

Most dotfile managers (stow, chezmoi, yadm, bare git repo) solve problem 1 and partially solve problem 2. None of them solve problems 3 or 4 well. The specific gaps:

- **Bare git repo with `$HOME` as worktree**: elegant but terrifying. Every `git clean` or careless `git checkout` can nuke your home directory. `git status` shows thousands of untracked files. Agents would need to be told "never run git clean" — a footgun.

- **Symlink managers (stow, etc.)**: solve syncing well but don't handle multiple versions. You end up with `hyprland.conf.linux` and `hyprland.conf.arch` and a script that picks the right one. The indirection makes it hard for agents to know which file to edit.

- **Chezmoi**: powerful, handles templates and secrets, but has a complex mental model (source state vs target state, template syntax, `.chezmoiignore`). An agent would need extensive prompting to use it correctly.

- **Branching approaches**: some people use git branches for per-machine config. This works until you need to propagate a universal change to all machines — you're manually cherry-picking or rebasing across N branches, re-solving the same conflicts.

The gap in all of these: none of them make it easy to say "this change belongs to all linux machines" and have it automatically propagate to every machine that cares, with conflict resolutions preserved.

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

Each scope is a git branch. A scope branch merges from its parent(s). So `linux` merges from `all`, `hyprland` merges from `linux`, and `mx-xps-cy` (a machine) merges from `hyprland`.

A machine is just a leaf scope — there's nothing structurally special about it. The only difference is that a machine scope is the one whose files get synced to the live system. dotsync knows which scope is "current" by reading the checked-out branch name.

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

The repo mirrors `~/`. A file at `~/dotfiles/.config/fish/config.fish` syncs to `~/.config/fish/config.fish`. No path mapping, no translation layer. This is critical for agent usability — an agent told "edit the fish config" can find it at the obvious path without consulting any mapping config.

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

## Sync direction

Sync is always repo -> system. The repo is the single source of truth. We never read config files from `~/` into the repo.

If the system file differs from the repo, that's drift. dotsync warns and shows a diff. The user (or agent) decides whether to overwrite the system file or investigate.

### Why not bidirectional?

Bidirectional sync requires conflict resolution between the repo and the system, which is a fundamentally different (and harder) problem than git merge conflicts. It also makes the mental model ambiguous: "which is the source of truth?" With unidirectional sync, the answer is always "the repo."

The cost: to contribute a system change, you must first recreate it in the repo. In practice this is trivial — you edit the file in `~/dotfiles/` instead of `~/`. The agent skill enforces this.

### Drift detection without tracking state

An early design considered tracking "last synced commit hash" to distinguish "repo advanced" from "system drifted." But this is unnecessary complexity. The check is just: does the system file match repo HEAD? If yes, nothing to do. If no, show a diff. The diff itself tells you whether it's a repo change to apply or unexpected drift.

We briefly worried about the case where you edit a file in the repo, sync it, edit it again, and sync again — the second sync would see the system (matching the first edit) differs from the repo (now with the second edit). But this isn't a problem because jj auto-commits the working copy before comparison. The repo HEAD always reflects the latest intended state.

## The jj decision

dotsync uses [jj (Jujutsu)](https://github.com/jj-vcs/jj) rather than raw git. The key reason: **jj can manipulate branches without touching the working copy.**

When you contribute a change, it needs to be committed on the right scope branch — not necessarily the branch you have checked out. With git, this requires stashing, checking out the target branch, committing, checking out back, merging, and popping the stash. If you have multiple uncommitted changes going to different scopes, this becomes a nightmare of stash juggling.

With jj, you create a commit directly on the target scope's branch and rebase/merge descendant branches, all without disturbing the working copy (assuming no conflicts). The working copy stays on your machine's branch throughout.

jj is also git-compatible — the repo is a valid git repo, pushable to GitHub, cloneable with git. jj is just a better local interface for the graph manipulation dotsync needs.

## Commands

There is one command: `dotsync`.

**`dotsync`** (no arguments): Sync repo -> system. Errors if there are uncommitted local changes (because those changes need a scope to be committed to).

**`dotsync <scope> -m "message"`**: Commit the current changes to the named scope branch, merge cascade through all descendant scopes, sync repo -> system, push to remote.

The merge cascade is the key operation: after committing to a scope, every scope that has it as an ancestor (directly or transitively) gets the change merged in. If there are no conflicts, this happens silently. If there are conflicts, dotsync stops and reports them.

### Why one command?

Earlier designs had separate `dotsync` (sync), `dotsync commit` (commit + cascade), and `dotsync push` (push to remote). But these are always done together — there's no useful state where you've committed but not cascaded, or cascaded but not synced. Collapsing them into one command means fewer steps to forget, and agents only need to know one invocation.

## Agent skill

dotsync includes an agent skill (`dotfiles`) that triggers whenever any home directory config file is edited. The skill tells agents:

1. Edit files in `~/dotfiles/`, not `~/` directly
2. Choose the root-est appropriate scope for the change
3. Run `dotsync <scope> -m "description"` when done

This is the mechanism that makes the system agent-friendly. The tool itself is simple plumbing — the skill is what makes agents use the plumbing correctly.

## What dotsync is NOT

- **Not a package manager.** Package lists can be tracked as files in the repo, but dotsync doesn't install anything.
- **Not a secret manager.** Don't put secrets in the repo. The repo is private but treat it as public.
- **Not a system config manager.** Files outside `~/` are out of scope. System-level config (like `/etc/systemd/logind.conf`) is tracked in notes but managed manually.
- **Not a bootstrapper.** Setting up a fresh machine (installing jj, cloning the repo, running dotsync the first time) is a manual process. dotsync is for steady-state maintenance.
