# Skill: dotfiles

Use this skill when editing dotfiles on a machine managed by dotsync.

## Workflow

1. Run `dotsync` first to pick up anything other machines have published.
2. Edit config files directly at `~/` (their real locations).
3. Run `dotsync status` to see what changed.
4. Run `dotsync commit <scope> -m "message" -- <paths>` to commit specific files, or `dotsync commit <scope> -m "message"` to commit all changed managed files.
5. Choose the root-est appropriate scope for the change (the highest scope that still semantically owns the edit).
6. To discover available scopes, read `.config/dotsync/config.toml` from the `all` scope — its comments explain what each scope is for.

## Choosing a scope

- `all`: config that applies to every machine (e.g. `.gitconfig`, universal shell aliases)
- OS scopes (e.g. `linux`, `windows`): config specific to an OS
- Environment scopes (e.g. `hyprland`): config specific to a desktop environment or tool stack
- Machine scopes (e.g. `mx-xps-cy`): config specific to one machine only

Always choose the **highest (most general) scope** that makes sense. If a change applies to all linux machines, use `linux`, not the machine scope.

## Notes

- dotsync is repo-first: the repo is the source of truth.
- After committing, dotsync cascades the change through all descendant scopes and syncs the result back to `~/`.
- `dotsync status` separates two things. Files it lists as **changed** were changed here and need a decision from you. Files it lists as **incoming** were changed on another machine and home has not caught up — plain `dotsync` applies those, and `dotsync commit` refuses them, because committing one would revert whoever published it.
- If live system files have drifted from what the repo expects, `dotsync` (sync) will show the diff and stop. Inspect the diff before re-running with `--force`.
- `--force` has two shapes. On plain `dotsync` and `continue` it overwrites every drifted file. On `commit` it applies only to the paths you name, and the run reports them as `forced_overwrites`.
- There is no `~/dotfiles/` directory. The repo is hidden at `~/.local/share/dotsync/repo/`. Never interact with it directly.
- `dotsync --output json <command>` gives structured output for programmatic use.
