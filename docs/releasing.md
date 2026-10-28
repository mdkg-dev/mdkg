# Releasing mdkg

How a version goes out: the one-time setup, the gates, and the publish
steps in order. Every publish step is the maintainer's to run. Nothing
publishes without a tag the maintainer pushes, or an approval the
maintainer gives.

## Where each channel comes from

| Channel | Built by | Published by | Needs |
|---|---|---|---|
| GitHub Release (archives, `sha256.sum`, installers) | `release.yml` (cargo-dist), on a `v*` tag | the same workflow | nothing beyond `GITHUB_TOKEN` |
| Shell installer | `release.yml` | attached to the GitHub Release | — |
| Homebrew (`mdkg-dev/homebrew-tap`) | `release.yml` | its `publish-homebrew-formula` job | secret `HOMEBREW_TAP_TOKEN` |
| npm `mdkg` | `release.yml` | its `publish-npm` job | secret `NPM_TOKEN` |
| PyPI `mdkg` | `pypi.yml` (maturin), on the same tag | its `publish` job, after approval | the `pypi` environment and PyPI trusted publishing |
| crates.io `mdkg` and its library crates | — | the maintainer, by hand (step 5) | `cargo login` on the maintainer's machine |

Targets: macOS arm64 and x86_64, and Linux x86_64 and arm64, each with
glibc and musl. Windows and the PowerShell installer are not built yet
(see `dist-workspace.toml`), and the macOS binaries aren't notarized.

## One-time setup

1. **Repositories:** `mdkg-dev/mdkg`, and `mdkg-dev/homebrew-tap` (public,
   with a README so it has a `main` branch).
2. **`HOMEBREW_TAP_TOKEN`:** a fine-grained personal access token with
   access to `mdkg-dev/homebrew-tap` only, Contents read and write. Store
   it as an Actions secret on `mdkg-dev/mdkg`.
3. **`NPM_TOKEN`:** an npm granular access token that can publish the
   `mdkg` package and bypasses two-factor authentication, since CI can't
   answer a prompt. Store it as an Actions secret on `mdkg-dev/mdkg`.
4. **PyPI:**
   - On pypi.org, add a trusted publisher to the `mdkg` project: owner
     `mdkg-dev`, repository `mdkg`, workflow `pypi.yml`, environment
     `pypi`. If the project doesn't exist yet, add it as a *pending*
     publisher. No token is stored.
   - On GitHub, create the environment `pypi` on `mdkg-dev/mdkg` with
     yourself as a required reviewer. Every PyPI upload then waits for
     your approval.
5. **crates.io:** run `cargo login` on your machine with a token that can
   publish `mdkg` and its library crates (mdkg-format, mdkg-log,
   mdkg-graph, mdkg-index, mdkg-coord, mdkg-coord-sqlite, mdkg-run): scopes
   `publish-new` and `publish-update`, crate pattern `mdkg*`. Check that
   crates.io lists you as an owner of each crate that already exists; the
   first publish creates the rest.
6. Optionally, protect `main` and require the CI workflow's checks.

## Gates before a release

- [ ] `scripts/check.sh` passes on main, and CI is green.
- [ ] `crates/mdkg-cli/Cargo.toml` has the version being tagged, and
      `CHANGELOG.md` has its section.
- [ ] `mdkg --version` from a release build prints `mdkg <version> (commit …)`.
- [ ] The release workflow's `plan` job passed on the last pull request,
      or `dist plan` passes locally (cargo-dist 0.32.0).
- [ ] `mdkg anchor` has been run and its `.mdkg/anchors/` file committed,
      so the release commit pins the repository's own work log.

## Local preparation

For an offline preparation run, install the pinned cargo-dist version and
a maturin version satisfying `pyproject.toml` beforehand, and populate
the dependency caches. Then, without any login or upload:

```sh
export CARGO_BUILD_JOBS=4 CARGO_NET_OFFLINE=true
cargo build --release --locked --offline -p mdkg
dist plan
dist generate --check
dist build --artifacts=all --target aarch64-apple-darwin
cargo package --workspace --locked --offline
maturin build --release --locked --offline --jobs 4 --out target/wheels
maturin sdist --out target/wheels
```

Choose the dist target matching the host. This checks only that target;
the release matrix still needs its other platform results. Run one Cargo
build at a time. Missing cached registry data is a failed offline gate,
and `cargo package --no-verify` does not replace package verification.
An older maturin may create diagnostic artifacts, but does not qualify
the backend version declared by the project. Install local artifacts in
temporary prefixes and exercise the CLI before publication; a generated
Homebrew formula checked with an emulator still needs real Homebrew
qualification. Registry resolution, remote URLs and attestations need
their own post-publication checks.

Cargo's publication dry run is the separate pre-publication gate below.
It may need registry metadata reads even when build dependencies are
cached, so dependency caches alone do not guarantee an offline run can
pass. An optional `cargo publish --workspace --dry-run --locked --offline`
probe must record that refusal as unqualified; it does not replace the
registry-qualified dry run. Run the latter with the required registry
access before tagging, without `CARGO_NET_OFFLINE=true` inherited from
this preparation shell.

## Publish steps

Run these from a clean checkout of `mdkg-dev/mdkg` at the commit being
released.

1. **Check the crates locally.** This publishes nothing:

   ```sh
   cargo publish --workspace --dry-run --locked
   ```

   Every crate the binary depends on is published with it: crates.io
   refuses a crate whose path dependencies aren't on crates.io. The
   library crates are internal to mdkg, with no stable API. If this step
   fails, fix it before tagging.

2. **Tag the release.** This publishes the GitHub Release, Homebrew and
   npm, and starts the PyPI build:

   ```sh
   git tag -a v0.7.0 -m "mdkg 0.7.0"
   git push origin v0.7.0
   ```

   Watch the `Release` workflow:
   - **plan, build, host:** these build the archives and checksums, then
     create the GitHub Release.
   - **publish-homebrew-formula:** this pushes `Formula/mdkg.rb` to the
     tap.
   - **publish-npm:** this publishes `mdkg@0.7.0`, which becomes npm's
     `latest`.

   If a publish job fails after `host`, rerun the failed jobs from the
   workflow page; once the GitHub Release exists, never re-tag. If a build
   fails before `host`, nothing is published yet:
   1. Cancel the tag's `PyPI` run, which is waiting for approval, so its
      wheels of the bad tag are never uploaded.
   2. Delete the tag: `git push origin :v0.7.0`, then `git tag -d v0.7.0`.
   3. Fix the problem and tag again.

3. **PyPI:** the `PyPI` workflow builds the wheels and the sdist, then
   waits at `publish`. Approve it only after the `Release` workflow has
   published the GitHub Release and npm.

4. **The previous major's users:** retire the old npm versions as the
   release plan says, for example `npm deprecate mdkg@"<0.7.0" "<message>"`.
   It's safe only once the new version is live.

5. **crates.io**, from the tagged checkout:

   ```sh
   cargo publish --workspace --locked
   ```

   Cargo publishes the workspace's publishable crates in dependency order:

   1. `mdkg-format`
   2. `mdkg-log`
   3. `mdkg-graph`
   4. `mdkg-index`
   5. `mdkg-coord`
   6. `mdkg-coord-sqlite`
   7. `mdkg-run`
   8. `mdkg` (the binary)

   The libraries are internal to mdkg, with no stable API: each one's
   description and README say so, and point at the `mdkg` binary. If
   `--workspace` stops part way (a network error), don't rerun it: it may
   refuse the versions already there. Publish each remaining crate in this
   order with `cargo publish -p <crate> --locked`. `cargo search <crate>`
   shows which are up.

6. **Verify each channel on a clean machine or container.** Each one must
   print the new version, then read a graph from the previous release:

   ```sh
   brew install mdkg-dev/tap/mdkg
   npm install --global mdkg
   curl --proto '=https' --tlsv1.2 -LsSf https://github.com/mdkg-dev/mdkg/releases/download/v0.7.0/mdkg-installer.sh | sh
   cargo install mdkg --locked
   pipx install mdkg
   ```

   Then, for each:

   ```sh
   mdkg --version
   cd <a 0.6 graph> && mdkg validate
   ```

   Check a downloaded archive against its checksum file, or against the
   release's `sha256.sum`:

   ```sh
   shasum -a 256 -c mdkg-aarch64-apple-darwin.tar.gz.sha256
   ```

   `gh attestation verify <archive> --repo mdkg-dev/mdkg` checks the
   build's attestation.

## If something goes wrong after publishing

- **A bad build:** publish a fixed patch release. Versions are never
  reused on npm, crates.io or PyPI.
- **npm:** `npm deprecate mdkg@<version> "<why; use the fix>"`.
  Unpublishing is possible only within 72 hours and blocks the version
  number for good.
- **crates.io:** `cargo yank --version <version> mdkg`, and the same for
  each library crate published with it. A yank keeps existing lockfiles
  working.
- **PyPI:** yank the release in the project's settings.
- **Homebrew:** revert the formula commit in `mdkg-dev/homebrew-tap`.
- **GitHub Release:** mark it a pre-release or delete it. The installers
  download from it.

## Changing the pipeline

- **`release.yml` is generated, then pinned.** cargo-dist writes it with
  actions pinned by tag and dist installed by `curl | sh`;
  `scripts/pin-workflows.py` pins every action by commit and installs dist
  with `cargo install cargo-dist --version 0.32.0 --locked`, and
  `scripts/check.sh` fails on anything unpinned. `allow-dirty = ["ci"]` in
  `dist-workspace.toml` keeps `dist generate` from undoing the pins. To
  regenerate (after changing `dist-workspace.toml`, or upgrading
  cargo-dist): drop that line, run `dist generate`, put it back, run
  `scripts/pin-workflows.py`, and commit all three. A new action or tag
  needs a pin: `scripts/pin-workflows.py --resolve` looks each tag's
  commit up (read only); review it, add it to the script's `PINS`, and
  run the script. `pypa/gh-action-pypi-publish@release/v1` is a branch,
  so it's pinned by hand to a reviewed commit; until then `--check` warns.
- **Adding Windows:** add `x86_64-pc-windows-msvc` to `targets` and
  `"powershell"` to `installers`, then run `dist generate`. First make
  `scripts/check.sh` pass on a Windows runner.
