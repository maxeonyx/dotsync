# dotsync - Agent Instructions

This repository is self-contained for development. A standalone clone must
build, test, and release without an `agent-tools` checkout.

## TDD ratchet — read before testing

Run `cargo ratchet`, not plain `cargo test`. A new test must be red when first introduced and committed as `pending`; that expected red test keeps CI green. A new test must not pass when first introduced—doing so makes the ratchet and CI red. Implement only after the red commit, then rerun the ratchet and commit the promotion to `passing`.

## Start Here

- Read `DESIGN.md` before changing command behavior, scope semantics, sync rules, or any product requirement.
- Read `PLAN.md` for current priorities, the ordered work plan, and standing constraints (notably: never hand-mutate the hidden repo or dotfiles history).
- Read `README.md` when updating public-facing positioning, quick-start content, or outbound links.
- Read `docs/SKILL.md` only when editing the end-user dotfiles workflow skill that agents load while changing config files.

## Stakes, and how to write about them

Don't reach for "safety", "risk" or data-loss drama when describing this repo's work. Max, 2026-08-14: _"I don't care about 'safety' in this repo, frankly. It's low stakes. Just do the work - regression is not a 'problem' per say, it's just inconvenient (and the whole point of this project is convenience)."_

Tests, review and the ratchet are here because they make the work faster and less annoying, not because a regression is a disaster. Describe a defect by what it does to the person using it — "the machine stops syncing until you fix the file by hand" — rather than by how alarming it sounds.

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

This project uses strict TDD via [tdd-ratchet](https://tdd-ratchet.maxeonyx.com). See `.test-status.json` for current test states.

**Renaming or deleting a test is supported — use the `renames` and `removals` entries in `.test-status.json`, edited in the same commit as the change.** `renames` maps **new name to old name**; the ratchet takes the old name's recorded state, moves it onto the new name, and requires that the old name is tracked, the new name is not yet tracked, and the run saw the new name and not the old one. It reads the applied renames back out of the commit's history snapshot afterwards, so the entry is a one-commit bridge: drop it in the next commit or every future run warns that it is stale. Commits `1deeb74` and `39f73c6` both used it — the second moved all 146 scenarios out of `tests/user_flows.rs` into eleven area files, since nextest ids carry the module path and moving a test therefore renames it.

## CI and Release

PRs in this repo can be merged without approval (Max, 2026-08-12).

Single `ci.yml` workflow: main-version-bump guard, release-guard tests, format, lint, check, test, build matrix (x86_64 linux-gnu + windows), GitHub Release (version from Cargo.toml, skipped when that release already exists), Pages deploy (docs + binaries combined at dotsync.maxeonyx.com).

**A push to `main` needs a version bump only when it changes what CI would build.** That means `src/**`, `Cargo.toml`, `Cargo.lock` or `rust-toolchain.toml` — the list is `ARTIFACT_FILES` and `ARTIFACT_DIRS` in `scripts/check_main_version_bump.py`. Markdown, tests, `.test-status.json`, `scripts/**` and the workflow itself never need one: push them freely, CI still runs and still redeploys Pages, and the Release job sees its tag already exists and does nothing. When a bump *is* needed, all three of `Cargo.toml`, `Cargo.lock` and `docs/version.json` must move together — the guard rejects the push if they disagree, on every push, bump or not. `docs/version.json` is deployed verbatim to Pages (`dotsync.maxeonyx.com/version.json`) and read by the agent-tools umbrella, so leaving it behind ships a false version; Pages therefore deploys only after the Release job succeeds.

CI runs the guard on `main` (`range` mode) and runs `scripts/test_check_main_version_bump.py`, which covers the guard's own behaviour and checks that every artifact-relevant path is also a CI trigger path. The repo-local `pre-push` hook in `.githooks/pre-push` runs the same guard in `pre-push` mode for `main` pushes if the clone has hooks wired up — that check and nothing else, so pushing stays fast; clippy and the ratchet are CI's job.

When preparing a clone for local release work, set `git config core.hooksPath .githooks` so the repo-local `pre-push` hook actually runs.

**After pushing a release:** install the new binary locally:

```bash
gh release download <tag> --repo maxeonyx/dotsync --pattern 'dotsync-x86_64-linux' --dir /tmp/ --clobber
chmod +x /tmp/dotsync-x86_64-linux
cp /tmp/dotsync-x86_64-linux ~/.local/bin/dotsync
dotsync --version  # verify
```
