# dotsync - Agent Instructions

This file guides AI agents working on the dotsync codebase itself.

## Start Here

- Read `DESIGN.md` when you need the product intent, command contract, and scope model.
- Read `README.md` when you need public-facing project context and links.
- Read `docs/SKILL.md` when working on the end-user OpenCode skill for dotfiles workflows.

## Project Overview

`dotsync` is a Rust CLI for dotfile synchronization using scope branches and merge cascades.
Current code is scaffolding only; product logic is intentionally not implemented yet.

`jj` (Jujutsu) is a runtime dependency for the final product behavior.
It may not be installed in every dev environment yet.

## Development Commands

```bash
# Build
cargo build

# Test
cargo test

# Lint and formatting checks
cargo fmt --check
cargo clippy -- -D warnings
```

## Architecture Notes

- Keep side effects at the edges (filesystem, subprocesses, VCS operations).
- Prefer explicit failures over silent fallbacks.
- Keep docs and implementation aligned with `DESIGN.md`.

## Key Files

- `DESIGN.md`: full design story and requirements; read before implementing behavior
- `src/main.rs`: CLI entry point (currently stub)
- `.github/workflows/ci.yml`: CI checks on pushes and pull requests
- `.github/workflows/release.yml`: tag-triggered cross-platform release assets
- `.github/workflows/pages.yml`: Pages deployment from `docs/`
- `docs/index.html`: website landing page
- `docs/SKILL.md`: skeleton dotfiles workflow skill

## CI and Release

- CI runs format, lint, check, and tests on Linux/macOS/Windows for pushes and PRs to `main`.
- Release runs on `v*` tags and publishes binaries as GitHub Release assets.
- Pages deploys static content from `docs/` via GitHub Actions.
