#!/usr/bin/env python3
"""Update vendored skills from their pinned git sources, preserving local edits."""

from __future__ import annotations

import json
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parent.parent
File = tuple[bytes, bool]


def command(*args: str, cwd: Path) -> str:
    """Run a command and report its failure with context."""
    result = subprocess.run(args, cwd=cwd, text=True, capture_output=True, check=False)
    if result.returncode:
        raise ValueError(f"{' '.join(args)}: {result.stderr.strip()}")
    return result.stdout.strip()


def safe_path(value: str) -> Path:
    """Reject absolute and escaping paths in the source registry."""
    path = PurePosixPath(value)
    if (
        not value
        or path.is_absolute()
        or any(part in ("..", ".") for part in path.parts)
    ):
        raise ValueError(f"invalid source path: {value!r}")
    return Path(*path.parts)


def source_path(repo: Path, path: Path) -> Path:
    """Reject symlinks in every component of an upstream path."""
    current = repo
    for part in path.parts:
        current = current / part
        if current.is_symlink():
            raise ValueError(f"symlink in upstream path: {path}")
    return current


def snapshot(folder: Path, *, skip_git: bool = False) -> dict[str, File]:
    """Read a directory without following symlinks or repository metadata."""
    if not folder.is_dir() or folder.is_symlink():
        raise ValueError(f"skill directory missing or symlinked: {folder}")
    result: dict[str, File] = {}
    for path in folder.rglob("*"):
        relative = path.relative_to(folder)
        if skip_git and relative.parts[0] == ".git":
            continue
        if path.is_symlink():
            raise ValueError(f"symlinks are not supported in skills: {path}")
        if path.is_file():
            result[relative.as_posix()] = (
                path.read_bytes(),
                bool(path.stat().st_mode & 0o111),
            )
        elif not path.is_dir():
            raise ValueError(f"unsupported skill file: {path}")
    return result


def upstream_snapshot(
    repo: Path, revision: str, skill_path: Path, license_path: Path | None
) -> dict[str, File]:
    """Read a skill and optional repository license at a commit."""
    command("git", "checkout", "--quiet", "--detach", revision, cwd=repo)
    data = snapshot(source_path(repo, skill_path), skip_git=not skill_path.parts)
    if "SKILL.md" not in data:
        raise ValueError(f"{skill_path} has no SKILL.md at {revision}")
    if license_path is not None:
        license_file = source_path(repo, license_path)
        if license_file.is_symlink() or not license_file.is_file():
            raise ValueError(f"license file missing or symlinked: {license_path}")
        license_data = (
            license_file.read_bytes(),
            bool(license_file.stat().st_mode & 0o111),
        )
        if "LICENSE" in data and data["LICENSE"] != license_data:
            raise ValueError(f"{skill_path}/LICENSE conflicts with {license_path}")
        data["LICENSE"] = license_data
    return data


def merge_file(
    name: str, base: File | None, local: File | None, remote: File | None, work: Path
) -> File | None:
    """Merge a file against its original upstream version, rejecting conflicts."""
    if remote == base:
        return local
    if local == base or local == remote:
        return remote
    if base is None or local is None or remote is None:
        raise ValueError(f"conflict in {name}: file added or removed on both sides")
    executable = remote[1] if local[1] == base[1] else local[1]
    if local[0] == base[0]:
        return remote[0], executable
    if remote[0] == base[0] or remote[0] == local[0]:
        return local[0], executable
    for contents in (base[0], local[0], remote[0]):
        if b"\x00" in contents:
            raise ValueError(f"conflict in {name}: both sides changed a binary file")
        try:
            contents.decode("utf-8")
        except UnicodeDecodeError as error:
            raise ValueError(
                f"conflict in {name}: both sides changed a binary file"
            ) from error
    for filename, contents in (
        ("local", local[0]),
        ("base", base[0]),
        ("remote", remote[0]),
    ):
        (work / filename).write_bytes(contents)
    result = subprocess.run(
        [
            "git",
            "merge-file",
            "-p",
            str(work / "local"),
            str(work / "base"),
            str(work / "remote"),
        ],
        capture_output=True,
        check=False,
    )
    if result.returncode:
        raise ValueError(
            f"conflict in {name}; reconcile upstream and local changes manually"
        )
    return result.stdout, executable


def apply_updates(updates: dict[str, dict[str, File]], registry_json: str) -> None:
    """Prepare complete replacements and restore originals if a write fails."""
    registry_path = ROOT / "skills-sources.json"
    lock_path = ROOT / "skills-lock.json"
    previous_registry = registry_path.read_bytes()
    previous_lock = lock_path.read_bytes() if lock_path.exists() else None
    staging = Path(tempfile.mkdtemp(prefix=".skills-sync-", dir=ROOT / "skills"))
    keep_backup = False
    try:
        (staging / "original-sources.json").write_bytes(previous_registry)
        if previous_lock is not None:
            (staging / "original-lock.json").write_bytes(previous_lock)
        for name, files in updates.items():
            for path, (contents, executable) in files.items():
                target = staging / "new" / name / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(contents)
                target.chmod(0o755 if executable else 0o644)
        swapped: list[tuple[Path, Path | None]] = []
        try:
            for name in updates:
                dest = ROOT / "skills" / name
                backup = staging / f"backup-{name}" if dest.exists() else None
                if backup is not None:
                    dest.rename(backup)
                swapped.append((dest, backup))
                (staging / "new" / name).rename(dest)
            registry_path.write_text(registry_json)
            command("bash", str(ROOT / "scripts" / "skills-lock.sh"), cwd=ROOT)
        except (OSError, ValueError, KeyboardInterrupt):
            try:
                for dest, backup in reversed(swapped):
                    if dest.exists():
                        shutil.rmtree(dest)
                    if backup is not None:
                        backup.rename(dest)
                registry_path.write_bytes(previous_registry)
                if previous_lock is None:
                    lock_path.unlink(missing_ok=True)
                else:
                    lock_path.write_bytes(previous_lock)
            except OSError as error:
                keep_backup = True
                raise ValueError(
                    f"rollback failed; backups retained in {staging}: {error}"
                ) from error
            raise
    finally:
        # A retained backup is always accompanied by a propagated rollback error.
        if not keep_backup:  # pragma: no branch
            shutil.rmtree(staging)


def sync() -> None:
    """Stage all updates before changing the checkout."""
    registry_path = ROOT / "skills-sources.json"
    registry = json.loads(registry_path.read_text())
    if (
        not isinstance(registry, dict)
        or registry.get("version") != 1
        or not isinstance(registry.get("skills"), dict)
    ):
        raise ValueError("invalid skills-sources.json")
    updates: dict[str, dict[str, File]] = {}
    with tempfile.TemporaryDirectory(prefix="nibble-skills-") as temp:
        work = Path(temp)
        for index, (name, source) in enumerate(registry["skills"].items()):
            if not isinstance(source, dict):
                raise TypeError(f"invalid source for {name}")
            if not re.fullmatch(r"[a-z0-9]+(?:-[a-z0-9]+)*", name):
                raise ValueError(f"invalid skill name: {name!r}")
            skill_path = safe_path(source["path"])
            license_path = (
                safe_path(source["license_file"])
                if source.get("license_file")
                else None
            )
            revision = source["revision"]
            if revision and not re.fullmatch(r"[0-9a-f]{40}", revision):
                raise ValueError(f"invalid revision for {name}")
            repo = work / f"repo-{index}"
            command(
                "git",
                "clone",
                "--quiet",
                "--no-checkout",
                "--branch",
                source["branch"],
                "--",
                source["repository"],
                str(repo),
                cwd=work,
            )
            head = command("git", "rev-parse", "HEAD", cwd=repo)
            old = (
                upstream_snapshot(repo, revision, skill_path, license_path)
                if revision
                else {}
            )
            new = upstream_snapshot(repo, head, skill_path, license_path)
            dest = ROOT / "skills" / name
            if dest.is_symlink():
                raise ValueError(f"{name}: destination is a symlink")
            if dest.exists() and (
                not dest.is_dir()
                or (dest / "SKILL.md").is_symlink()
                or not (dest / "SKILL.md").is_file()
            ):
                raise ValueError(f"{name}: destination is missing a regular SKILL.md")
            if revision and not dest.exists():
                updates[name] = new
                source["revision"] = head
                print(f"{name}: restoring missing skill at {head}")
                continue
            if old == new:
                print(f"{name}: up to date")
                continue
            if not revision and dest.exists():
                raise ValueError(
                    f"{name}: cannot import over an existing directory without a pinned base"
                )
            local = snapshot(dest) if dest.exists() else {}
            merged: dict[str, File] = {}
            for path in sorted(old.keys() | local.keys() | new.keys()):
                result = merge_file(
                    f"{name}/{path}",
                    old.get(path),
                    local.get(path),
                    new.get(path),
                    work,
                )
                if result is not None:
                    merged[path] = result
            updates[name] = merged
            source["revision"] = head
            print(f"{name}: {revision or 'new'} -> {head}")
        if updates:
            apply_updates(updates, json.dumps(registry, indent=2) + "\n")
        else:
            # Metadata may have changed even when upstream skill files did not.
            command("bash", str(ROOT / "scripts" / "skills-lock.sh"), cwd=ROOT)


def main() -> int:
    """Run the sync, reporting failures without a traceback."""
    try:
        sync()
    except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
        print(f"skill sync failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
