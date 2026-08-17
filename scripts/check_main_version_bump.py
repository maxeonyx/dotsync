#!/usr/bin/env python3

import argparse
import json
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass

ZERO_OID = "0" * 40
DOCS_VERSION_PATH = "docs/version.json"

# A push only has to mint a release when the binary CI publishes could differ
# from the one already published. These are the paths that feed `cargo build`:
# the sources, the manifest, the locked dependency versions, and the toolchain
# pin. Markdown, tests, CI config and dev-shell files change how the repo is
# worked on, not what comes out of the compiler, so they push freely.
ARTIFACT_FILES = frozenset({"Cargo.toml", "Cargo.lock", "rust-toolchain.toml"})
ARTIFACT_DIRS = ("src/",)


@dataclass(frozen=True)
class PackageVersion:
    name: str
    version: str


class VersionCheckError(RuntimeError):
    pass


@dataclass(frozen=True)
class VersionBaseline:
    ref: str
    package: PackageVersion


def git_stdout(*args: str) -> bytes:
    result = subprocess.run(
        ["git", *args],
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        stderr = result.stderr.decode("utf-8", errors="replace").strip()
        raise VersionCheckError(f"git {' '.join(args)} failed: {stderr}")
    return result.stdout


def git_show(ref: str, path: str) -> bytes:
    try:
        return git_stdout("show", f"{ref}:{path}")
    except VersionCheckError as exc:
        raise VersionCheckError(f"failed to read {path} at {ref}: {exc}") from exc


def git_commit_exists(ref: str) -> bool:
    result = subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}"],
        capture_output=True,
        check=False,
    )
    return result.returncode == 0


def git_first_parent(ref: str) -> str | None:
    parents = git_stdout("rev-list", "--parents", "-n", "1", ref).decode("utf-8").strip().split()
    if len(parents) < 2:
        return None
    return parents[1]


def path_feeds_binary(path: str) -> bool:
    return path in ARTIFACT_FILES or path.startswith(ARTIFACT_DIRS)


def artifact_changes(base_ref: str, head_ref: str) -> list[str]:
    """The changed paths between two trees that could change the built binary."""
    changed = git_stdout("diff", "--name-only", "--no-renames", base_ref, head_ref)
    return sorted(path for path in changed.decode("utf-8").splitlines() if path_feeds_binary(path))


def git_ref_exists(ref: str) -> bool:
    result = subprocess.run(
        ["git", "show-ref", "--verify", "--quiet", ref],
        capture_output=True,
        check=False,
    )
    return result.returncode == 0


def git_latest_version_tag() -> str | None:
    tag_pattern = re.compile(r"^v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$")
    tags = git_stdout("tag", "--list", "--sort=-version:refname").decode("utf-8").splitlines()
    for tag in tags:
        if tag_pattern.match(tag):
            return tag
    return None


def load_package_version(ref: str) -> PackageVersion:
    cargo_toml = tomllib.loads(git_show(ref, "Cargo.toml").decode("utf-8"))
    package = cargo_toml.get("package")
    if not isinstance(package, dict):
        raise VersionCheckError(f"Cargo.toml at {ref} is missing [package]")
    name = package.get("name")
    version = package.get("version")
    if not isinstance(name, str) or not isinstance(version, str):
        raise VersionCheckError(f"Cargo.toml at {ref} is missing package name/version")
    return PackageVersion(name=name, version=version)


def load_lock_version(ref: str, package_name: str) -> str:
    cargo_lock = tomllib.loads(git_show(ref, "Cargo.lock").decode("utf-8"))
    packages = cargo_lock.get("package")
    if not isinstance(packages, list):
        raise VersionCheckError(f"Cargo.lock at {ref} is missing [[package]] entries")
    for package in packages:
        if isinstance(package, dict) and package.get("name") == package_name:
            version = package.get("version")
            if isinstance(version, str):
                return version
            raise VersionCheckError(f"Cargo.lock at {ref} has a non-string version for {package_name}")
    raise VersionCheckError(f"Cargo.lock at {ref} does not contain package {package_name}")


def load_docs_version(ref: str) -> str:
    document = json.loads(git_show(ref, DOCS_VERSION_PATH).decode("utf-8"))
    if not isinstance(document, dict):
        raise VersionCheckError(f"{DOCS_VERSION_PATH} at {ref} is not a JSON object")
    version = document.get("version")
    if not isinstance(version, str):
        raise VersionCheckError(f"{DOCS_VERSION_PATH} at {ref} is missing a string \"version\" field")
    return version


def ensure_versions_consistent(ref: str) -> PackageVersion:
    """Every place the released version is written must agree with Cargo.toml."""
    package = load_package_version(ref)
    mismatches = []

    lock_version = load_lock_version(ref, package.name)
    if lock_version != package.version:
        mismatches.append(f"Cargo.lock has {package.name} at {lock_version}")

    docs_version = load_docs_version(ref)
    if docs_version != package.version:
        mismatches.append(f"{DOCS_VERSION_PATH} has version {docs_version}")

    if mismatches:
        raise VersionCheckError(
            f"version files disagree at {ref}: Cargo.toml has {package.name} at {package.version}, but "
            + "; ".join(mismatches)
            + f". Set the same version in all of Cargo.toml, Cargo.lock and {DOCS_VERSION_PATH} "
            "(run `cargo check` to refresh Cargo.lock), then commit them together."
        )
    return package


def describe_changes(changed: list[str]) -> str:
    if len(changed) <= 5:
        return ", ".join(changed)
    return ", ".join(changed[:5]) + f", and {len(changed) - 5} more"


def ensure_release_tag_unused(package: PackageVersion, head_ref: str, changed: list[str]) -> None:
    tag_ref = f"refs/tags/v{package.version}"
    if git_ref_exists(tag_ref):
        raise VersionCheckError(
            f"{head_ref} changes what CI would build ({describe_changes(changed)}) and sets {package.name} to "
            f"version {package.version}, but tag v{package.version} already exists. Use a fresh crate version so "
            "CI publishes these changes as a new release."
        )


def ensure_version_bumped(
    baseline: VersionBaseline,
    head_package: PackageVersion,
    head_ref: str,
    changed: list[str],
) -> None:
    if baseline.package.name != head_package.name:
        raise VersionCheckError(
            f"package name changed from {baseline.package.name} at {baseline.ref} to {head_package.name} "
            f"at {head_ref}; main push guard expects the same package"
        )
    if baseline.package.version != head_package.version:
        return
    raise VersionCheckError(
        f"main push changes what CI would build ({describe_changes(changed)}) but keeps {head_package.name} at "
        f"version {head_package.version}, the same version as {baseline.ref}. Bump Cargo.toml, Cargo.lock and "
        f"{DOCS_VERSION_PATH} together so CI publishes these changes as a new release."
    )


def load_fallback_baseline(head_ref: str) -> VersionBaseline | None:
    parent_ref = git_first_parent(head_ref)
    if parent_ref is not None and git_commit_exists(parent_ref):
        return VersionBaseline(ref=parent_ref, package=load_package_version(parent_ref))

    latest_tag = git_latest_version_tag()
    if latest_tag is not None and git_commit_exists(latest_tag):
        return VersionBaseline(ref=latest_tag, package=load_package_version(latest_tag))

    return None


def resolve_baseline(base_ref: str, head_ref: str) -> VersionBaseline:
    if git_commit_exists(base_ref):
        return VersionBaseline(ref=base_ref, package=load_package_version(base_ref))

    fallback = load_fallback_baseline(head_ref)
    if fallback is None:
        raise VersionCheckError(
            f"cannot read rewritten main base {base_ref}, and no local fallback baseline is available for {head_ref}."
        )
    print(f"dotsync release guard: cannot read main base {base_ref}; comparing against {fallback.ref} instead.")
    return fallback


def handle_range(base_ref: str, head_ref: str) -> None:
    head_package = ensure_versions_consistent(head_ref)
    if base_ref == ZERO_OID:
        return

    baseline = resolve_baseline(base_ref, head_ref)
    changed = artifact_changes(baseline.ref, head_ref)
    if not changed:
        print(
            f"dotsync release guard: nothing between {baseline.ref} and {head_ref} feeds the binary, "
            "so no version bump is needed."
        )
        return

    ensure_version_bumped(baseline, head_package, head_ref, changed)
    ensure_release_tag_unused(head_package, head_ref, changed)


def handle_pre_push() -> None:
    for line in sys.stdin:
        stripped = line.strip()
        if not stripped:
            continue
        _local_ref, local_oid, remote_ref, remote_oid = stripped.split()
        if remote_ref != "refs/heads/main":
            continue
        if local_oid == ZERO_OID:
            raise VersionCheckError("refusing to delete refs/heads/main")
        handle_range(remote_oid, local_oid)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    range_parser = subparsers.add_parser("range")
    range_parser.add_argument("--base", required=True)
    range_parser.add_argument("--head", required=True)

    subparsers.add_parser("pre-push")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.command == "range":
            handle_range(args.base, args.head)
        elif args.command == "pre-push":
            handle_pre_push()
        else:
            raise AssertionError(f"unexpected command: {args.command}")
    except VersionCheckError as exc:
        print(f"dotsync release guard: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
