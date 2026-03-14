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

## Key Files

- `DESIGN.md`: read when implementation choices might affect requirements or workflow semantics
- `src/main.rs`: read when modifying CLI parsing, command shapes, or startup behavior
- `.github/workflows/ci.yml`: read when changing CI, release, or Pages deployment
- `docs/index.html`: read when updating the public landing page content or style
- `docs/SKILL.md`: read when refining end-user agent instructions for dotfiles edits

## CI and Release

Single `ci.yml` workflow: format, lint, check, test, build matrix (6 targets), GitHub Release (version from Cargo.toml, no tags), Pages deploy (docs + binaries combined at dotsync.maxeonyx.com).
