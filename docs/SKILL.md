# Skill: dotfiles

Use this skill when editing dotfiles on a machine managed by dotsync.

## Workflow

1. Run `dotsync` first to pick up anything other machines have published.
2. Edit config files directly at `~/` (their real locations).
3. Run `dotsync status` to see what changed.
4. Run `dotsync commit <scope> -m "message" -- <paths>` to commit specific files, or `dotsync commit <scope> -m "message"` to commit all changed managed files.
5. Choose the root-est appropriate scope for the change (the highest scope that still semantically owns the edit).
6. To discover available scopes, read `~/.config/dotsync/config.toml` — its comments explain what each scope covers, and what belongs on it if anyone has written that down yet. When you learn something about a scope that the file does not say, write it under that scope's `What belongs here:` line and commit the file to `all`; that is how the next agent finds out.

## Choosing a scope

- `all`: config that applies to every machine (e.g. `.gitconfig`, universal shell aliases)
- OS scopes (e.g. `linux`, `windows`): config specific to an OS
- Environment scopes (e.g. `hyprland`): config specific to a desktop environment or tool stack
- Machine scopes (e.g. `mx-xps-cy`): config specific to one machine only

Always choose the **highest (most general) scope** that makes sense. If a change applies to all linux machines, use `linux`, not the machine scope.

## When a cascade pauses

A commit merges the change through every descendant scope. Where two scopes changed the same file differently, the run stops with **exit code 3** and names the conflicted files. That is not a failure to retry: it is a question. Edit the named files in `~/` to the merged contents you want — the file has to change, because dotsync reads the resolution back out of it — then run `dotsync continue`. Run `dotsync abort` instead to discard the whole cascade, including the home edit that started it. Until you do one of those, `dotsync commit` refuses to start another cascade — also with exit 3 — and nothing this machine has committed is published. `dotsync status` and `dotsync diff` say so whenever a cascade is paused, so that is where to look if you have lost the original message.

## Exit codes

- `0` — the command did what it says.
- `1` — dotsync stopped, or `dotsync diff` found changes. Under `--output json`, `status` is `"error"` for a stop and `"ok"` for the changes `diff` found.
- `2` — the command line was wrong.
- `3` — a paused cascade is waiting.

## Notes

- dotsync is repo-first: the repo is the source of truth.
- After committing, dotsync cascades the change through all descendant scopes and syncs the result back to `~/`.
- `dotsync status` separates two things. Files it lists as **changed** were changed here and need a decision from you. Files it lists as **incoming** were changed on another machine and home has not caught up — plain `dotsync` applies those, and `dotsync commit` refuses one you name, because committing it would revert whoever published it.
- Naming a directory (`-- .config/fish/`) records what this machine changed under it, adds what is new under it, and steps around what another machine changed — listing what it left alone. Naming no paths at all records only changes to files dotsync already tracks; it never adds a new file, so a new file has to be opted into by naming it or the directory it is in. Only a path you name exactly is refused. Naming your whole home directory (`.`) is refused outright — name the directories you mean.
- `dotsync diff` is `dotsync status`'s changed list with the diffs shown; it exits 1 when it finds any. `dotsync view` shows what is checked in: `--scope <scope>` for a scope's files, `--file <path>` for the scopes holding a file, both for that file's contents on that scope.
- Symlinks cannot be committed. Naming the link itself is refused; a link found under a directory you named is reported in `skipped_paths` and left out of the commit. Dotsync records the content at the path you name and writes it back to that path on every machine sharing the scope, and it has no answer yet for what that should mean when the path is a link. Config kept outside home and linked into place has to be moved into home to be managed.
- A commit reports the files it put on a scope for the first time (`newly_tracked`). Every machine sharing that scope gets them written into its home directory, so it is worth reading that line.
- When the remote cannot be reached, every command still works against the state this machine last fetched and says so; commits stay local until a run that reaches the remote publishes them. Only `dotsync init` needs the remote to be up.
- If live system files have drifted from what the repo expects, `dotsync` (sync) will show the diff and stop. Inspect the diff before re-running with `--force`.
- `--force` has two shapes. On plain `dotsync` and `continue` it overwrites every drifted file. On `commit` it applies only to the paths you name, and the run reports them as `forced_overwrites`.
- There is no `~/dotfiles/` directory. The repo is hidden at `~/.local/share/dotsync/repo/`. Never interact with it directly.
- `dotsync --output json <command>` gives structured output for programmatic use.
