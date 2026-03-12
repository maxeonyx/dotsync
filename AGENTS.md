# dotsync - Agent Instructions

This file guides AI agents working on the dotsync codebase itself.

## Start Here

- Read `DESIGN.md` before changing command behavior, scope semantics, sync rules, or any product requirement.
- Read `README.md` when updating public-facing positioning, quick-start content, or outbound links.
- Read `docs/SKILL.md` only when editing the end-user dotfiles workflow skill that agents load while changing config files.

## Project Overview

`dotsync` is a Rust CLI that wraps `jj` (Jujutsu) workflows for dotfile synchronization using scope branches and merge cascades.
Current code is scaffolding only; product logic is intentionally not implemented yet.

`jj` (Jujutsu) is a runtime dependency for the final product behavior.
It may not be installed in every dev environment yet.

## Scope Model

- Scopes form a DAG of branches (for example `all -> linux -> hyprland -> machine`), and machine scopes are leaf scopes.
- The full model, rationale, and command contract live in `DESIGN.md`; treat it as the source of truth.

## Agent Docs Boundaries

- `AGENTS.md` is developer guidance for contributors working on the dotsync codebase itself.
- `docs/SKILL.md` is the end-user dotfiles skill for agents editing `~/dotfiles/` on managed machines.

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

- `DESIGN.md`: read when implementation choices might affect requirements or workflow semantics
- `src/main.rs`: read when modifying CLI parsing, command shapes, or startup behavior
- `.github/workflows/ci.yml`: read when changing test/lint/build expectations in CI
- `.github/workflows/release.yml`: read when changing release packaging or tag-triggered publishing
- `.github/workflows/pages.yml`: read when changing how `docs/` is deployed to Pages
- `docs/index.html`: read when updating the public landing page content or style
- `docs/SKILL.md`: read when refining end-user agent instructions for dotfiles edits

## CI and Release

- CI runs format, lint, check, and tests on Linux/macOS/Windows for pushes and PRs to `main`.
- Release runs on `v*` tags and publishes binaries as GitHub Release assets.
- Pages deploys static content from `docs/` via GitHub Actions.
