#!/usr/bin/env python3

"""Exercises check_main_version_bump.py against throwaway git repositories.

The guard decides whether a push to main is allowed, so a mistake in it either
blocks a push that should go through or lets an unreleased binary claim a
version that was already published. Both are cheap to catch here: every case
below builds a real repository, makes real commits, and runs the real script.
"""

import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
GUARD = REPO_ROOT / "scripts" / "check_main_version_bump.py"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "ci.yml"
ZERO_OID = "0" * 40


class Repo:
    def __init__(self, root: Path) -> None:
        self.root = root

    def git(self, *args: str) -> str:
        result = subprocess.run(
            ["git", *args],
            cwd=self.root,
            capture_output=True,
            text=True,
            check=True,
        )
        return result.stdout.strip()

    def write(self, path: str, content: str) -> None:
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)

    def set_version(self, version: str, *, docs_version: str | None = None, lock_version: str | None = None) -> None:
        self.write("Cargo.toml", f'[package]\nname = "dotsync"\nversion = "{version}"\n')
        self.write("Cargo.lock", f'[[package]]\nname = "dotsync"\nversion = "{lock_version or version}"\n')
        self.write(
            "docs/version.json",
            json.dumps({"package": "dotsync", "binary": "dotsync", "version": docs_version or version}) + "\n",
        )

    def commit(self, message: str) -> str:
        self.git("add", "-A")
        self.git("commit", "-m", message)
        return self.git("rev-parse", "HEAD")

    def guard(self, *args: str, stdin: str = "") -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(GUARD), *args],
            cwd=self.root,
            input=stdin,
            capture_output=True,
            text=True,
            check=False,
        )


def new_repo(root: Path) -> Repo:
    repo = Repo(root)
    repo.git("init", "-b", "main")
    repo.git("config", "user.email", "guard-test@example.com")
    repo.git("config", "user.name", "Guard Test")
    repo.write("src/main.rs", "fn main() {}\n")
    repo.write("README.md", "# guard test\n")
    repo.write("tests/smoke.rs", "// nothing yet\n")
    repo.set_version("0.1.0")
    repo.commit("Initial commit")
    repo.git("tag", "v0.1.0")
    return repo


def expect(result: subprocess.CompletedProcess[str], *, passes: bool, mentions: str = "") -> None:
    output = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
    if passes and result.returncode != 0:
        raise AssertionError(f"expected the guard to allow this push, but it refused.\n{output}")
    if not passes and result.returncode == 0:
        raise AssertionError(f"expected the guard to refuse this push, but it allowed it.\n{output}")
    if mentions and mentions not in result.stdout + result.stderr:
        raise AssertionError(f"expected the guard to mention {mentions!r}.\n{output}")


def markdown_only_change_needs_no_bump(repo: Repo) -> None:
    base = repo.git("rev-parse", "HEAD")
    repo.write("README.md", "# guard test\n\nReflowed.\n")
    repo.write("tests/agent-scenarios/README.md", "# scenarios\n")
    head = repo.commit("Un-hard-wrap markdown")
    expect(repo.guard("range", "--base", base, "--head", head), passes=True, mentions="feeds the binary")


def test_change_needs_no_bump(repo: Repo) -> None:
    base = repo.git("rev-parse", "HEAD")
    repo.write("tests/smoke.rs", "#[test]\nfn smoke() {}\n")
    repo.write(".test-status.json", "{}\n")
    head = repo.commit("Add a test")
    expect(repo.guard("range", "--base", base, "--head", head), passes=True)


def source_change_without_a_bump_is_refused(repo: Repo) -> None:
    base = repo.git("rev-parse", "HEAD")
    repo.write("src/main.rs", "fn main() { println!(\"hi\"); }\n")
    head = repo.commit("Change behaviour")
    expect(repo.guard("range", "--base", base, "--head", head), passes=False, mentions="src/main.rs")


def source_change_with_a_bump_is_allowed(repo: Repo) -> None:
    base = repo.git("rev-parse", "HEAD")
    repo.write("src/main.rs", "fn main() { println!(\"hi\"); }\n")
    repo.set_version("0.1.1")
    head = repo.commit("Change behaviour and release 0.1.1")
    expect(repo.guard("range", "--base", base, "--head", head), passes=True)


def dependency_change_without_a_bump_is_refused(repo: Repo) -> None:
    base = repo.git("rev-parse", "HEAD")
    repo.write("Cargo.lock", '[[package]]\nname = "dotsync"\nversion = "0.1.0"\n\n[[package]]\nname = "serde"\n')
    head = repo.commit("Update a dependency")
    expect(repo.guard("range", "--base", base, "--head", head), passes=False, mentions="Cargo.lock")


def bumping_onto_an_existing_tag_is_refused(repo: Repo) -> None:
    base = repo.git("rev-parse", "HEAD")
    repo.write("src/main.rs", "fn main() { println!(\"hi\"); }\n")
    repo.set_version("0.0.9")
    head = repo.commit("Move the version backwards onto a released tag")
    repo.git("tag", "v0.0.9", base)
    expect(repo.guard("range", "--base", base, "--head", head), passes=False, mentions="already exists")


def disagreeing_version_files_are_refused(repo: Repo) -> None:
    base = repo.git("rev-parse", "HEAD")
    repo.set_version("0.1.1", docs_version="0.1.0")
    head = repo.commit("Bump Cargo.toml but forget docs/version.json")
    expect(repo.guard("range", "--base", base, "--head", head), passes=False, mentions="docs/version.json")


def disagreeing_version_files_are_refused_even_without_a_bump(repo: Repo) -> None:
    """The three files must agree on every push, not only on releases."""
    base = repo.git("rev-parse", "HEAD")
    repo.write("docs/version.json", json.dumps({"package": "dotsync", "binary": "dotsync", "version": "9.9.9"}) + "\n")
    head = repo.commit("Break docs/version.json on its own")
    expect(repo.guard("range", "--base", base, "--head", head), passes=False, mentions="version files disagree")


def an_unreadable_base_falls_back_to_the_first_parent(repo: Repo) -> None:
    """A rewritten remote main leaves a base oid nobody can read; compare anyway."""
    repo.write("src/main.rs", "fn main() { println!(\"hi\"); }\n")
    head = repo.commit("Change behaviour after a history rewrite")
    expect(repo.guard("range", "--base", "1" * 40, "--head", head), passes=False, mentions="cannot read main base")


def an_unreadable_base_falls_back_to_the_latest_tag_when_there_is_no_parent(repo: Repo) -> None:
    repo.git("checkout", "--orphan", "rebuilt")
    repo.write("src/main.rs", "fn main() { println!(\"hi\"); }\n")
    head = repo.commit("Rebuild main from nothing")
    expect(repo.guard("range", "--base", "1" * 40, "--head", head), passes=False, mentions="v0.1.0")


def a_brand_new_main_is_allowed(repo: Repo) -> None:
    head = repo.git("rev-parse", "HEAD")
    expect(repo.guard("range", "--base", ZERO_OID, "--head", head), passes=True)


def pre_push_refuses_an_unbumped_source_change(repo: Repo) -> None:
    base = repo.git("rev-parse", "HEAD")
    repo.write("src/main.rs", "fn main() { println!(\"hi\"); }\n")
    head = repo.commit("Change behaviour")
    stdin = f"refs/heads/main {head} refs/heads/main {base}\n"
    expect(repo.guard("pre-push", stdin=stdin), passes=False, mentions="src/main.rs")


def pre_push_allows_a_markdown_change(repo: Repo) -> None:
    base = repo.git("rev-parse", "HEAD")
    repo.write("README.md", "# guard test\n\nReflowed.\n")
    head = repo.commit("Un-hard-wrap markdown")
    stdin = f"refs/heads/main {head} refs/heads/main {base}\n"
    expect(repo.guard("pre-push", stdin=stdin), passes=True)


def pre_push_ignores_other_branches(repo: Repo) -> None:
    base = repo.git("rev-parse", "HEAD")
    repo.write("src/main.rs", "fn main() { println!(\"hi\"); }\n")
    head = repo.commit("Change behaviour on a branch")
    stdin = f"refs/heads/work {head} refs/heads/work {base}\n"
    expect(repo.guard("pre-push", stdin=stdin), passes=True)


def source_ci_is_explicitly_dispatched(_repo: Repo) -> None:
    """Source CI must never spend runner minutes merely because a ref moved."""
    workflow = WORKFLOW.read_text()
    if "workflow_dispatch:" not in workflow or "pr_number:" not in workflow:
        raise AssertionError("source CI must be explicitly dispatched for a pull request")
    if "push:" in workflow or "pull_request:" in workflow or "merge_group:" in workflow:
        raise AssertionError("source CI must not trigger automatically when a ref moves")


def pre_push_refuses_deleting_main(repo: Repo) -> None:
    base = repo.git("rev-parse", "HEAD")
    stdin = f"(delete) {ZERO_OID} refs/heads/main {base}\n"
    expect(repo.guard("pre-push", stdin=stdin), passes=False, mentions="refusing to delete")


CASES = [
    markdown_only_change_needs_no_bump,
    test_change_needs_no_bump,
    source_change_without_a_bump_is_refused,
    source_change_with_a_bump_is_allowed,
    dependency_change_without_a_bump_is_refused,
    bumping_onto_an_existing_tag_is_refused,
    disagreeing_version_files_are_refused,
    disagreeing_version_files_are_refused_even_without_a_bump,
    an_unreadable_base_falls_back_to_the_first_parent,
    an_unreadable_base_falls_back_to_the_latest_tag_when_there_is_no_parent,
    a_brand_new_main_is_allowed,
    pre_push_refuses_an_unbumped_source_change,
    pre_push_allows_a_markdown_change,
    pre_push_ignores_other_branches,
    pre_push_refuses_deleting_main,
    source_ci_is_explicitly_dispatched,
]


def main() -> int:
    failures = 0
    for case in CASES:
        with tempfile.TemporaryDirectory() as scratch:
            repo = new_repo(Path(scratch))
            try:
                case(repo)
            except AssertionError as exc:
                failures += 1
                print(f"FAIL {case.__name__}: {exc}")
            else:
                print(f"ok   {case.__name__}")
    print(f"\n{len(CASES) - failures}/{len(CASES)} passed")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
