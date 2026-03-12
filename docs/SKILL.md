# Skill: dotfiles

Use this skill when editing dotfiles on a machine managed by dotsync.

## Workflow

1. Edit files in `~/dotfiles/`, not in `~/`.
2. Read `~/dotfiles/.config/dotsync/config.toml` to discover available scopes.
3. Choose the most root-appropriate scope for the change.
4. Run `dotsync <scope> -m "message"` after edits are complete.

## Notes

- dotsync is repo-first: the repo is the source of truth.
- If live system files drift, inspect the diff before forcing sync.
