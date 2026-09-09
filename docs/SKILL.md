# Skill: dotfiles

Use this skill when editing dotfiles on a machine managed by dotsync.

## Workflow

1. Run `dotsync` first to pick up anything other machines have published.
2. Edit config files directly at `~/` (their real locations).
3. Run `dotsync status` to see what changed. A file dotsync does not track yet will not be there: it lists changes to managed files, and a brand-new one is not a change to anything. Do not read "no changes" as "nothing to commit".
4. Run `dotsync commit <scope> -m "message" -- <paths>` to commit specific files, or `dotsync commit <scope> -m "message"` to commit every changed managed file. A new file has to be named — naming no paths never starts tracking one.
5. Choose the root-est appropriate scope for the change (the highest scope that still semantically owns the edit).
6. To see the scopes there are, run `dotsync view`. It lists each one with the scopes it inherits from, marks the one this machine is, and shows what a scope is for where whoever created it said.

## Choosing a scope

- `all`: config that applies to every machine (e.g. `.gitconfig`, universal shell aliases)
- OS scopes (e.g. `linux`, `windows`): config specific to an OS
- Environment scopes (e.g. `hyprland`): config specific to a desktop environment or tool stack
- Machine scopes (e.g. `mx-xps-cy`): config specific to one machine only

Always choose the **highest (most general) scope** that makes sense. If a change applies to all linux machines, use `linux`, not the machine scope.

The scope has to be one this machine is on — its own or one above it. Config for a machine family you are not on goes on the scope you share with it, together with the pattern an agent over there should follow when it adds that machine's own version; committing straight onto another machine's scope is refused, and the refusal names the scope to use instead.

There may be no scope for what you are holding — config shared by some machines and not others, with nothing in the graph that means "those machines". `dotsync create-scope <name> --parent <scope> -m "what belongs here"` makes one. It only carries config to machines that join under it with `dotsync init --parent <name>`, though: nothing moves an existing machine onto a new scope, so a scope created now is for machines set up later, and the first of those is what puts config on it. Prefer an existing scope.

## Ending a local change

A file you edited in `~/` stays yours until you do one of two things with it. `dotsync commit` makes it everybody's. `dotsync discard <paths>` throws it away and writes the scope's version back into home. Deleting the file yourself is neither — a deletion is a local change too, so home comes back empty rather than canonical.

Every path `discard` takes has to be one of the changes `dotsync status` lists. Anything else is a stop, because discarding cannot be undone and a mistyped path is likelier than a change of mind.

## When a merge is waiting

A commit merges the change through every descendant scope, and a sync merges what other machines published into what this one holds. Where two sides changed the same file differently, the run stops and prints every version of every file it could not merge: the one both sides started from, and each side, labelled with where it came from. Nothing is written into the file itself, so the config it holds stays valid while you work on it.

That is not a failure to retry. It is a question. Decide what each file should hold, write that into it at its real path in `~/`, and run `dotsync continue`. Dotsync reads the resolution back out of the file, so leaving one exactly as it is says you decided on the version already there. `continue` refuses a file that still holds conflict markers, since those would cascade to every other machine's live config.

`dotsync abort` is the other way out: it discards what this machine committed, including the home edit that started it. Note what it cannot do — when the change you collided with came from another machine, there is nothing of yours in the way, so home goes back and the merge is still waiting. Resolving is the only way through that one.

Until you do one of those, `dotsync commit` refuses to start another cascade and nothing this machine has committed is published. `dotsync status` and `dotsync diff` say so; `dotsync view` prints the whole conflict again, which is where to go if the original message has scrolled away — those versions exist nowhere else.

## Exit codes

- `0` — the command did what it says.
- `1` — it did not, or `dotsync diff` found changes.

Under `--output json`, `status` is `"error"` for a stop and `"ok"` for the changes `diff` found, and `error` names the kind of stop — `cascade_paused` for a merge waiting on you, `usage` for a command line dotsync could not parse.

## Notes

- dotsync is repo-first: the repo is the source of truth.
- After committing, dotsync cascades the change through all descendant scopes and syncs the result back to `~/`.
- `dotsync status` separates two things. Files it lists as **changed** were changed here and need a decision from you. Files it lists as **incoming** were changed on another machine and home has not caught up — plain `dotsync` applies those, and `dotsync commit` refuses one you name, because committing it would revert whoever published it.
- Naming a directory (`-- .config/fish/`) records what this machine changed under it, adds what is new under it, and steps around what another machine changed — listing what it left alone. Naming no paths at all records only changes to files dotsync already tracks; it never adds a new file, so a new file has to be opted into by naming it or the directory it is in. Only a path you name exactly is refused. Naming your whole home directory (`.`) is refused outright — name the directories you mean.
- `dotsync diff` is `dotsync status`'s changed list with the diffs shown; it exits 1 when it finds any. `dotsync view` shows what is checked in: `--scope <scope>` for a scope's files, `--file <path>` for the scopes holding a file and which one owns it, both for that file's contents on that scope.
- Dotsync records what it finds at the path you name, kind and all: an executable script stays executable on every machine, and a symlink is recorded as a symlink whose content is its target. A link is never followed, so naming a link to a directory records one link rather than everything under it. What is refused is a path that reaches its file *through* a link (`.config/nvim/init.lua` where `.config/nvim` is a link): what dotsync would read is not what you named, and the other machines have no such link. Config kept outside home and linked into place is committable as the link, and the file it points at is not managed.
- A commit reports the files it put on a scope for the first time (`newly_tracked`). Every machine sharing that scope gets them written into its home directory, so it is worth reading that line.
- A sync merges rather than gates. A file you edited that nothing else changed is carried across and reported as `carried_changes` — it stays yours to commit, and you do not have to deal with it before receiving anything else. Only a file that home and the scope both changed stops the run, and then the run stops whole: nothing is written, not even the incoming changes to other files, because home is derived from one commit.
- When the remote cannot be reached, every command still works against the state this machine last fetched and says so; commits stay local until a run that reaches the remote publishes them. Only `dotsync init` needs the remote to be up.
- There is no `~/dotfiles/` directory. The repo is hidden at `~/.local/share/dotsync/repo/`. Never interact with it directly.
- `dotsync --output json <command>` gives structured output for programmatic use.
