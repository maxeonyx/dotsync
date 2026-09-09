# dotsync — Plan

`DESIGN.md` describes dotsync as it is. This file is for what is not built: what is ahead, what is deliberately deferred, and the standing constraints any of it is done under.

## Where things stand

- The next-gen rewrite is done. Home is jj's working copy, the scope graph is derived from the repo's own structure, one convergence pass moves every scope bookmark, "paused" is recomputed rather than stored, and each error carries its own explanation. Two files exist per machine: the home files, and the hidden repo — plus one record of a run that stopped, which is a fact about that run rather than about the repo.
- Development happens in the agent-tools workspace (`tools/dotsync`). For the current version, run `dotsync --version` or check the releases; don't record it here, it rots.
- Live fleet: remote `git@github.com:maxeonyx/dotfiles.git`, twelve scopes, `all` → `home`/`work`/`linux`/`windows` → intersections → machines (`mc-wsl-fd`, `mx-vps-fd`, `mx-xps-cy`, `maxeonyx-pc-windows`).
- **mc-wsl-fd is the only machine upgraded.** `mx-xps-cy`, `mx-vps-fd` and `maxeonyx-pc-windows` are on ≤0.4.6 and every command on them exits 1 with `config missing on all scope` until their binary is upgraded, because `config.toml` was deleted from the `all` scope on 2026-09-08. That is Max's own decision rather than an open question: upgrade the binary, then run `dotsync`.
- One test is red on purpose: `read_only_commands_leave_the_scope_bookmarks_where_they_found_them`. What moves the bookmark is jj's import of what the fetch brought, which is the fast-forward case of convergence — the case that creates nothing. Meeting it needs a run that answers from a fetched view it never records, which is a change to how a run holds the repo. Whoever wants it should take it as its own piece.

## Standing constraints

- **Prioritise conceptual simplification and the removal of bad modelling.** When a change supersedes a mechanism, delete the old mechanism in the same change rather than layering. Anything robustly wrong or unnecessary is in scope to remove.
- **The cull applies to the product surface, not just internals** (Max, 2026-08-12): _"not just gurky internals, but sharp product edges too. Remember - me, and mainly agents with no memory, are the only users. So self-documentation is critical and backwards compatibility (on the frontend, not the backend though (ie. the repo / scope model)) is an antipattern on this project."_ Redesign a sharp flag, message or JSON field outright. The repo/scope model is where compatibility matters.
- **Back-chain smells to root causes.** Don't fix a smell where it presents; trace it to the modelling decision that produced it and fix that, accepting large refactorings.
- **Never hand-mutate the hidden repo or the dotfiles history.** If a change would need to rewrite the live dotfiles history, find another way or stop and tell Max.
- **Make invalid states unrepresentable. Make bugs impossible by construction. Relentlessly identify and destroy incidental complexity** — while keeping enough essential complexity (Max).

## Ahead

### Agent validation loop

Use the headless agent-scenario infrastructure (`tests/agent-scenarios/`) with a cheap model to validate the full UX: make a config change, run dotsync, resolve a conflict from what the stop printed, done. Add scenarios starting from an interrupted-push state and a conflicted-head state — the agent must recover using dotsync alone.

This is the actual product bar: the tool has failed in practice precisely when real agents met unplanned states. It is also what settles the one open presentation question. Conflicts are presented in dotsync's own output and never written into home, because markers in a live config file are broken config for as long as the pause lasts (Max, 2026-08-19, "the overwhelming preference") — but "a real agent can reliably resolve one from what it is shown" is the bar that presentation has to clear, and only watching one do it says whether it does. If it does not, the fallback is materialising markers through ordinary sync, and `continue` would delete with it, because "the markers are gone" is then legible from the file.

### Scope graph changes beyond creation

Rename, reparent and delete. Creation is the whole of the mutation surface today, so a scope created now is only for the machines that join under it afterwards — an existing machine cannot be moved onto one. Imperative operations first, validated empirically (Max, 2026-08-19): _"can an agent perform a correct sequence of imperative actions to modify the scope graph? And can dotsync manage scope graph changes across multiple machines without the reconciler? Does convergence need to handle scope graph changes?"_ Only after living with the answers: whether a declarative `config.toml` with a reconciler is worth bringing back at all.

### Windows

- **What a scope may hold is constrained by what its machines can represent** ([#28](https://github.com/maxeonyx/dotsync/issues/28)). Symlinks, executable bits, non-UTF-8 text: the constraint is the union over every leaf a scope reaches, which makes `all` the _most_ constrained scope rather than the least. The issue holds the model and the one open question — whether the constraint comes from machine leaves that exist, or from the OS scopes in the graph.
- **Path separators are unconfirmed on Windows.** The directory walk builds relative paths with `read_dir` separators and feeds `from_internal_string`, which would record `.config\fish\config.fish` as a tree entry name; `render::display_path` is `Path::display()`, so JSON would carry backslashes in some fields and forward slashes in others; and `canonicalize` returns the on-disk casing, so on a case-insensitive filesystem `dotsync commit all -- .APPRC` may look like a link to somewhere else and be refused. All inferred from source. Needs one Windows run.

### Smaller, unowned

- **`view` answers in four JSON shapes under one command name.** They are coherent only in the envelope, `scopes` changes type between two of them, and file contents are UTF-8 lossy. Whoever takes it should decide whether these are one command at all.
- **A conflict's side labels name the scope whose head each input is, not the scope the change was made on.** A change committed to `all` is presented as `` side: scope `linux` `` when `linux`'s head is the commit that came down from `all`. Correct and confusing at once.
- **Convergence merge commit ids are not reproducible, and could be.** `machine_signature` stamps `Timestamp::now()`, so predicting the pass twice writes two objects for the same merge. Deriving a convergence merge's timestamp from its inputs would make it byte-identical on every machine that computes it — which would also mean two machines converging the same scopes independently never diverge.
- **Auto-update** — Max, 2026-08-19: he wouldn't mind implementing it at some point. Workspace-level, since every tool would want it.

### Backlog triage

- PR [#15](https://github.com/maxeonyx/dotsync/pull/15) (scope lifecycle / add-scope): predates the rewrite. Review against current `main`, land or close.
- Issues [#4](https://github.com/maxeonyx/dotsync/issues/4), [#5](https://github.com/maxeonyx/dotsync/issues/5), [#8](https://github.com/maxeonyx/dotsync/issues/8), [#10](https://github.com/maxeonyx/dotsync/issues/10), [#11](https://github.com/maxeonyx/dotsync/issues/11), [#18](https://github.com/maxeonyx/dotsync/issues/18): all written before the rewrite and several are fixed by it. Re-triage against what ships today, then close or trim.
