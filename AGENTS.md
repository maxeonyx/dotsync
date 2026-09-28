# dotsync - Agent Instructions

This repository is self-contained for development. A standalone clone must build, test, and release without an `agent-tools` checkout.

## TDD ratchet — read before testing

Run `cargo ratchet`, not plain `cargo test`. A new test must be red when first introduced and committed as `pending`; that expected red test keeps CI green. A new test must not pass when first introduced—doing so makes the ratchet and CI red. Push the red implementation commit, then wait for the trusted ledger workflow's ledger-only bot commit before implementing the fix. After implementation, rerun the ratchet, push the green commit, and again wait for the bot commit that records the promotion to `passing`.

**The tests encode the design, so a design change changes them** (Max): _"the tests encode the design - they are derived from the design. If the design changes, the tests thus change. No questions needed."_ Rewrite or delete such a test in the same commit as the behaviour change, and say why in the message. Never work around a test you believe is wrong, and never silently delete one.

**[tdd-ratchet](https://tdd-ratchet.maxeonyx.com)'s ledger, `.test-status.json`, is bot-written output.** Never commit it by hand. Declare a deliberate rename or removal under `renames` or `removals` in `.tdd-ratchet.json`, commit that alongside the change, and delete the instruction file in the next commit — a leftover instruction fails every later ratchet run with `removal target is not present in committed status`. Renames are the common case here: nextest ids carry the module path, so moving a test renames it, and commit `39f73c6` moved all 146 scenarios out of `tests/user_flows.rs` into eleven area files.

**Don't run `cargo ratchet` from a git hook.** It fails there: git's `GIT_DIR`/`GIT_WORK_TREE` leak into the test processes, so every test that shells out to git resolves against the wrong repository ([tdd-ratchet-rs#4](https://github.com/maxeonyx/tdd-ratchet-rs/issues/4)).

## Integration workflow

Run `devenv test` before committing and pushing; it includes `actionlint`, so workflow syntax is checked offline. Source CI does not run on push. Open a pull request, merge current `main` into the feature branch, mark the pull request ready — the run's own merge step fails with `Pull Request is still a draft` otherwise, after spending ten minutes building — then explicitly dispatch:

```bash
gh pr ready <number>
gh workflow run ci.yml --ref <feature-branch> -f pr_number=<number>
```

The repository-serialized run records the required `Ready` check, builds the release artifacts, auto-merges the pull request, publishes those same artifacts, and records `integrated-ci` on the exact merge commit.

The trusted ledger workflow runs on every push to an open pull request and commits even when the ledger is unchanged, so every ledger run moves the head SHA. Dispatch only once the ledger run for that push has finished. Dispatch first and the bot's commit lands after `Ready` was recorded, leaving the required status on a commit that is no longer the head, so auto-merge waits for a check that will never arrive and the Merge job fails.

## Design discipline — read before designing anything

This is the stable part: how to decide what dotsync should be. Follow it, and hand it on — to any agent you delegate to, and in any guidance you write.

- **Optimise terminal consequences, not proxies.** The question is what happens to Max, to the agents editing his config, and to the next developer — not whether the architecture looks clean, the diff is small, or the test count went up. Every command, type, flag, test and paragraph has to earn its cost in those consequences.
- **The current implementation is evidence, not specification.** Understand what it does and why before keeping, changing or deleting it; then keep it only if it still earns its place.
- **Work from concrete episodes, broadly, before converging.** Actor × goal × state × what they try × what they need to see × what could go wrong × how they verify × how they recover. Max is the primary source of real use cases — ask him. Real transcripts, the dotfiles history, and `~/.config/AGENTS.md` (his own procedure for agents, much of it written to compensate for missing tooling) are the next best evidence. Separate evidenced episodes from generic heuristics.
- **Generate genuinely competing designs where the choice is consequential**, replay the episodes through each, and compare by consequences. Use independent agents with fresh context for episode mining, competing models and final review when anchoring is a risk — and disposition every material finding they return.
- **Integrate rather than accumulate.** Before adding a command, flag or special case, look for the one model that makes several problems disappear. The standing table and pins-in-the-pass are this repo's examples: one relation answers every read; one mechanism is every write, its cascade, its preview and its report.
- **Treat implementation friction as design evidence**, and refactor radically when a better model appears. Represent essential complexity honestly, then delete the compensating machinery it makes redundant, in the same change.
- **Agents are the primary users, so investigation is a product feature.** Dotsync must expose enough read-side evidence to justify every write it allows, and every write must say what it will do and did. The abstraction boundary is the point: an agent must never need jj, the hidden repo, or a clone of the remote to do ordinary work. Any time one does, dotsync is incomplete.
- **The development system is part of the product.** Black-box scenario tests over real fleets are what make radical change safe; keep them expressing episodes and invariants rather than implementation accidents.
- **Before stopping, reconsider the whole system**: if you had known at the start what you know now, would you build exactly this? Take the second-order simplifications. Stop when further passes produce only cosmetic or speculative change.
- **Keep this guidance true.** Project facts below go stale; when they are wrong, fix them rather than working around them. Propagate this section, recursively, to whoever works next.

## How dotsync thinks — current project facts

Update these when they stop being true.

- **Scopes and standing.** A scope's tree is already its effective config; what matters for every decision is the scope's *own* layer — the difference from the merge of its parents' trees, per path: inherited, added, overridden, removed. `fleet::Fleet` computes it, always over the pass predicted in a transaction nothing commits (a parent not yet merged down would otherwise read as the child overriding it). Every read (`scopes`, `files`, `show`, `diff <scope> [<scope>]`) is a reading of that table, for any scope, from any machine, before joining too.
- **Every write is pins in the pass.** `converge::Pins`: per scope, per path, what the scope holds once its parents have merged into it (`Holds(value)` or `Inherits`). `commit` pins home's three-way edit on its target; `move`/`drop` (`src/place.rs`) pin repo content and keep every other scope's own version. The pass lays pins in cascade order, so multi-scope writes are one run and ordering is the graph's job. `place::plan` compares every machine's config before and after — that is the effect every write reports, and `--dry-run` is the same plan with the transaction dropped.
- **Where writes may land.** Content from home may only land on this machine's scope or its ancestors (home has no base on anyone else's scope). Content from the repo may land anywhere (Max, 2026-09-28). What neither can know is whether other machines have tried the config — which is what the effect lists.
- **The graph is structural**: a scope is a bookmark whose history holds its creation commit; edges are creation-commit ancestry. Create and delete exist; reparent/rename are designed but not built (PLAN, "Graph changes").
- **Home is jj's working copy**; the mark is the wc commit's parent; a sync is `merge(home, mark, tip)`; nothing about a pause is stored except a stopped commit's own record. `init` carries home: collisions stop the first sync rather than overwrite.

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

Home is jj's working copy through dotsync's own `WorkingCopy` implementation; the scope graph is derived from the repo's structure; one convergence pass moves every scope bookmark and lays every write's pins; a paused merge is recomputed rather than stored. Commands: `dotsync`, `init`, `create-scope`, `delete-scope`, `commit`, `move`, `drop`, `discard`, `status`, `diff`, `scopes`, `files`, `show`, `continue`, `abort`, with `--output json` everywhere and `--dry-run` on every write.

jj is linked in as a library (`jj-lib`); the jj CLI is never needed. See "Hard-won knowledge" below.

## Scope Model

- Scopes form a DAG of branches (for example `all -> linux -> hyprland -> machine`), and machine scopes are leaf scopes.
- The full model, rationale, and command contract live in `DESIGN.md`; treat it as the source of truth.

## Key Files

- `DESIGN.md`: read when implementation choices might affect requirements or workflow semantics; it also holds the JSON contract
- `src/main.rs`: read when modifying CLI parsing, command shapes, or startup behavior
- `src/fleet.rs`: the standing table and the per-machine effect; read before adding any read or report
- `src/place.rs`: plans, `move`/`drop`, and how a planned write is carried out; read before adding any write
- `.github/workflows/ci.yml`: read when changing CI, release, or Pages deployment
- `docs/index.html`: read when updating the public landing page content or style
- `docs/SKILL.md`: read when refining end-user agent instructions for dotfiles edits

## CI and Release

PRs in this repo can be merged without approval (Max, 2026-08-12).

Single explicitly dispatched `ci.yml` integration workflow: PR/base validation, actionlint, release-guard tests, format, lint, the test ratchet, Linux and Windows builds, auto-merge, GitHub Release, Pages, then an `integrated-ci` status on the exact merge commit.

Each integration run publishes a fresh release, so its PR must use a new version in `Cargo.toml`, `Cargo.lock`, and `docs/version.json`. The existing release guard still verifies that artifact-changing work actually moved these versions together. `docs/version.json` is deployed verbatim to Pages and read by the agent-tools umbrella.

CI compares the PR head with `origin/main` in `range` mode and runs `scripts/test_check_main_version_bump.py`. The repo-local pre-push hook remains an early version check; `devenv test` is the full offline actionlint, format, clippy, and ratchet gate.

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
