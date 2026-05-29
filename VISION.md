# dotsync - Vision

`dotsync` is an agent-first dotfile sync tool for people whose config lives across multiple machines and overlapping scopes.

## Problem

Dotfiles drift. Universal config, OS-specific config, and machine-specific config all need to live in one source-of-truth repo without forcing users or agents through templates, symlink farms, or manual cherry-picking.

## Users

- People managing dotfiles across multiple machines
- AI agents editing config as a primary workflow

## Goals

- Keep the repo as the single source of truth for synced config
- Model variation with plain files and scope branches rather than template logic
- Make contributing and propagating config changes simple enough for agents to do reliably

## Non-goals

- Bidirectional sync from home back into the repo
- Embedding scope logic inside config files with templates
- Requiring users or agents to maintain path-mapping or whitelist config

Detailed design and rationale live in [DESIGN.md](DESIGN.md).
