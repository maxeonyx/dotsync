# dotsync - Agent Instructions

This repository is self-contained for development. A standalone clone must
build, test, and release without an `agent-tools` checkout.

## TDD ratchet — read before testing

Run `cargo ratchet`, not plain `cargo test`. A new test must be red when first introduced and committed as `pending`; that expected red test keeps CI green. A new test must not pass when first introduced—doing so makes the ratchet and CI red. Implement only after the red commit, then rerun the ratchet and commit the promotion to `passing`.

**The tests encode the design, so a design change changes them** (Max): _"the tests encode the design - they are derived from the design. If the design changes, the tests thus change. No questions needed."_ Rewrite or delete such a test in the same commit as the behaviour change, and say why in the message. Never work around a test you believe is wrong, and never silently delete one.

**Don't run `cargo ratchet` from a git hook.** It fails there: git's `GIT_DIR`/`GIT_WORK_TREE` leak into the test processes, so every test that shells out to git resolves against the wrong repository ([tdd-ratchet-rs#4](https://github.com/maxeonyx/tdd-ratchet-rs/issues/4)).

**Run the ratchet once, then commit what it wrote.** It rewrites `.test-status.json` itself — consuming a `removals` list, applying a `renames` bridge, flipping a promotion — and running it again before committing that rewrite reports the removals as tests missing from the run.

## Integration workflow

Run `devenv test` before committing and pushing; it includes `actionlint`, so
workflow syntax is checked offline. Source CI does not run on push. Open a pull
request, merge current `main` into the feature branch, mark the pull request
ready — the run's own merge step fails with `Pull Request is still a draft`
otherwise, after spending ten minutes building — then explicitly dispatch:

```bash
gh pr ready <number>
gh workflow run ci.yml --ref <feature-branch> -f pr_number=<number>
```

The repository-serialized run records the required `Ready` check, builds the
release artifacts, auto-merges the pull request, publishes those same artifacts,
and records `integrated-ci` on the exact merge commit.

## Start Here

- Read `DESIGN.md` before changing command behavior, scope semantics, sync rules, or any product requirement. It describes dotsync as it is.
- Read `PLAN.md` for what is not built: what is ahead, what is deferred, and the standing constraints any of it is done under (notably: never hand-mutate the hidden repo or dotfiles history).
- Read `README.md` when updating public-facing positioning, quick-start content, or outbound links.
- Read `docs/SKILL.md` only when editing the end-user dotfiles workflow skill that agents load while changing config files.

## Stakes, and how to write about them

Don't reach for "safety", "risk" or data-loss drama when describing this repo's work. Max, 2026-08-14: _"I don't care about 'safety' in this repo, frankly. It's low stakes. Just do the work - regression is not a 'problem' per say, it's just inconvenient (and the whole point of this project is convenience)."_

Tests, review and the ratchet are here because they make the work faster and less annoying, not because a regression is a disaster. Describe a defect by what it does to the person using it — "the machine stops syncing until you fix the file by hand" — rather than by how alarming it sounds.

## Project Overview

`dotsync` is a Rust CLI that wraps `jj` (Jujutsu) workflows for dotfile synchronization using scope branches and merge cascades.

Home is jj's working copy through dotsync's own `WorkingCopy` implementation; the scope graph is derived from the repo's structure; one convergence pass moves every scope bookmark; a paused merge is recomputed rather than stored. Commands: `dotsync`, `init`, `create-scope`, `commit`, `discard`, `status`, `diff`, `view`, `continue`, `abort`, with `--output json` everywhere.

`jj` (Jujutsu) is a runtime dependency. It may not be installed in every dev environment yet.

## Scope Model

- Scopes form a DAG of branches (for example `all -> linux -> hyprland -> machine`), and machine scopes are leaf scopes.
- The full model, rationale, and command contract live in `DESIGN.md`; treat it as the source of truth.

## Key Files

- `DESIGN.md`: read when implementation choices might affect requirements or workflow semantics; it also holds the JSON contract
- `src/main.rs`: read when modifying CLI parsing, command shapes, or startup behavior
- `.github/workflows/ci.yml`: read when changing CI, release, or Pages deployment
- `docs/index.html`: read when updating the public landing page content or style
- `docs/SKILL.md`: read when refining end-user agent instructions for dotfiles edits

## TDD Ratchet

This project uses strict TDD via [tdd-ratchet](https://tdd-ratchet.maxeonyx.com). See `.test-status.json` for current test states.

**Renaming or deleting a test is supported — use the `renames` and `removals` entries in `.test-status.json`.** `renames` maps **new name to old name**; the ratchet takes the old name's recorded state, moves it onto the new name, and requires that the old name is tracked, the new name is not yet tracked, and the run saw the new name and not the old one. It reads the applied renames back out of the commit's history snapshot afterwards, so the entry is a one-commit bridge: drop it in the next commit or every future run warns that it is stale. Commits `1deeb74` and `39f73c6` both used it — the second moved all 146 scenarios out of `tests/user_flows.rs` into eleven area files, since nextest ids carry the module path and moving a test therefore renames it.

`removals` is a list of tracked names, and the thing to know is what to commit. Leave the entries in `tests` and add the list; `cargo ratchet` then passes and **rewrites the file itself**, dropping both the entries and the list. Commit that rewrite. Editing the entries out by hand instead fails with "tracked test missing from run", because the removal is checked against the working tree's `tests` map, and committing the pre-rewrite file leaves every later run reporting the same thing — the run reads the baseline from the commit. The same is true of a status flip: run the ratchet and commit what it writes, rather than editing states by hand.

## CI and Release

PRs in this repo can be merged without approval (Max, 2026-08-12).

Single explicitly dispatched `ci.yml` integration workflow: PR/base validation,
actionlint, release-guard tests, format, lint, the test ratchet, Linux and
Windows builds, auto-merge, GitHub Release, Pages, then an `integrated-ci`
status on the exact merge commit.

Each integration run publishes a fresh release, so its PR must use a new version
in `Cargo.toml`, `Cargo.lock`, and `docs/version.json`. The existing release
guard still verifies that artifact-changing work actually moved these versions
together. `docs/version.json` is deployed verbatim to Pages and read by the
agent-tools umbrella.

CI compares the PR head with `origin/main` in `range` mode and runs
`scripts/test_check_main_version_bump.py`. The repo-local pre-push hook remains
an early version check; `devenv test` is the full offline actionlint, format,
clippy, and ratchet gate.

When preparing a clone for local release work, set `git config core.hooksPath .githooks` so the repo-local `pre-push` hook actually runs.

**After pushing a release:** install the new binary locally:

```bash
gh release download <tag> --repo maxeonyx/dotsync --pattern 'dotsync-x86_64-linux' --dir /tmp/ --clobber
chmod +x /tmp/dotsync-x86_64-linux
cp /tmp/dotsync-x86_64-linux ~/.local/bin/dotsync
dotsync --version  # verify
```

## Hard-won knowledge

- `jj-lib` is used as a library, never the jj CLI: user machines do not have jj installed. A `git` binary on PATH is still needed for fetch and push, which jj shells out for.
- The hidden repo's git store can be inspected read-only with `git --git-dir ~/.local/share/dotsync/repo/.jj/repo/store/git ...` — invaluable for diagnosis, never for mutation.
- The `git_target` file in `.jj/repo/store/` controls where jj-lib finds the git backend. Relative path; `git` for a non-colocated repo.
- `DOTSYNC_OS` and `DOTSYNC_HOSTNAME` override OS and hostname detection. Every test uses them, and so does every sandbox drive — never drive a build against real state; there is a live fleet using this tool.
- The primary confidence signal is the final home config, not the internal branch shape. Branch assertions support that story; they do not replace it.
