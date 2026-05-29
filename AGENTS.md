# dotsync - Agent Instructions

This file guides AI agents working on the dotsync codebase itself. This tool is developed from the [agent-tools workspace](https://github.com/maxeonyx/agent-tools); clone and develop there, not from this repo directly.

## Start Here

- Read `DESIGN.md` before changing command behavior, scope semantics, sync rules, or any product requirement.
- Read `README.md` when updating public-facing positioning, quick-start content, or outbound links.
- Read `docs/SKILL.md` only when editing the end-user dotfiles workflow skill that agents load while changing config files.

## Project Overview

`dotsync` is a Rust CLI that wraps `jj` (Jujutsu) workflows for dotfile synchronization using scope branches and merge cascades.

Core flows implemented: `dotsync init`, sync, commit with cascade, conflict pause/resume via `dotsync continue`, `--output json`, drift detection, scope isolation, multi-machine support.

`jj` (Jujutsu) is a runtime dependency. It may not be installed in every dev environment yet.

## Scope Model

- Scopes form a DAG of branches (for example `all -> linux -> hyprland -> machine`), and machine scopes are leaf scopes.
- The full model, rationale, and command contract live in `DESIGN.md`; treat it as the source of truth.

## Key Files

- `DESIGN.md`: read when implementation choices might affect requirements or workflow semantics
- `src/main.rs`: read when modifying CLI parsing, command shapes, or startup behavior
- `.github/workflows/ci.yml`: read when changing CI, release, or Pages deployment
- `docs/index.html`: read when updating the public landing page content or style
- `docs/SKILL.md`: read when refining end-user agent instructions for dotfiles edits

## TDD Ratchet

This project uses strict TDD via [tdd-ratchet](https://tdd-ratchet.maxeonyx.com). Run `cargo ratchet` instead of `cargo test`. New tests must fail first (committed as `pending`), then pass in a separate commit. See `.test-status.json` for current test states.

## CI and Release

Single `ci.yml` workflow: main-version-bump guard, format, lint, check, test, build matrix (x86_64 linux-musl + windows), GitHub Release (version from Cargo.toml, fails if that release already exists), Pages deploy (docs + binaries combined at dotsync.maxeonyx.com).

Every push to `main` must bump the crate version in both `Cargo.toml` and `Cargo.lock` so CI publishes that push as a new release. The repo-local guard lives in `scripts/check_main_version_bump.py`; CI runs it on `main`, and the repo-local `pre-push` hook in `.githooks/pre-push` runs that version check plus `cargo clippy -- -D warnings` and `cargo ratchet` for `main` pushes if the clone has hooks wired up.

When preparing a clone for local release work, set `git config core.hooksPath .githooks` so the repo-local `pre-push` hook actually runs.

**After pushing a release:** install the new binary locally:
```bash
gh release download <tag> --repo maxeonyx/dotsync --pattern 'dotsync-x86_64-linux-musl' --dir /tmp/ --clobber
chmod +x /tmp/dotsync-x86_64-linux-musl
sudo cp /tmp/dotsync-x86_64-linux-musl /usr/local/sbin/dotsync
dotsync --version  # verify
```
