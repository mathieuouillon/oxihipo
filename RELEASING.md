# Releasing

`oxihipo` ships as Python wheels on [PyPI](https://pypi.org/project/oxihipo/).
A release is cut by pushing a `vX.Y.Z` **git tag**: the [`wheels`](.github/workflows/wheels.yml)
workflow then builds `abi3` wheels for Linux (x86_64/aarch64), macOS
(x86_64/aarch64), and Windows (x64) plus an sdist, and publishes them to PyPI via
**Trusted Publishing** (OIDC — no stored token).

## One-time setup (PyPI Trusted Publishing)

Do this once, before the first release. It authorizes *this repository's workflow*
to upload to the `oxihipo` project without any secret in the repo.

Because `oxihipo` does not exist on PyPI yet, register a **pending publisher**:

1. Sign in to <https://pypi.org> → **Your account → Publishing** (or
   <https://pypi.org/manage/account/publishing/>).
2. Under **Add a new pending publisher**, fill in:
   - **PyPI Project Name**: `oxihipo`
   - **Owner**: `mathieuouillon`
   - **Repository name**: `oxihipo`
   - **Workflow name**: `wheels.yml`
   - **Environment name**: `pypi`
3. Save. (After the first successful upload PyPI converts it to a normal trusted
   publisher — no further action.)
4. In GitHub → **Settings → Environments**, create an environment named `pypi`
   (optionally add protection rules, e.g. required reviewers). The `release` job
   references `environment: pypi`.

> Prefer to dry-run first? Add a second pending publisher on
> [TestPyPI](https://test.pypi.org) and point a scratch workflow at it, or upload
> a build manually with `twine upload --repository testpypi`.

## Cut a release

Use the script. It exists because a release has to keep **six files plus the
changelog** in step, and until it existed only one of those was checked by
anything — 0.2.1 shipped a stale `pypi-v0.1.1` badge in its PyPI description for
exactly that reason.

```sh
scripts/release.py check              # are all the version sites consistent?
scripts/release.py prepare 0.3.0      # rewrite, verify, commit
git push github main                  # or: prepare --push
# wait for ci / wheels / docs to go green on that commit
scripts/release.py tag                # gated on green CI, then publishes
scripts/release.py github-release     # release notes from the changelog
```

`prepare` is reversible — it writes files and makes a local commit. It:

1. refuses unless the tree is clean, you are on `main`, the version is semver,
   the tag does not exist, and **the version is not already on PyPI** (a number
   can never be reused, so this is the check that saves you);
2. rewrites `Cargo.toml`, `py/Cargo.toml`, `py/pyproject.toml`, the static
   `pypi-vX.Y.Z` badge in `py/README.md`, and both `Cargo.lock`s;
3. moves `[Unreleased]` into a dated `[X.Y.Z]` section and fixes the compare
   links (refusing an empty changelog unless you pass `--allow-empty`);
4. runs fmt, clippy, tests, doctests and the docs build;
5. commits as `release: vX.Y.Z`.

`tag` is **not** reversible — pushing the tag publishes. So it re-checks
consistency, requires `HEAD` to equal `github/main` (so the commit CI tested is
the commit being tagged), requires **every workflow green on that exact commit**,
re-checks PyPI, and then asks you to type the version before pushing.

`scripts/release.py check` also runs as the `version-consistency` job on every
PR, so drift is caught long before a release.

### Why the PyPI badge is static

`py/README.md` is the PyPI long description. PyPI **freezes it at upload** and
serves it on every older version's page too, so a dynamic `pypi/v` badge there
can only be right by luck: it reports whatever is newest — wrong on
`/project/oxihipo/<older>/` — and on the project page it lags up to three hours,
because shields.io caches with `max-age=10800` no matter what `cacheSeconds` you
pass. A static badge is exactly right for a frozen page, and `prepare` bumps it
so it cannot be forgotten.

`README.md` and the docs site keep **dynamic** badges: those pages are live, so
"latest" is the correct meaning there.

## After publishing

- Verify: `pip install oxihipo==X.Y.Z` in a clean venv, then
  `python -c "import oxihipo; print(oxihipo.__version__)"`.
- `scripts/release.py github-release` creates the GitHub Release with the
  changelog section as its notes.
- Nothing to do about the changelog or the docs site: `prepare` already opened a
  fresh `[Unreleased]`, and the docs site regenerates its release-notes page from
  `CHANGELOG.md` on every build.

## Notes

- The Rust crate is **not** published to crates.io (this release is PyPI-only). To
  add that later, wire a `cargo publish` job (or crates.io Trusted Publishing) and
  give the root crate the `description` / `license` / `repository` metadata
  crates.io requires.
- Wheels are `abi3` (`abi3-py313`), so one wheel per OS/arch serves every CPython
  ≥ 3.13 — the matrix builds *platforms*, not interpreter versions.
