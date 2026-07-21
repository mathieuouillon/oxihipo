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

1. **Pick the version** `X.Y.Z` (SemVer; pre-1.0 minor bumps may break). Update it
   in all three manifests so they agree — CI's `tag-check` job refuses a tag that
   doesn't match `py/pyproject.toml`:
   - `Cargo.toml` — `[package] version`
   - `py/Cargo.toml` — `[package] version`
   - `py/pyproject.toml` — `[project] version`
2. **Update [`CHANGELOG.md`](CHANGELOG.md)**: move items out of `[Unreleased]` into
   a new `[X.Y.Z]` section with the date, and refresh the compare links at the
   bottom.
3. **Commit** on `main`: `git commit -am "release: vX.Y.Z"` and push.
4. Wait for CI (`ci`, `wheels`, `docs`) to be green on that commit.
5. **Tag and push the tag** — this is what triggers the publish:
   ```sh
   git tag -a vX.Y.Z -m "vX.Y.Z"
   git push github vX.Y.Z
   ```
6. Watch the `wheels` run. `tag-check` → all builds → `release` (publish to PyPI).
   The publish step is **irreversible**: a version can never be re-uploaded or
   overwritten on PyPI, so a mistake means burning the number and shipping
   `X.Y.Z+1`.

## After publishing

- Verify: `pip install oxihipo==X.Y.Z` in a clean venv, then
  `python -c "import oxihipo; print(oxihipo.__version__)"`.
- Optionally create a **GitHub Release** from the tag, pasting the changelog
  section (`gh release create vX.Y.Z --notes-file <(...)`).
- On the **first** release, flip the "Not yet on PyPI" install notes to
  `pip install oxihipo` in `README.md`, `py/README.md`, and
  `website/docs/getting-started/python.md`.
- Start a fresh `[Unreleased]` section in the changelog.

## Notes

- The Rust crate is **not** published to crates.io (this release is PyPI-only). To
  add that later, wire a `cargo publish` job (or crates.io Trusted Publishing) and
  give the root crate the `description` / `license` / `repository` metadata
  crates.io requires.
- Wheels are `abi3` (`abi3-py313`), so one wheel per OS/arch serves every CPython
  ≥ 3.13 — the matrix builds *platforms*, not interpreter versions.
