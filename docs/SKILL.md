# Skill: dotfiles

Use this skill when editing dotfiles on a machine managed by dotsync.

Everything you need to know about the dotfiles — what any machine has, where a version comes from, how two machines differ — and every change to where config lives is a dotsync command. The repo behind it is hidden at `~/.local/share/dotsync/repo/`; never read or change it directly, and never clone the dotfiles remote to look at it.

## The model in one paragraph

Config lives on **scopes**, and scopes inherit from the scopes above them: `all` → `linux` → … → this machine's own scope at the bottom. A scope can **add** a file, **override** one it inherits with its own version, or **remove** one; anything else it holds it **inherited**. Every scope can be read from every machine.

## Workflow

1. Run `dotsync` first to pick up anything other machines have published.
2. Edit config files directly at `~/` (their real locations).
3. Run `dotsync status` to see what changed. A file dotsync does not track yet will not be there: it lists changes to managed files, and a brand-new one is not a change to anything. Do not read "no changes" as "nothing to commit".
4. Run `dotsync commit <scope> -m "message" -- <paths>` to commit specific files, or `dotsync commit <scope> -m "message"` to commit every changed managed file. A new file has to be named — naming no paths never starts tracking one.

## Choosing a scope

New config starts on **this machine's own scope**. It moves to a shared scope when a second machine wants it — see "Sharing config" below — because config that only one machine has tried belongs to that machine.

Config that is plainly shared already (a change to a file that `all` or `linux` owns) is committed where it lives. `dotsync files <path>` says where that is.

A commit from home can only target this machine's own scope or one above it: home was built from those. Config reaches any other scope through `move` and `drop`, whose content comes from the repo.

`dotsync scopes` shows the scopes, where each hangs, which one is this machine, and which machines each one reaches.

There may be no scope for what you are holding — config shared by some machines and not others, with nothing in the graph that means "those machines". `dotsync create-scope <name> --parent <scope> -m "what belongs here"` makes one. It only carries config to machines that join under it with `dotsync init --parent <name>`, though: nothing moves an existing machine onto a new scope yet. Prefer an existing scope.

## Reading the fleet

- `dotsync files --own` — what every scope holds of its own: added (`A`), overridden (`M`), removed (`D`). Identical copies on two scopes say `same as …`. This is the audit: what is local to each machine and what could be shared.
- `dotsync files <path>` — every scope's standing at a path, and for an inherited version the scope it comes from.
- `dotsync files --scope <scope>` — everything one scope holds.
- `dotsync show <scope> <path>` — a file as one scope holds it. Works for any scope, including other machines'.
- `dotsync diff <scope>` — what a scope changes over what it inherits. `dotsync diff <scope> <scope>` — how two scopes differ. Add `-- <path>` to narrow either.

Add `--output json` to any of them for rows to process; `content` is equal exactly when two versions are identical.

## Sharing config

When this machine wants config another machine has:

1. `dotsync files --own` to find it, `dotsync show <other-machine> <path>` to read it.
2. If it works here as it is: `dotsync move <path> --from <other-machine> --to <shared-scope> -m "why"`. The shared scope is the nearest one both machines are under (`dotsync scopes`). The other machine's config does not change; this machine gains the file.
3. If it needs changing to work here: write your version into `~/<path>`, `dotsync commit <this-machine> -m "why" -- <path>`, make sure it works, then `dotsync move <path> --from <this-machine> --to <shared-scope> -m "why"`. The other machine keeps its own version until something says otherwise — `dotsync drop <path> --from <other-machine> -m "why"` makes it take the shared one.

Before any write that reaches other machines, add `--dry-run`: it lists, machine by machine, what would change, and changes nothing. Every real write lists the same thing afterwards. Tell the user which machines receive config nobody has tried on them, and what might break there (missing binaries, paths, the platform).

## Moving and removing config

- **Too broad**: `dotsync move <path> --from linux --to work-linux -m "why"` narrows config to where it belongs. Machines under `work-linux` keep it unchanged; the others lose it.
- **Stop overriding**: `dotsync drop <path> --from <scope> -m "why"` makes the scope take what it inherits again.
- **Remove everywhere**: `dotsync drop <path> --from <owner> -m "why"`, where the owner is the scope `dotsync files <path>` says the version comes from.

`move` and `drop` work on any scope from any machine, and leave every other scope's own version alone: they never land a conflict on another machine's scope.

## Ending a local change

A file you edited in `~/` stays yours until you do one of two things with it. `dotsync commit` makes it everybody's. `dotsync discard <paths>` throws it away and writes the scope's version back into home. Deleting the file yourself is neither — a deletion is a local change too, so home comes back empty rather than canonical.

Every path `discard` takes has to be one of the changes `dotsync status` lists. Anything else is a stop, because discarding cannot be undone and a mistyped path is likelier than a change of mind.

## When a merge is waiting

A commit merges the change through every descendant scope, and a sync merges what other machines published into what this one holds. Where two sides changed the same file differently, the run stops and prints every version of every file it could not merge: the one both sides started from, and each side, labelled with where it came from. Nothing is written into the file itself, so the config it holds stays valid while you work on it.

That is not a failure to retry. It is a question. Decide what each file should hold, write that into it at its real path in `~/`, and run `dotsync continue`. Dotsync reads the resolution back out of the file, so leaving one exactly as it is says you decided on the version already there. `continue` refuses a file that still holds conflict markers, since those would cascade to every other machine's live config.

`dotsync abort` is the other way out: it discards what this machine committed, including the home edit that started it. Note what it cannot do — when the change you collided with came from another machine, there is nothing of yours in the way, so home goes back and the merge is still waiting. Resolving is the only way through that one.

Until you do one of those, writes refuse to start and nothing this machine has committed is published. `dotsync status` says so, and prints the whole conflict again — the place to go if the original message has scrolled away, since those versions exist nowhere else.

## Setting up a new machine

`dotsync init <remote-url>` clones the fleet. If the fleet already has scopes it stops there, so you can look before choosing where this machine hangs: `dotsync scopes`, `dotsync files --scope <candidate>`, `dotsync show <candidate> <path>`. Then `dotsync init --parent <scope>` joins.

Files already in home that the fleet holds differently are never overwritten: the first sync stops and shows both versions. Keep this machine's with `dotsync continue` (it stays a local change to commit or discard), or take the fleet's with `dotsync discard <path>`. No backup is needed.

## Exit codes

- `0` — the command did what it says.
- `1` — it did not, or `dotsync diff` found changes or differences.

Under `--output json`, `status` is `"error"` for a stop and `"ok"` for differences `diff` found, and `error` names the kind of stop — `cascade_paused` for a merge waiting on you, `usage` for a command line dotsync could not parse.

## Notes

- dotsync is repo-first: the repo is the source of truth.
- `dotsync status` separates two things. Files it lists as **changed** were changed here and need a decision from you. Files it lists as **incoming** were changed on another machine and home has not caught up — plain `dotsync` applies those, and `dotsync commit` refuses one you name, because committing it would revert whoever published it.
- Naming a directory (`-- .config/fish/`) records what this machine changed under it, adds what is new under it, and steps around what another machine changed — listing what it left alone. Naming no paths at all records only changes to files dotsync already tracks; it never adds a new file, so a new file has to be opted into by naming it or the directory it is in. Only a path you name exactly is refused. Naming your whole home directory (`.`) is refused outright — name the directories you mean.
- Dotsync records what it finds at the path you name, kind and all: an executable script stays executable on every machine, and a symlink is recorded as a symlink whose content is its target. A link is never followed, so naming a link to a directory records one link rather than everything under it. What is refused is a path that reaches its file *through* a link (`.config/nvim/init.lua` where `.config/nvim` is a link): what dotsync would read is not what you named, and the other machines have no such link. Config kept outside home and linked into place is committable as the link, and the file it points at is not managed.
- A commit reports the files it put on a scope for the first time (`newly_tracked`). Every machine sharing that scope gets them written into its home directory, so it is worth reading that line.
- A sync merges rather than gates. A file you edited that nothing else changed is carried across and reported as `carried_changes` — it stays yours to commit, and you do not have to deal with it before receiving anything else. Only a file that home and the scope both changed stops the run, and then the run stops whole: nothing is written, not even the incoming changes to other files, because home is derived from one commit.
- When the remote cannot be reached, every command still works against the state this machine last fetched and says so; commits stay local until a run that reaches the remote publishes them. Only `dotsync init` needs the remote to be up.
