#!/usr/bin/env python3

import argparse
import subprocess
import sys
import tomllib
from dataclasses import dataclass

ZERO_OID = "0" * 40


@dataclass(frozen=True)
class PackageVersion:
    name: str
    version: str


class VersionCheckError(RuntimeError):
    pass


def git_show(ref: str, path: str) -> bytes:
    result = subprocess.run(
        ["git", "show", f"{ref}:{path}"],
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        stderr = result.stderr.decode("utf-8", errors="replace").strip()
        raise VersionCheckError(f"failed to read {path} at {ref}: {stderr}")
    return result.stdout


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


def ensure_lock_matches_manifest(ref: str) -> PackageVersion:
    package = load_package_version(ref)
    lock_version = load_lock_version(ref, package.name)
    if lock_version != package.version:
        raise VersionCheckError(
            f"Cargo.lock package version for {package.name} is {lock_version} at {ref}, "
            f"but Cargo.toml is {package.version}"
        )
    return package


def ensure_main_version_bumped(base_ref: str, head_ref: str) -> None:
    base_package = load_package_version(base_ref)
    head_package = ensure_lock_matches_manifest(head_ref)
    if base_package.name != head_package.name:
        raise VersionCheckError(
            f"package name changed from {base_package.name} to {head_package.name}; "
            "main push guard expects the same package"
        )
    if base_package.version == head_package.version:
        raise VersionCheckError(
            f"main push would keep {head_package.name} at version {head_package.version}. "
            "Every push to main must bump Cargo.toml and Cargo.lock first so CI releases a new version instead of mutating existing assets."
        )


def handle_range(base_ref: str, head_ref: str) -> None:
    if base_ref == ZERO_OID:
        ensure_lock_matches_manifest(head_ref)
        return
    ensure_main_version_bumped(base_ref, head_ref)


def handle_pre_push() -> None:
    saw_main_update = False
    for line in sys.stdin:
        stripped = line.strip()
        if not stripped:
            continue
        local_ref, local_oid, remote_ref, remote_oid = stripped.split()
        if remote_ref != "refs/heads/main":
            continue
        saw_main_update = True
        if local_oid == ZERO_OID:
            raise VersionCheckError("refusing to delete refs/heads/main")
        handle_range(remote_oid, local_oid)
    if not saw_main_update:
        return


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
