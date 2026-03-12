# Skill: dotfiles

Use this skill when editing dotfiles on a machine managed by dotsync.

## Workflow

1. Edit files in `~/dotfiles/`, not in `~/`.
2. Read `~/dotfiles/.config/dotsync/config.toml` first to discover available scopes; its comments explain what each scope is for.
3. Choose the root-est appropriate scope for the change (the highest scope that still semantically owns the edit).
4. Run `dotsync <scope> -m "message"` after edits are complete.
5. Run plain `dotsync` only when there are no local repo edits and you only want repo-to-system sync.

## Notes

- dotsync is repo-first: the repo is the source of truth.
- The repo mirrors `~/`: paths map directly (for example `~/dotfiles/.config/fish/config.fish` maps to `~/.config/fish/config.fish`).
- If live system files drift, inspect the diff before forcing sync.
