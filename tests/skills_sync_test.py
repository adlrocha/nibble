"""Integration tests for syncing vendored skills from git repositories."""

from __future__ import annotations

import json
import os
import runpy
import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]


def run(*args: str, cwd: Path, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, cwd=cwd, text=True, capture_output=True, check=check)


def commit(repo: Path, message: str) -> str:
    run("git", "add", "-A", cwd=repo)
    run("git", "commit", "-qm", message, cwd=repo)
    return run("git", "rev-parse", "HEAD", cwd=repo).stdout.strip()


@pytest.fixture
def repos(tmp_path: Path) -> tuple[Path, Path, str]:
    upstream = tmp_path / "upstream"
    upstream.mkdir()
    run("git", "init", "-q", "-b", "main", cwd=upstream)
    run("git", "config", "user.name", "Test", cwd=upstream)
    run("git", "config", "user.email", "test@example.com", cwd=upstream)
    skill = upstream / "skills" / "example"
    skill.mkdir(parents=True)
    (skill / "SKILL.md").write_text("first\nmiddle\nlast\n")
    (skill / "helper.sh").write_text("old helper\n")
    os.chmod(skill / "helper.sh", 0o755)
    (upstream / "LICENSE").write_text("license\n")
    revision = commit(upstream, "initial")

    checkout = tmp_path / "nibble"
    (checkout / "scripts").mkdir(parents=True)
    shutil.copy2(ROOT / "scripts" / "skills-sync.py", checkout / "scripts")
    shutil.copy2(ROOT / "scripts" / "skills-lock.sh", checkout / "scripts")
    shutil.copy2(ROOT / "scripts" / "skills-update-pr.sh", checkout / "scripts")
    shutil.copytree(skill, checkout / "skills" / "example")
    shutil.copy2(upstream / "LICENSE", checkout / "skills" / "example" / "LICENSE")
    registry = {
        "version": 1,
        "skills": {
            "example": {
                "repository": upstream.as_uri(),
                "branch": "main",
                "path": "skills/example",
                "revision": revision,
                "license": "MIT",
            }
        },
    }
    (checkout / "skills-sources.json").write_text(json.dumps(registry))
    return upstream, checkout, revision


def sync(checkout: Path) -> subprocess.CompletedProcess[str]:
    return run("python3", "scripts/skills-sync.py", cwd=checkout, check=False)


def test_updates_complete_skill_and_records_revision(
    repos: tuple[Path, Path, str],
) -> None:
    upstream, checkout, old_revision = repos
    skill = upstream / "skills" / "example"
    (skill / "helper.sh").write_text("new helper\n")
    (skill / "extra.txt").write_text("new reference\n")
    revision = commit(upstream, "update reference")
    result = sync(checkout)
    assert result.returncode == 0, result.stderr
    installed = checkout / "skills" / "example"
    assert (installed / "helper.sh").read_text() == "new helper\n"
    assert os.access(installed / "helper.sh", os.X_OK)
    assert (installed / "extra.txt").read_text() == "new reference\n"
    assert (
        json.loads((checkout / "skills-sources.json").read_text())["skills"]["example"][
            "revision"
        ]
        == revision
        != old_revision
    )
    assert (
        run("bash", "scripts/skills-lock.sh", "--check", cwd=checkout).returncode == 0
    )


def test_imports_new_skill_and_license(repos: tuple[Path, Path, str]) -> None:
    upstream, checkout, _ = repos
    shutil.rmtree(checkout / "skills/example")
    registry_path = checkout / "skills-sources.json"
    registry = json.loads(registry_path.read_text())
    registry["skills"]["example"]["revision"] = ""
    registry["skills"]["example"]["license_file"] = "LICENSE"
    registry_path.write_text(json.dumps(registry))
    result = sync(checkout)
    assert result.returncode == 0, result.stderr
    assert (checkout / "skills/example/LICENSE").read_text() == "license\n"
    assert (checkout / "skills/example/SKILL.md").is_file()
    assert (
        json.loads(registry_path.read_text())["skills"]["example"]["revision"]
        == run("git", "rev-parse", "HEAD", cwd=upstream).stdout.strip()
    )


@pytest.mark.parametrize("upstream_changed", [False, True])
def test_restores_missing_pinned_skill(
    repos: tuple[Path, Path, str], upstream_changed: bool
) -> None:
    upstream, checkout, revision = repos
    run("bash", "scripts/skills-lock.sh", cwd=checkout)
    lock_path = checkout / "skills-lock.json"
    previous_lock = lock_path.read_bytes()
    shutil.rmtree(checkout / "skills/example")
    for args in ((), ("--check",)):
        result = run("bash", "scripts/skills-lock.sh", *args, cwd=checkout, check=False)
        assert result.returncode != 0
        assert lock_path.read_bytes() == previous_lock
    if upstream_changed:
        (upstream / "skills/example/helper.sh").write_text("upstream updated\n")
        revision = commit(upstream, "update")
    result = sync(checkout)
    assert result.returncode == 0, result.stderr
    assert (checkout / "skills/example/SKILL.md").read_text() == "first\nmiddle\nlast\n"
    assert (checkout / "skills/example/helper.sh").read_text() == (
        "upstream updated\n" if upstream_changed else "old helper\n"
    )
    assert json.loads(lock_path.read_text())["skills"]["example"]["version"] == revision
    assert (
        run("bash", "scripts/skills-lock.sh", "--check", cwd=checkout).returncode == 0
    )


def test_refuses_partial_skill_without_entrypoint(
    repos: tuple[Path, Path, str],
) -> None:
    _, checkout, _ = repos
    (checkout / "skills/example/SKILL.md").unlink()
    result = sync(checkout)
    assert result.returncode != 0
    assert "SKILL.md" in result.stderr
    assert (checkout / "skills/example/helper.sh").read_text() == "old helper\n"


def test_refuses_symlinked_entrypoint(
    repos: tuple[Path, Path, str], tmp_path: Path
) -> None:
    _, checkout, _ = repos
    skill_file = checkout / "skills/example/SKILL.md"
    skill_file.unlink()
    target = tmp_path / "other-skill.md"
    target.write_text("protected")
    skill_file.symlink_to(target)
    assert sync(checkout).returncode != 0
    assert (
        run("bash", "scripts/skills-lock.sh", cwd=checkout, check=False).returncode != 0
    )
    assert target.read_text() == "protected"


def test_imports_root_skill_without_git_metadata(repos: tuple[Path, Path, str]) -> None:
    upstream, checkout, _ = repos
    (upstream / "SKILL.md").write_text("root skill\n")
    commit(upstream, "add root skill")
    shutil.rmtree(checkout / "skills/example")
    registry_path = checkout / "skills-sources.json"
    registry = json.loads(registry_path.read_text())
    registry["skills"]["example"]["path"] = "."
    registry["skills"]["example"]["revision"] = ""
    registry_path.write_text(json.dumps(registry))
    result = sync(checkout)
    assert result.returncode == 0, result.stderr
    assert (checkout / "skills/example/SKILL.md").read_text() == "root skill\n"
    assert not (checkout / "skills/example/.git").exists()


@pytest.mark.parametrize("registry", [[], {"version": 1, "skills": {"example": []}}])
def test_invalid_registry_reports_error_without_traceback(
    repos: tuple[Path, Path, str], registry: object
) -> None:
    _, checkout, _ = repos
    (checkout / "skills-sources.json").write_text(json.dumps(registry))
    result = sync(checkout)
    assert result.returncode != 0
    assert "skill sync failed" in result.stderr
    assert "Traceback" not in result.stderr
    assert (checkout / "skills/example/SKILL.md").read_text() == "first\nmiddle\nlast\n"


def test_removes_deleted_upstream_files_but_keeps_local_files(
    repos: tuple[Path, Path, str],
) -> None:
    upstream, checkout, _ = repos
    (checkout / "skills/example/local.txt").write_text("local")
    (upstream / "skills/example/helper.sh").unlink()
    commit(upstream, "delete helper")
    result = sync(checkout)
    assert result.returncode == 0, result.stderr
    assert not (checkout / "skills/example/helper.sh").exists()
    assert (checkout / "skills/example/local.txt").read_text() == "local"


def test_sync_refreshes_lock_after_registry_only_edit(
    repos: tuple[Path, Path, str],
) -> None:
    _, checkout, revision = repos
    run("bash", "scripts/skills-lock.sh", cwd=checkout)
    path = checkout / "skills-sources.json"
    registry = json.loads(path.read_text())
    registry["skills"]["example"]["license"] = "Apache-2.0"
    path.write_text(json.dumps(registry))
    result = sync(checkout)
    assert result.returncode == 0, result.stderr
    lock = json.loads((checkout / "skills-lock.json").read_text())
    assert lock["skills"]["example"]["license"] == "Apache-2.0"
    assert lock["skills"]["example"]["version"] == revision
    assert (
        run("bash", "scripts/skills-lock.sh", "--check", cwd=checkout).returncode == 0
    )


@pytest.mark.parametrize(
    "broken", [None, "{,}", "null", '{"version":1,"skills":{"example":null}}']
)
def test_lock_rejects_bad_registry_without_modifying_lock(
    repos: tuple[Path, Path, str], broken: str | None
) -> None:
    _, checkout, _ = repos
    run("bash", "scripts/skills-lock.sh", cwd=checkout)
    lock = checkout / "skills-lock.json"
    before = lock.read_bytes()
    registry_path = checkout / "skills-sources.json"
    if broken is None:
        registry_path.unlink()
    else:
        registry_path.write_text(broken)
    for args in ((), ("--check",)):
        result = run("bash", "scripts/skills-lock.sh", *args, cwd=checkout, check=False)
        assert result.returncode != 0
        assert lock.read_bytes() == before


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("name", "../escape"),
        ("name", "valid\n"),
        ("revision", "d397f7c"),
        ("revision", "A" * 40),
        ("revision", "f" * 39 + "\n"),
        ("path", "../escape"),
        ("path", "foo/../escape"),
        ("path", "/absolute"),
        ("license_file", "../license"),
    ],
)
def test_lock_rejects_registry_values_the_sync_rejects(
    repos: tuple[Path, Path, str], field: str, value: str
) -> None:
    _, checkout, _ = repos
    run("bash", "scripts/skills-lock.sh", cwd=checkout)
    lock_path = checkout / "skills-lock.json"
    before = lock_path.read_bytes()
    path = checkout / "skills-sources.json"
    registry = json.loads(path.read_text())
    if field == "name":
        registry["skills"][value] = registry["skills"].pop("example")
    else:
        registry["skills"]["example"][field] = value
    path.write_text(json.dumps(registry))
    for args in ((), ("--check",)):
        result = run("bash", "scripts/skills-lock.sh", *args, cwd=checkout, check=False)
        assert result.returncode != 0
        assert lock_path.read_bytes() == before
    assert sync(checkout).returncode != 0


def test_ignores_unrelated_upstream_commit(repos: tuple[Path, Path, str]) -> None:
    upstream, checkout, revision = repos
    (upstream / "unrelated").write_text("change")
    commit(upstream, "unrelated")
    result = sync(checkout)
    assert result.returncode == 0, result.stderr
    assert (
        json.loads((checkout / "skills-sources.json").read_text())["skills"]["example"][
            "revision"
        ]
        == revision
    )


def test_preserves_non_overlapping_local_edits(repos: tuple[Path, Path, str]) -> None:
    upstream, checkout, _ = repos
    (checkout / "skills/example/SKILL.md").write_text("local\nmiddle\nlast\n")
    (upstream / "skills/example/SKILL.md").write_text("first\nmiddle\nupstream\n")
    commit(upstream, "upstream change")
    result = sync(checkout)
    assert result.returncode == 0, result.stderr
    assert (
        checkout / "skills/example/SKILL.md"
    ).read_text() == "local\nmiddle\nupstream\n"


def test_conflict_leaves_checkout_untouched(repos: tuple[Path, Path, str]) -> None:
    upstream, checkout, revision = repos
    local = checkout / "skills/example/SKILL.md"
    local.write_text("local\nmiddle\nlast\n")
    (upstream / "skills/example/SKILL.md").write_text("upstream\nmiddle\nlast\n")
    commit(upstream, "conflict")
    result = sync(checkout)
    assert result.returncode != 0
    assert "conflict" in result.stderr.lower()
    assert local.read_text() == "local\nmiddle\nlast\n"
    assert (
        json.loads((checkout / "skills-sources.json").read_text())["skills"]["example"][
            "revision"
        ]
        == revision
    )


@pytest.mark.parametrize("existing_lock", [False, True])
def test_failed_lock_generation_restores_original_files(
    repos: tuple[Path, Path, str], existing_lock: bool
) -> None:
    upstream, checkout, revision = repos
    if existing_lock:
        run("bash", "scripts/skills-lock.sh", cwd=checkout)
    lock_path = checkout / "skills-lock.json"
    previous_lock = lock_path.read_bytes() if existing_lock else None
    (upstream / "skills/example/helper.sh").write_text("upstream update\n")
    commit(upstream, "update")
    (checkout / "scripts/skills-lock.sh").write_text("#!/bin/sh\nexit 1\n")
    result = sync(checkout)
    assert result.returncode != 0
    assert (checkout / "skills/example/helper.sh").read_text() == "old helper\n"
    assert (
        json.loads((checkout / "skills-sources.json").read_text())["skills"]["example"][
            "revision"
        ]
        == revision
    )
    if existing_lock:
        assert lock_path.read_bytes() == previous_lock
    else:
        assert not lock_path.exists()


def test_failed_swap_restores_original_directory(
    repos: tuple[Path, Path, str], monkeypatch: pytest.MonkeyPatch
) -> None:
    _, checkout, _ = repos
    namespace = runpy.run_path(str(ROOT / "scripts/skills-sync.py"))
    apply = namespace["apply_updates"]
    monkeypatch.setitem(apply.__globals__, "ROOT", checkout)
    original_rename = Path.rename

    def fail_install(self: Path, target: Path) -> Path:
        if self.parent.name == "new":
            raise OSError("cannot install replacement")
        return original_rename(self, target)

    monkeypatch.setattr(Path, "rename", fail_install)
    with pytest.raises(OSError, match="cannot install replacement"):
        apply({"example": {"SKILL.md": (b"replacement", False)}}, "{}")
    assert (checkout / "skills/example/SKILL.md").read_text() == "first\nmiddle\nlast\n"


def test_failed_rollback_retains_backups(
    repos: tuple[Path, Path, str], monkeypatch: pytest.MonkeyPatch
) -> None:
    _, checkout, _ = repos
    namespace = runpy.run_path(str(ROOT / "scripts/skills-sync.py"))
    apply = namespace["apply_updates"]
    monkeypatch.setitem(apply.__globals__, "ROOT", checkout)

    def fail_command(*args: str, cwd: Path) -> str:
        raise ValueError("lock failed")

    def fail_remove(path: Path) -> None:
        raise OSError("cannot remove replacement")

    monkeypatch.setitem(apply.__globals__, "command", fail_command)
    with monkeypatch.context() as context:
        context.setattr(shutil, "rmtree", fail_remove)
        with pytest.raises(ValueError, match="backups retained"):
            apply({"example": {"SKILL.md": (b"replacement", False)}}, "{}")
    backups = list((checkout / "skills").glob(".skills-sync-*/backup-example/SKILL.md"))
    assert len(backups) == 1
    assert backups[0].read_text() == "first\nmiddle\nlast\n"
    assert (backups[0].parents[1] / "original-sources.json").is_file()


def test_binary_conflicts_do_not_overwrite_local_changes(
    repos: tuple[Path, Path, str],
) -> None:
    upstream, checkout, _ = repos
    local = checkout / "skills/example/helper.sh"
    local.write_bytes(b"local\x00binary")
    (upstream / "skills/example/helper.sh").write_bytes(b"upstream\x00binary")
    commit(upstream, "binary conflict")
    result = sync(checkout)
    assert result.returncode != 0
    assert local.read_bytes() == b"local\x00binary"


@pytest.mark.parametrize("mode_side", ["local", "upstream", "both"])
def test_merges_independent_binary_content_and_mode_changes(
    repos: tuple[Path, Path, str], mode_side: str
) -> None:
    upstream, checkout, _ = repos
    remote = upstream / "skills/example/helper.sh"
    remote.write_bytes(b"\xfforiginal")
    revision = commit(upstream, "add binary")
    local = checkout / "skills/example/helper.sh"
    local.write_bytes(b"\xfforiginal")
    path = checkout / "skills-sources.json"
    registry = json.loads(path.read_text())
    registry["skills"]["example"]["revision"] = revision
    path.write_text(json.dumps(registry))
    if mode_side == "local":
        local.chmod(0o644)
        remote.write_bytes(b"\xffupdated")
    else:
        remote.chmod(0o644)
        local.write_bytes(b"\xffupdated")
        if mode_side == "both":
            remote.write_bytes(b"\xffupdated")
    commit(upstream, "update binary")
    result = sync(checkout)
    assert result.returncode == 0, result.stderr
    assert local.read_bytes() == b"\xffupdated"
    assert not os.access(local, os.X_OK)


def test_non_utf8_binary_changes_do_not_get_line_merged(
    repos: tuple[Path, Path, str],
) -> None:
    upstream, checkout, _ = repos
    original = b"\xfffirst\nmiddle\nlast\n"
    remote = upstream / "skills/example/helper.sh"
    remote.write_bytes(original)
    revision = commit(upstream, "add binary")
    local = checkout / "skills/example/helper.sh"
    local.write_bytes(original)
    path = checkout / "skills-sources.json"
    registry = json.loads(path.read_text())
    registry["skills"]["example"]["revision"] = revision
    path.write_text(json.dumps(registry))
    local.write_bytes(b"\xfflocal\nmiddle\nlast\n")
    remote.write_bytes(b"\xfffirst\nmiddle\nupstream\n")
    commit(upstream, "change binary")
    result = sync(checkout)
    assert result.returncode != 0
    assert "conflict" in result.stderr.lower()
    assert local.read_bytes() == b"\xfflocal\nmiddle\nlast\n"


def test_missing_source_leaves_vendor_untouched(repos: tuple[Path, Path, str]) -> None:
    upstream, checkout, revision = repos
    shutil.rmtree(upstream)
    result = sync(checkout)
    assert result.returncode != 0
    assert (checkout / "skills/example/SKILL.md").read_text() == "first\nmiddle\nlast\n"
    assert (
        json.loads((checkout / "skills-sources.json").read_text())["skills"]["example"][
            "revision"
        ]
        == revision
    )


def test_locally_triggered_pr_leaves_worktree_untouched(
    repos: tuple[Path, Path, str], tmp_path: Path
) -> None:
    upstream, checkout, _ = repos
    run("bash", "scripts/skills-lock.sh", cwd=checkout)
    run("git", "init", "-q", "-b", "main", cwd=checkout)
    run("git", "config", "user.name", "Test", cwd=checkout)
    run("git", "config", "user.email", "test@example.com", cwd=checkout)
    (checkout / "tests").mkdir()
    (checkout / "tests/skills_sync_test.py").write_text("# fixture\n")
    commit(checkout, "initial nibble")
    remote = tmp_path / "remote.git"
    run("git", "init", "-q", "--bare", str(remote), cwd=tmp_path)
    run("git", "remote", "add", "origin", str(remote), cwd=checkout)
    run("git", "push", "-q", "-u", "origin", "main", cwd=checkout)
    (checkout / "skills/example/SKILL.md").write_text("local work\n")
    (checkout / "untracked").write_text("untouched\n")
    stubs = tmp_path / "bin"
    stubs.mkdir()
    (stubs / "gh").write_text(
        "#!/bin/sh\nif [ \"$1 $2\" = 'pr create' ]; then echo created; fi\n"
    )
    (stubs / "cargo").write_text("#!/bin/sh\nexit 0\n")
    (stubs / "pytest").write_text("#!/bin/sh\nexit 0\n")
    for stub in stubs.iterdir():
        stub.chmod(0o755)
    env = os.environ | {"PATH": f"{stubs}:{os.environ['PATH']}"}
    unchanged = subprocess.run(
        ["bash", "scripts/skills-update-pr.sh"],
        cwd=checkout,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    assert unchanged.returncode == 0, unchanged.stderr
    assert "no PR needed" in unchanged.stdout
    (upstream / "skills/example/helper.sh").write_text("upstream update\n")
    commit(upstream, "update")
    (stubs / "pytest").write_text("#!/bin/sh\nexit 1\n")
    failed_tests = subprocess.run(
        ["bash", "scripts/skills-update-pr.sh"],
        cwd=checkout,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    assert failed_tests.returncode != 0
    assert not run(
        "git", "ls-remote", "--heads", "origin", "sync/skills-*", cwd=checkout
    ).stdout
    (stubs / "pytest").write_text("#!/bin/sh\nexit 0\n")
    (stubs / "cargo").write_text('#!/bin/sh\n[ "$1" != "${FAIL_CARGO:-}" ]\n')
    for cargo_command in ("test", "clippy", "fmt"):
        failed_cargo = subprocess.run(
            ["bash", "scripts/skills-update-pr.sh"],
            cwd=checkout,
            env=env | {"FAIL_CARGO": cargo_command},
            text=True,
            capture_output=True,
            check=False,
        )
        assert failed_cargo.returncode != 0
        assert not run(
            "git", "ls-remote", "--heads", "origin", "sync/skills-*", cwd=checkout
        ).stdout
    result = subprocess.run(
        ["bash", "scripts/skills-update-pr.sh"],
        cwd=checkout,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert "created" in result.stdout
    assert (checkout / "skills/example/SKILL.md").read_text() == "local work\n"
    assert (checkout / "untracked").read_text() == "untouched\n"
    branches = run(
        "git", "ls-remote", "--heads", "origin", "sync/skills-*", cwd=checkout
    ).stdout.splitlines()
    assert len(branches) == 1
    sha = branches[0].split()[0]
    assert (
        run("git", "show", f"{sha}:skills/example/helper.sh", cwd=checkout).stdout
        == "upstream update\n"
    )
    again = subprocess.run(
        ["bash", "scripts/skills-update-pr.sh"],
        cwd=checkout,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    assert again.returncode != 0
    assert "already exists" in again.stderr
    assert (checkout / "skills/example/SKILL.md").read_text() == "local work\n"


def test_pr_command_refreshes_metadata_only_lock(
    repos: tuple[Path, Path, str], tmp_path: Path
) -> None:
    _, checkout, _ = repos
    run("bash", "scripts/skills-lock.sh", cwd=checkout)
    path = checkout / "skills-sources.json"
    registry = json.loads(path.read_text())
    registry["skills"]["example"]["license"] = "Apache-2.0"
    path.write_text(json.dumps(registry))
    run("git", "init", "-q", "-b", "main", cwd=checkout)
    run("git", "config", "user.name", "Test", cwd=checkout)
    run("git", "config", "user.email", "test@example.com", cwd=checkout)
    (checkout / "tests").mkdir()
    (checkout / "tests/skills_sync_test.py").write_text("# fixture\n")
    commit(checkout, "registry metadata with stale lock")
    remote = tmp_path / "remote.git"
    run("git", "init", "-q", "--bare", str(remote), cwd=tmp_path)
    run("git", "remote", "add", "origin", str(remote), cwd=checkout)
    run("git", "push", "-q", "-u", "origin", "main", cwd=checkout)
    stubs = tmp_path / "bin"
    stubs.mkdir()
    for name in ("gh", "cargo", "pytest"):
        stub = stubs / name
        stub.write_text("#!/bin/sh\nexit 0\n")
        stub.chmod(0o755)
    result = subprocess.run(
        ["bash", "scripts/skills-update-pr.sh"],
        cwd=checkout,
        env=os.environ | {"PATH": f"{stubs}:{os.environ['PATH']}"},
        text=True,
        capture_output=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    branch = run(
        "git", "ls-remote", "--heads", "origin", "sync/skills-*", cwd=checkout
    ).stdout.split()[0]
    lock = json.loads(
        run("git", "show", f"{branch}:skills-lock.json", cwd=checkout).stdout
    )
    assert lock["skills"]["example"]["license"] == "Apache-2.0"


def test_pr_command_reports_missing_dependency(
    repos: tuple[Path, Path, str], tmp_path: Path
) -> None:
    _, checkout, _ = repos
    bin_dir = tmp_path / "limited-bin"
    bin_dir.mkdir()
    for name in ("git", "gh", "python3"):
        command_path = shutil.which(name)
        assert command_path is not None
        (bin_dir / name).symlink_to(command_path)
    result = subprocess.run(
        ["/bin/bash", "scripts/skills-update-pr.sh"],
        cwd=checkout,
        env=os.environ | {"PATH": str(bin_dir)},
        text=True,
        capture_output=True,
        check=False,
    )
    assert result.returncode != 0
    assert "pytest is required" in result.stderr


def test_pr_command_stops_if_github_auth_fails(
    repos: tuple[Path, Path, str], tmp_path: Path
) -> None:
    _, checkout, _ = repos
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (bin_dir / "gh").write_text("#!/bin/sh\nexit 1\n")
    (bin_dir / "gh").chmod(0o755)
    result = subprocess.run(
        ["bash", "scripts/skills-update-pr.sh"],
        cwd=checkout,
        env=os.environ | {"PATH": f"{bin_dir}:{os.environ['PATH']}"},
        text=True,
        capture_output=True,
        check=False,
    )
    assert result.returncode != 0
    assert not (checkout / ".git").exists()


def test_rejects_symlinked_upstream_source(repos: tuple[Path, Path, str]) -> None:
    upstream, checkout, _ = repos
    (upstream / "outside").symlink_to("skills", target_is_directory=True)
    revision = commit(upstream, "symlink")
    registry_file = checkout / "skills-sources.json"
    registry = json.loads(registry_file.read_text())
    registry["skills"]["example"]["path"] = "outside/example"
    registry["skills"]["example"]["revision"] = revision
    registry_file.write_text(json.dumps(registry))
    result = sync(checkout)
    assert result.returncode != 0
    assert "symlink" in result.stderr.lower()


@pytest.mark.parametrize(
    ("field", "value", "message"),
    [("revision", "not-a-sha", "revision"), ("path", "../escape", "path")],
)
def test_rejects_invalid_source_metadata(
    repos: tuple[Path, Path, str], field: str, value: str, message: str
) -> None:
    _, checkout, _ = repos
    registry_file = checkout / "skills-sources.json"
    registry = json.loads(registry_file.read_text())
    registry["skills"]["example"][field] = value
    registry_file.write_text(json.dumps(registry))
    result = sync(checkout)
    assert result.returncode != 0
    assert message in result.stderr.lower()


def test_rejects_unpinned_import_over_existing_skill(
    repos: tuple[Path, Path, str],
) -> None:
    _, checkout, _ = repos
    registry_file = checkout / "skills-sources.json"
    registry = json.loads(registry_file.read_text())
    registry["skills"]["example"]["revision"] = ""
    registry_file.write_text(json.dumps(registry))
    result = sync(checkout)
    assert result.returncode != 0
    assert "existing directory" in result.stderr.lower()
    assert (checkout / "skills/example/SKILL.md").read_text() == "first\nmiddle\nlast\n"


def test_rejects_deleted_skill_entrypoint(repos: tuple[Path, Path, str]) -> None:
    upstream, checkout, _ = repos
    (upstream / "skills/example/SKILL.md").unlink()
    commit(upstream, "remove entrypoint")
    result = sync(checkout)
    assert result.returncode != 0
    assert "no skill.md" in result.stderr.lower()
    assert (checkout / "skills/example/SKILL.md").is_file()


def test_rejects_delete_modify_conflict(repos: tuple[Path, Path, str]) -> None:
    upstream, checkout, _ = repos
    local = checkout / "skills/example/helper.sh"
    local.write_text("local edit")
    (upstream / "skills/example/helper.sh").unlink()
    commit(upstream, "remove helper")
    result = sync(checkout)
    assert result.returncode != 0
    assert "conflict" in result.stderr.lower()
    assert local.read_text() == "local edit"


def test_rejects_missing_license(repos: tuple[Path, Path, str]) -> None:
    _, checkout, _ = repos
    registry_file = checkout / "skills-sources.json"
    registry = json.loads(registry_file.read_text())
    registry["skills"]["example"]["license_file"] = "MISSING"
    registry_file.write_text(json.dumps(registry))
    result = sync(checkout)
    assert result.returncode != 0
    assert "license file missing" in result.stderr.lower()


@pytest.mark.parametrize("upstream_changed", [False, True])
def test_rejects_local_symlink_without_modifying_target(
    repos: tuple[Path, Path, str], tmp_path: Path, upstream_changed: bool
) -> None:
    upstream, checkout, _ = repos
    if upstream_changed:
        (upstream / "skills/example/helper.sh").write_text("upstream update")
        commit(upstream, "update")
    target = tmp_path / "target"
    target.mkdir()
    (target / "SKILL.md").write_text("protected")
    shutil.rmtree(checkout / "skills/example")
    (checkout / "skills/example").symlink_to(target, target_is_directory=True)
    result = sync(checkout)
    assert result.returncode != 0
    assert "destination is a symlink" in result.stderr.lower()
    assert (
        run("bash", "scripts/skills-lock.sh", cwd=checkout, check=False).returncode != 0
    )
    assert (target / "SKILL.md").read_text() == "protected"


def test_rejects_missing_upstream_skill_directory(
    repos: tuple[Path, Path, str],
) -> None:
    _, checkout, _ = repos
    path = checkout / "skills-sources.json"
    registry = json.loads(path.read_text())
    registry["skills"]["example"]["path"] = "skills/missing"
    path.write_text(json.dumps(registry))
    result = sync(checkout)
    assert result.returncode != 0
    assert "skill directory missing" in result.stderr


@pytest.mark.parametrize("kind", ["symlink", "fifo"])
def test_rejects_unsupported_local_files(
    repos: tuple[Path, Path, str], tmp_path: Path, kind: str
) -> None:
    upstream, checkout, _ = repos
    (upstream / "skills/example/helper.sh").write_text("new\n")
    commit(upstream, "update")
    target = checkout / "skills/example/local"
    if kind == "symlink":
        target.symlink_to(tmp_path / "other")
    else:
        os.mkfifo(target)
    result = sync(checkout)
    assert result.returncode != 0
    assert ("symlink" if kind == "symlink" else "unsupported") in result.stderr


def test_rejects_conflicting_upstream_licenses(repos: tuple[Path, Path, str]) -> None:
    upstream, checkout, _ = repos
    (upstream / "skills/example/LICENSE").write_text("different license")
    commit(upstream, "add license")
    path = checkout / "skills-sources.json"
    registry = json.loads(path.read_text())
    registry["skills"]["example"]["license_file"] = "LICENSE"
    path.write_text(json.dumps(registry))
    result = sync(checkout)
    assert result.returncode != 0
    assert "conflicts" in result.stderr


def test_rejects_invalid_skill_name(repos: tuple[Path, Path, str]) -> None:
    _, checkout, _ = repos
    path = checkout / "skills-sources.json"
    registry = json.loads(path.read_text())
    registry["skills"]["../escape"] = registry["skills"].pop("example")
    path.write_text(json.dumps(registry))
    result = sync(checkout)
    assert result.returncode != 0
    assert "invalid skill name" in result.stderr


def test_refuses_to_leave_skill_without_entrypoint(
    repos: tuple[Path, Path, str],
) -> None:
    upstream, checkout, _ = repos
    (checkout / "skills/example/SKILL.md").unlink()
    (upstream / "skills/example/helper.sh").write_text("new\n")
    commit(upstream, "update")
    result = sync(checkout)
    assert result.returncode != 0
    assert "SKILL.md" in result.stderr


def test_failed_new_import_is_rolled_back(repos: tuple[Path, Path, str]) -> None:
    _, checkout, _ = repos
    shutil.rmtree(checkout / "skills/example")
    path = checkout / "skills-sources.json"
    registry = json.loads(path.read_text())
    registry["skills"]["example"]["revision"] = ""
    path.write_text(json.dumps(registry))
    (checkout / "scripts/skills-lock.sh").write_text("#!/bin/sh\nexit 1\n")
    result = sync(checkout)
    assert result.returncode != 0
    assert not (checkout / "skills/example").exists()
    assert json.loads(path.read_text())["skills"]["example"]["revision"] == ""


def test_rejects_unsafe_source_path(repos: tuple[Path, Path, str]) -> None:
    _, checkout, _ = repos
    registry_file = checkout / "skills-sources.json"
    registry = json.loads(registry_file.read_text())
    registry["skills"]["example"]["path"] = "../../outside"
    registry_file.write_text(json.dumps(registry))
    result = sync(checkout)
    assert result.returncode != 0
    assert "path" in result.stderr.lower()
