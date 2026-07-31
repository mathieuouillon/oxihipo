#!/usr/bin/env python3
"""Cut and publish a release, keeping every place the version appears in step.

A release touches eight things that must agree, and until this script only one of
them was checked by anything (`tag-check`, comparing the tag to
`py/pyproject.toml`). The rest were habits, and habits fail: 0.2.1 shipped with a
stale `pypi-v0.1.1` badge in the PyPI description because that step lives only in
prose.

    scripts/release.py check              # are all eight consistent? (also runs in CI)
    scripts/release.py prepare 0.3.0      # edit + verify + commit the bump
    scripts/release.py tag                # require green CI, then tag -> publish
    scripts/release.py notes 0.3.0        # print a version's changelog section

`prepare` is reversible: it only writes files and makes a local commit. `tag` is
not — pushing a `vX.Y.Z` tag triggers the PyPI publish, and a version number can
never be reused. So `tag` refuses unless CI is already green on the exact commit
being tagged, and asks before pushing.

Standard library only, so it runs anywhere the project builds.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import urllib.error
import urllib.request
from datetime import date, timezone, datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SLUG = "mathieuouillon/oxihipo"
PYPI_PROJECT = "oxihipo"

# Every file carrying the version, and how to find it. `count` is asserted so a
# refactor that adds a second `version =` line fails loudly instead of silently
# updating the wrong one.
VERSION_SITES: list[tuple[str, str, str]] = [
    ("Cargo.toml", r'^version = "(?P<v>[^"]+)"', 'version = "{v}"'),
    ("py/Cargo.toml", r'^version = "(?P<v>[^"]+)"', 'version = "{v}"'),
    ("py/pyproject.toml", r'^version = "(?P<v>[^"]+)"', 'version = "{v}"'),
    # Every version badge is static, and the version is in the URL on purpose.
    # A shields.io badge sits behind its own Cloudflare edge (max-age=10800) and,
    # on GitHub, behind camo as well. A dynamic `pypi/v` URL never changes, so
    # neither cache refetches — ours showed v0.1.1 across four releases. Putting
    # the version in the URL means each release mints a URL no cache has seen.
    # (The PyPI long description has a second reason: PyPI freezes it at upload
    # and serves it on older versions' pages, where "latest" is simply wrong.)
    ("py/README.md", r"badge/pypi-v(?P<v>[0-9][^-]*)-", "badge/pypi-v{v}-"),
    ("README.md", r"badge/pypi-v(?P<v>[0-9][^-]*)-", "badge/pypi-v{v}-"),
    ("website/docs/intro.md", r"badge/pypi-v(?P<v>[0-9][^-]*)-", "badge/pypi-v{v}-"),
    # CITATION.cff carries the released version so a citation names what the
    # user actually ran.
    ("CITATION.cff", r"^version: (?P<v>[0-9][^\s]*)$", "version: {v}"),
    # website/docs/release-notes.md is generated and reads the version straight
    # out of py/pyproject.toml, so it needs no entry here.
]

LOCKFILES = [("Cargo.lock", "oxihipo"), ("py/Cargo.lock", "oxihipo-py")]

SEMVER = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$")


# --------------------------------------------------------------------------- #
# small helpers
# --------------------------------------------------------------------------- #

class Fail(SystemExit):
    def __init__(self, msg: str) -> None:
        super().__init__(f"error: {msg}")


def run(cmd: list[str], *, cwd: Path | None = None, capture: bool = True) -> str:
    r = subprocess.run(
        cmd, cwd=cwd or ROOT, text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.STDOUT if capture else None,
    )
    if r.returncode != 0:
        out = (r.stdout or "").strip()
        raise Fail(f"`{' '.join(cmd)}` failed\n{out}")
    return (r.stdout or "").strip()


def ok(msg: str) -> None:
    print(f"  \033[32m✓\033[0m {msg}")


def warn(msg: str) -> None:
    print(f"  \033[33m!\033[0m {msg}")


def step(msg: str) -> None:
    print(f"\n\033[1m{msg}\033[0m")


def read(rel: str) -> str:
    return (ROOT / rel).read_text()


def write(rel: str, text: str) -> None:
    (ROOT / rel).write_text(text)


def git_remote() -> str:
    """The remote pointing at this project, whatever it is called locally."""
    for line in run(["git", "remote", "-v"]).splitlines():
        name, _, url = line.partition("\t")
        if SLUG in url:
            return name
    raise Fail(f"no git remote points at {SLUG}")


# --------------------------------------------------------------------------- #
# consistency
# --------------------------------------------------------------------------- #

def site_versions() -> dict[str, str]:
    found: dict[str, str] = {}
    for rel, pattern, _ in VERSION_SITES:
        matches = re.findall(pattern, read(rel), re.M)
        if len(matches) != 1:
            raise Fail(
                f"{rel}: expected exactly one version match for /{pattern}/, "
                f"found {len(matches)}. Update VERSION_SITES in this script."
            )
        found[rel] = matches[0]
    return found


def lock_versions() -> dict[str, str | None]:
    out: dict[str, str | None] = {}
    for rel, pkg in LOCKFILES:
        m = re.search(rf'name = "{re.escape(pkg)}"\nversion = "([^"]+)"', read(rel))
        out[rel] = m.group(1) if m else None
    return out


def changelog_section(version: str) -> str | None:
    """The body of one `## [version]` section, or None if absent."""
    text = read("CHANGELOG.md")
    m = re.search(
        rf"^## \[{re.escape(version)}\][^\n]*\n(.*?)(?=^## \[|\Z)",
        text, re.S | re.M,
    )
    return m.group(1).strip() if m else None


def cmd_check(_args: argparse.Namespace) -> int:
    """Assert every version site agrees. Cheap enough to run in CI on each PR."""
    step("Version consistency")
    sites = site_versions()
    version = sites["py/pyproject.toml"]
    bad = {k: v for k, v in sites.items() if v != version}
    for rel, v in sites.items():
        (ok if rel not in bad else warn)(f"{rel:<24} {v}")
    for rel, v in lock_versions().items():
        if v != version:
            bad[rel] = str(v)
            warn(f"{rel:<24} {v}")
        else:
            ok(f"{rel:<24} {v}")

    if bad:
        print()
        raise Fail(
            f"these disagree with py/pyproject.toml ({version}): "
            + ", ".join(f"{k}={v}" for k, v in bad.items())
            + "\nRun: scripts/release.py prepare " + version
        )

    step("Python floor")
    req = re.search(r'^requires-python = ">=([0-9.]+)"', read("py/pyproject.toml"), re.M)
    if not req:
        raise Fail("py/pyproject.toml has no requires-python")
    floor = req.group(1)
    ok(f"requires-python >={floor}")
    mismatched = []
    for rel in ("py/README.md", "README.md", "website/docs/intro.md",
                "website/scripts/sync-changelog.mjs"):
        for badged in re.findall(r"badge/python-([0-9.]+)%2B", read(rel)):
            if badged != floor:
                mismatched.append(f"{rel}={badged}")
            else:
                ok(f"{rel:<34} python-{badged}+")
    if mismatched:
        raise Fail(
            f"python badge disagrees with requires-python (>={floor}): "
            + ", ".join(mismatched)
            + "\nThis is the mismatch that shipped in 0.2.0 (build said 3.10, docs said 3.13)."
        )

    step("Licence")
    # PEP 639 `license-files` globs cannot escape the project directory, so the
    # wheel can only pick up a licence living under py/. It is a SYMLINK to the
    # real one, not a copy: a copy would be free to drift, and a wheel shipping
    # stale licence text is worse than shipping none. It also must not be named
    # LICENSE — maturin already places the repo-root LICENSE at the sdist root,
    # and a second file with that name is a hard "already added" error.
    link = ROOT / "py/LICENSE.txt"
    if not link.is_symlink():
        raise Fail(
            "py/LICENSE.txt must be a symlink to ../LICENSE, not a copy "
            "(a copy can drift). Run: ln -sf ../LICENSE py/LICENSE.txt"
        )
    if link.resolve() != (ROOT / "LICENSE").resolve():
        raise Fail(f"py/LICENSE.txt points at {link.resolve()}, expected {ROOT / 'LICENSE'}")
    ok("py/LICENSE.txt -> LICENSE (symlink, cannot drift)")

    step("PyPI long description")
    # `readme` resolves relative to the manifest directory, and maturin's sdist
    # re-roots py/pyproject.toml to the tarball root — where the *Rust* README
    # sits. A direct build then advertised the Python README and an sdist build
    # the Rust one. Two symlinks under a name neither root already uses make the
    # same file reachable from both.
    for rel, target in (("py/README-pypi.md", ROOT / "py/README.md"),
                        ("README-pypi.md", ROOT / "py/README.md")):
        link = ROOT / rel
        if not link.is_symlink():
            raise Fail(f"{rel} must be a symlink to py/README.md (see py/pyproject.toml `readme`)")
        if link.resolve() != target.resolve():
            raise Fail(f"{rel} points at {link.resolve()}, expected {target}")
        ok(f"{rel} -> py/README.md")

    step("Changelog")
    if changelog_section(version) is None:
        warn(f"no `## [{version}]` section yet — expected before tagging")
    else:
        ok(f"`## [{version}]` section present")
    links = read("CHANGELOG.md")
    if f"[{version}]: https://github.com/{SLUG}/compare/" not in links:
        warn(f"no compare link for {version}")
    else:
        ok("compare link present")

    print(f"\nconsistent at \033[1m{version}\033[0m")
    return 0


# --------------------------------------------------------------------------- #
# external state
# --------------------------------------------------------------------------- #

def pypi_versions() -> set[str]:
    url = f"https://pypi.org/pypi/{PYPI_PROJECT}/json"
    req = urllib.request.Request(url, headers={"User-Agent": "oxihipo-release-script"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return set(json.load(r)["releases"])
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return set()  # not published yet; the first release
        raise Fail(f"could not query PyPI: {e}")
    except OSError as e:
        raise Fail(f"could not reach PyPI ({e}). Refusing to guess.")


def ci_status(sha: str) -> dict[str, str]:
    """Conclusions of the workflow runs for `sha`, keyed by workflow name."""
    raw = run([
        "gh", "run", "list", "--repo", SLUG, "--commit", sha, "--limit", "20",
        "--json", "workflowName,status,conclusion",
    ])
    out: dict[str, str] = {}
    for r in json.loads(raw or "[]"):
        name = r["workflowName"]
        state = r["conclusion"] or r["status"]
        # Keep the worst result if a workflow ran more than once.
        if out.get(name) != "failure":
            out[name] = state
    return out


# --------------------------------------------------------------------------- #
# prepare
# --------------------------------------------------------------------------- #

# Words that mean "a downstream crate can stop compiling". Matched against the
# `[Unreleased]` body, which is where such a change is described if anywhere.
BREAKING_MARKERS = re.compile(
    r"\bbreaking\b|\bsource-breaking\b|\bsemver break\b|\bincompatible\b", re.I
)


def check_breaking_bump(version: str, current: str, allow_patch: bool) -> None:
    """Refuse a patch bump when the changelog says the release breaks callers.

    0.8.0 nearly shipped as 0.7.2. Dropping the `Debug` bound from
    `BankRow::Handles` *relaxes* what an implementor must provide, which reads
    as harmless — but it is source-breaking for a consumer that relied on
    `T::Handles: Debug` in a generic context, so a `^0.7` dependent would have
    broken on a plain `cargo update`. Nothing in the tooling would have caught
    it; a human noticed.

    Below 1.0 the minor is the breaking position (Cargo treats `0.x.y` as
    compatible within `0.x`), so a breaking change must move the minor.
    """
    if allow_patch:
        return
    body = re.search(r"^## \[Unreleased\]\n(.*?)(?=^## \[|\Z)", read("CHANGELOG.md"), re.S | re.M)
    if not body or not BREAKING_MARKERS.search(body.group(1)):
        return
    cur = [int(x) for x in current.split("-")[0].split(".")[:3]]
    new = [int(x) for x in version.split("-")[0].split(".")[:3]]
    breaking_moved = new[0] > cur[0] or (new[0] == cur[0] and new[1] > cur[1])
    if breaking_moved:
        ok(f"changelog says breaking, and {current} -> {version} moves the "
           f"{'major' if new[0] > cur[0] else 'minor'}")
        return
    raise Fail(
        f"the [Unreleased] changelog describes a breaking change, but "
        f"{current} -> {version} is a patch bump. Below 1.0 the minor is the "
        f"breaking position, so `^{cur[0]}.{cur[1]}` dependents would take this "
        f"on a plain `cargo update`. Use {cur[0]}.{cur[1] + 1}.0, or pass "
        f"--allow-patch-breaking if the wording is a false positive."
    )


def bump_changelog(version: str, allow_empty: bool) -> None:
    text = read("CHANGELOG.md")
    m = re.search(r"^## \[Unreleased\]\n(.*?)(?=^## \[|\Z)", text, re.S | re.M)
    if not m:
        raise Fail("CHANGELOG.md has no `## [Unreleased]` section")
    body = m.group(1).strip()
    if body in ("", "Nothing yet.") and not allow_empty:
        raise Fail(
            "nothing under `## [Unreleased]` — a release with an empty changelog "
            "is almost always a mistake. Use --allow-empty if it really is one."
        )
    if body == "Nothing yet.":
        body = "No user-visible changes."

    prev = previous_version(version)
    today = date.today().isoformat()
    replacement = (
        "## [Unreleased]\n\nNothing yet.\n\n"
        f"## [{version}] - {today}\n\n{body}\n\n"
    )
    text = text[: m.start()] + replacement + text[m.end():]

    # Links: retarget Unreleased and insert this version's compare link above it.
    text = re.sub(
        rf"^\[Unreleased\]: https://github\.com/{re.escape(SLUG)}/compare/\S+\.\.\.HEAD$",
        f"[Unreleased]: https://github.com/{SLUG}/compare/v{version}...HEAD\n"
        f"[{version}]: https://github.com/{SLUG}/compare/v{prev}...v{version}",
        text, count=1, flags=re.M,
    )
    write("CHANGELOG.md", text)
    ok(f"CHANGELOG.md: [Unreleased] -> [{version}] - {today} (previous: {prev})")


def previous_version(new: str) -> str:
    """Newest released version in the changelog, for the compare link."""
    for v in re.findall(r"^## \[(\d[^\]]*)\]", read("CHANGELOG.md"), re.M):
        if v != new:
            return v
    raise Fail("could not find a previous version in CHANGELOG.md")


def set_version(version: str) -> None:
    for rel, pattern, template in VERSION_SITES:
        text = read(rel)
        new, n = re.subn(pattern, template.format(v=version), text, count=1, flags=re.M)
        if n != 1:
            raise Fail(f"{rel}: could not rewrite the version")
        write(rel, new)
        ok(f"{rel:<24} -> {version}")


def refresh_locks(version: str) -> None:
    for rel, pkg in LOCKFILES:
        cwd = ROOT / Path(rel).parent
        try:
            run(["cargo", "update", "-p", pkg, "--precise", version], cwd=cwd)
        except Fail:
            # A fresh checkout may not have the package in the lock yet; a plain
            # resolve writes it.
            run(["cargo", "update", "-w"], cwd=cwd)
        ok(f"{rel:<24} refreshed")


def verify_build(skip: bool) -> None:
    if skip:
        warn("build checks skipped (--skip-checks) — CI is then your only gate")
        return
    step("Verifying (this is the slow part)")
    run(["cargo", "fmt", "--check"]); ok("cargo fmt")
    run(["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]); ok("clippy")
    run(["cargo", "test", "--all-targets"]); ok("cargo test")
    run(["cargo", "test", "--doc"]); ok("doctests")
    if (ROOT / "website" / "node_modules").is_dir():
        run(["npm", "run", "build"], cwd=ROOT / "website")
        ok("docs site builds (regenerates release notes from the changelog)")
    else:
        warn("website/node_modules missing — skipping the docs build")


def cmd_prepare(args: argparse.Namespace) -> int:
    version = args.version
    if not SEMVER.match(version):
        raise Fail(f"{version!r} is not a semver X.Y.Z")

    step("Preflight")
    branch = run(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    if branch != "main" and not args.force:
        raise Fail(f"on branch {branch!r}, expected main (--force to override)")
    ok(f"branch {branch}")
    if run(["git", "status", "--porcelain"]):
        raise Fail("working tree is dirty — commit or stash first")
    ok("clean tree")

    current = site_versions()["py/pyproject.toml"]
    if version == current:
        raise Fail(f"already at {version}")
    ok(f"current version {current}")

    if version in pypi_versions():
        raise Fail(
            f"{version} is ALREADY on PyPI. A version can never be reused — "
            f"pick the next one."
        )
    ok(f"{version} is free on PyPI")

    tags = set(run(["git", "tag"]).split())
    if f"v{version}" in tags:
        raise Fail(f"tag v{version} already exists locally")
    ok(f"tag v{version} does not exist")

    check_breaking_bump(version, current, args.allow_patch_breaking)

    step(f"Rewriting version sites -> {version}")
    set_version(version)
    refresh_locks(version)
    bump_changelog(version, args.allow_empty)

    step("Re-checking consistency")
    cmd_check(args)

    verify_build(args.skip_checks)

    step("Committing")
    run(["git", "add", "-A"])
    run(["git", "-c", "commit.gpgsign=false", "commit", "-m", f"release: v{version}"])
    ok(run(["git", "log", "--oneline", "-1"]))

    remote = git_remote()
    if args.push:
        run(["git", "push", remote, branch]); ok(f"pushed to {remote}/{branch}")
        print(f"\nNext: wait for CI, then `scripts/release.py tag`")
    else:
        print(
            f"\nNot pushed. Review with `git show`, then:\n"
            f"  git push {remote} {branch}\n"
            f"  scripts/release.py tag"
        )
    return 0


# --------------------------------------------------------------------------- #
# tag (the irreversible half)
# --------------------------------------------------------------------------- #

def cmd_tag(args: argparse.Namespace) -> int:
    step("Preflight")
    cmd_check(args)
    version = site_versions()["py/pyproject.toml"]
    remote = git_remote()

    if run(["git", "status", "--porcelain"]):
        raise Fail("working tree is dirty")
    ok("clean tree")

    branch = run(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    if branch != "main" and not args.force:
        raise Fail(f"on branch {branch!r}, expected main")

    sha = run(["git", "rev-parse", "HEAD"])
    run(["git", "fetch", "--quiet", remote, branch])
    if sha != run(["git", "rev-parse", f"{remote}/{branch}"]):
        raise Fail(
            f"HEAD is not what {remote}/{branch} points at — push (or pull) first, "
            "so the commit CI tested is the commit being tagged"
        )
    ok(f"HEAD {sha[:8]} matches {remote}/{branch}")

    if changelog_section(version) is None:
        raise Fail(f"CHANGELOG.md has no `## [{version}]` section")

    if version in pypi_versions():
        raise Fail(f"{version} is already on PyPI — nothing to publish")
    ok(f"{version} is free on PyPI")

    step("CI on the exact commit being tagged")
    if args.skip_ci_check:
        warn("CI check skipped (--skip-ci-check)")
    else:
        runs = ci_status(sha)
        if not runs:
            raise Fail(f"no workflow runs found for {sha[:8]} — has CI started?")
        for name, state in sorted(runs.items()):
            (ok if state == "success" else warn)(f"{name:<10} {state}")
        bad = {n: s for n, s in runs.items() if s != "success"}
        if bad:
            raise Fail(
                "CI is not green on this commit: "
                + ", ".join(f"{n}={s}" for n, s in bad.items())
                + "\nThe publish cannot be undone, so this is a hard gate "
                  "(--skip-ci-check to override)."
            )

    step("Ready to publish")
    print(f"  tag        v{version}")
    print(f"  commit     {sha[:8]}")
    print(f"  triggers   wheels -> publish to PyPI (IRREVERSIBLE)")
    if not args.yes:
        if not sys.stdin.isatty():
            raise Fail("not a tty — re-run with --yes to confirm the publish")
        if input(f"\ntype the version to confirm ({version}): ").strip() != version:
            return 1

    body = f"v{version}\n\n{changelog_section(version)}"
    run(["git", "tag", "-a", f"v{version}", "-m", body])
    run(["git", "push", remote, f"v{version}"])
    ok(f"pushed tag v{version} — the wheels workflow will publish")

    print(
        f"\nWatch:   gh run watch --repo {SLUG} "
        f"$(gh run list --repo {SLUG} --limit 1 --json databaseId -q '.[0].databaseId')"
        f"\nThen:    scripts/release.py github-release"
    )
    return 0


def cmd_github_release(args: argparse.Namespace) -> int:
    version = site_versions()["py/pyproject.toml"]
    section = changelog_section(version)
    if section is None:
        raise Fail(f"no changelog section for {version}")
    notes = (
        f"Install: `pip install {PYPI_PROJECT}=={version}`\n\n{section}\n"
    )
    (ROOT / ".release-notes.tmp").write_text(notes)
    try:
        run([
            "gh", "release", "create", f"v{version}", "--repo", SLUG,
            "--title", f"v{version}", "--verify-tag",
            "--notes-file", str(ROOT / ".release-notes.tmp"),
        ])
        ok(f"created GitHub release v{version}")
    finally:
        (ROOT / ".release-notes.tmp").unlink(missing_ok=True)
    return 0


def cmd_notes(args: argparse.Namespace) -> int:
    version = args.version or site_versions()["py/pyproject.toml"]
    section = changelog_section(version)
    if section is None:
        raise Fail(f"no `## [{version}]` section in CHANGELOG.md")
    print(section)
    return 0


# --------------------------------------------------------------------------- #

def main() -> int:
    p = argparse.ArgumentParser(
        prog="scripts/release.py", description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    sub = p.add_subparsers(dest="cmd", required=True)

    c = sub.add_parser("check", help="assert every version site agrees")
    c.set_defaults(func=cmd_check)

    pr = sub.add_parser("prepare", help="bump, verify and commit a new version")
    pr.add_argument("version")
    pr.add_argument("--push", action="store_true", help="also push the commit")
    pr.add_argument("--skip-checks", action="store_true", help="skip fmt/clippy/test")
    pr.add_argument("--allow-empty", action="store_true",
                    help="allow a release with an empty [Unreleased]")
    pr.add_argument("--force", action="store_true", help="allow a non-main branch")
    pr.add_argument("--allow-patch-breaking", action="store_true",
                    help="ship a patch bump even though the changelog says breaking")
    pr.set_defaults(func=cmd_prepare)

    t = sub.add_parser("tag", help="tag and push, triggering the PyPI publish")
    t.add_argument("--yes", action="store_true", help="skip the confirmation prompt")
    t.add_argument("--skip-ci-check", action="store_true",
                   help="tag even if CI is not green (dangerous)")
    t.add_argument("--force", action="store_true", help="allow a non-main branch")
    t.set_defaults(func=cmd_tag)

    g = sub.add_parser("github-release", help="create the GitHub release from the changelog")
    g.set_defaults(func=cmd_github_release)

    n = sub.add_parser("notes", help="print a version's changelog section")
    n.add_argument("version", nargs="?")
    n.set_defaults(func=cmd_notes)

    args = p.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
