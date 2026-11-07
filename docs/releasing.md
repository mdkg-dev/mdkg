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
| npm `mdkg` | `release.yml` (attached to the GitHub Release) | `publish.yml`'s `npm` job, on the same tag, after approval | the `npm` environment and npm trusted publishing |
| PyPI `mdkg` | `pypi.yml` (maturin), on the same tag | its `publish` job, after approval | the `pypi` environment and PyPI trusted publishing |
| crates.io `mdkg` and its library crates | `publish.yml`'s `crates-verify` job, on the same tag | its `crates-io` job, after approval | the `crates-io` environment and crates.io trusted publishing |

No registry token is stored: npm, PyPI and crates.io each exchange the
job's GitHub OIDC token for a short-lived one. Only `HOMEBREW_TAP_TOKEN`
remains a long-lived secret.

Targets: macOS arm64 and x86_64, and Linux x86_64 and arm64, each with
glibc and musl. Windows and the PowerShell installer are not built yet
(see `dist-workspace.toml`), and the macOS binaries aren't notarized.

## One-time setup

1. **Repositories:** `mdkg-dev/mdkg`, and `mdkg-dev/homebrew-tap` (public,
   with a README so it has a `main` branch).
2. **`HOMEBREW_TAP_TOKEN`:** a fine-grained personal access token with
   access to `mdkg-dev/homebrew-tap` only, Contents read and write. Store
   it as an Actions secret on `mdkg-dev/mdkg`.
3. **npm:**
   - On npmjs.com, in the `mdkg` package's settings, add a trusted
     publisher: GitHub Actions, organization `mdkg-dev`, repository
     `mdkg`, workflow `publish.yml`, environment `npm`, with direct
     publishing (`npm publish`) allowed, not only staged publishing.
     Then, under publishing access, require two-factor authentication
     and disallow tokens; trusted publishing still works.
   - On GitHub, create the environment `npm` on `mdkg-dev/mdkg`, as for
     `pypi` below.
   - Delete any `NPM_TOKEN` Actions secret, and revoke every npm token
     that can publish `mdkg`.
4. **PyPI:**
   - On pypi.org, add a trusted publisher to the `mdkg` project: owner
     `mdkg-dev`, repository `mdkg`, workflow `pypi.yml`, environment
     `pypi`. If the project doesn't exist yet, add it as a *pending*
     publisher. No token is stored.
   - On GitHub, create the environment `pypi` on `mdkg-dev/mdkg` with
     yourself as a required reviewer, and deployments limited to tags
     matching `v*`. Every PyPI upload then waits for your approval.
5. **crates.io:** trusted publishing works only for a crate that exists,
   so the first version of a new crate is published by hand (`cargo
   login` with a short-lived token, `cargo publish -p <crate> --locked`,
   then revoke the token).
   - On crates.io, in the settings of all thirteen publishable crates,
     in the order derived by `scripts/publish-crates.py`: mdkg-format,
     mdkg-git, mdkg-graph, mdkg-index, mdkg-log, mdkg-coord,
     mdkg-coord-sqlite, mdkg-ops, mdkg-run, mdkg-wire, mdkg-presence,
     mdkg-serve, mdkg, add a trusted publisher: GitHub, owner `mdkg-dev`,
     repository `mdkg`, workflow `publish.yml`, environment `crates-io`.
   - On GitHub, create the environment `crates-io` on `mdkg-dev/mdkg`, as
     for `pypi`.
   - For 0.7.1, the five new crate names (mdkg-git, mdkg-ops,
     mdkg-presence, mdkg-serve and mdkg-wire) were created as 0.0.1
     placeholders on 2026-11-02. Set up all thirteen trusted publishers
     before approving the 0.7.1 job; no local publication is needed.
   - Until `publish.yml` is on `mdkg-dev/mdkg`'s default branch,
     crates.io shows "Workflow file not found" beside each entry. It only
     means crates.io can't see the file there yet; the entry is saved.
     Push public `main` (below) before the tag, and the warning goes.
   - After all thirteen entries are configured, delete any
     `CARGO_REGISTRY_TOKEN` secret, run `cargo logout` and revoke the
     local API tokens that can publish these crates.
6. Optionally, protect `main` and require the CI workflow's checks
   (`check (ubuntu-24.04)` and `check (macos-15)`). With that rule, a
   commit reaches `main` only once both have passed on it; see "Updating
   public main".
7. **The `gh` login** that pushes the release needs the `workflow`
   scope, since a release commit usually changes `.github/workflows/`:
   `gh auth refresh -h github.com -s workflow`, then check that `gh auth
   status` lists it. Without it GitHub refuses the push.

## Gates before a release

- [ ] `scripts/check.sh` passes on main, and CI is green.
- [ ] `scripts/check.sh --ci-env` passes at the release commit. This is
      the gate before a release candidate: it runs every check on a
      fresh public export of HEAD, in CI's environment rather than
      yours. See "The CI-like local run" below.
- [ ] The Linux leg passes at the release commit: public CI on a
      release branch (see "Updating public main"), or a private Linux
      runner. A macOS run can't stand in for it (see below).
- [ ] `crates/mdkg-cli/Cargo.toml` has the version being tagged, and
      `CHANGELOG.md` has its section.
- [ ] `mdkg --version` from a release build prints `mdkg <version> (commit …)`.
- [ ] The release workflow's `plan` job passed on the last pull request,
      or `dist plan` passes locally (cargo-dist 0.32.0).
- [ ] `mdkg anchor` has been run and its `.mdkg/anchors/` file committed,
      so the release commit pins the repository's own work log.

### The CI-like local run

Tests pass on a developer's machine that fail on CI because of the
environment: a git identity in `~/.gitconfig`, this repository's own
`.mdkg` graph, the shell tools of one platform. `scripts/check.sh
--ci-env` takes them away. It runs in the development repository; the
public tree ships `check.sh` but not the exporter, so there the mode
refuses to start.

- It exports HEAD with `scripts/export-public.sh --init <mdkg>`, so the
  tree is the public one with a fresh graph, and commits it once as a
  new repository. `<mdkg>` is `mdkg-next` or `mdkg` from PATH, or the
  absolute path `--mdkg` names; use a build whose init writes the
  current graph format.
- It runs the export's own `scripts/check.sh` under `env -i`, with:
  - `HOME` a new, empty folder holding only what CI's "Configure git"
    step writes (an identity and `init.defaultBranch`). As on CI, a test
    that moves `HOME` loses that identity;
  - `GIT_CONFIG_NOSYSTEM=1`, and `user.useConfigOnly=true` through
    `GIT_CONFIG_COUNT`: on macOS, git otherwise makes up an identity
    from the account and host name, where Linux's git refuses to commit;
  - `CARGO_TARGET_DIR` a new folder in the export, so nothing compiled
    before is reused;
  - `CARGO_HOME` a new folder as well, linking only your Cargo home's
    download caches (`registry`, `git`) and their lock files. Your
    `config.toml` (a rustc wrapper, build flags, `[env]`), Cargo
    credentials and installed binaries stay out;
  - `CI=true`, and only these from your environment: `RUSTUP_HOME`,
    `PATH`, `USER`, `LOGNAME`, `LANG`, `LC_ALL`, `TERM`,
    `CARGO_BUILD_JOBS`, `NEXTEST_TEST_THREADS` and `RUST_TEST_THREADS`.
    No other variable, credentials included, reaches the tests;
  - the 0.6.0 oracle, as CI installs it: `MDKG_NODE24` (Node 24) and
    `MDKG_TS_CLI` (mdkg@0.6.0's `dist/cli.js`), as absolute paths, or
    check.sh's defaults.
    The mode refuses to start without them, since the write-command
    tests would otherwise skip their 0.6 checks.
- It removes the export when it ends, pass or fail.

It isolates the environment, not the disk. The tests still run as you,
so a test that names an absolute path (in your real home, say) can still
read it, and the toolchain in `RUSTUP_HOME` and the programs on `PATH`
are yours.

It builds everything from scratch, so it takes about as long as CI's
first run. Before it, the cheap `test hygiene` step of every
`scripts/check.sh` run catches the commonest cases by grep: a test file
that commits with no git identity in it, a path to this repository's own
`.mdkg` with no "skipped" guard, and `env`'s GNU-only `-C`, or `env`
running the `command` builtin.

**What it can't check is the operating system.** Code under
`cfg(target_os = "linux")`, Linux's socket errors (`ECONNRESET` where
macOS gives EOF) and GNU tools only show up on Linux. Running clippy for
Linux from macOS (`cargo clippy --target x86_64-unknown-linux-gnu`) isn't
cheap: it needs that target's standard library installed, and the
workspace's C dependencies (`ring`, bundled SQLite in `libsqlite3-sys`,
and `hidapi`, which finds `libudev` through `pkg-config`) need a Linux C
toolchain and sysroot to build. So the Linux leg runs on Linux.

## Local preparation

For an offline preparation run, install the pinned cargo-dist version and
a maturin version satisfying `pyproject.toml` beforehand, and populate
the dependency caches. Work in a **clean exported checkout outside the
active development checkout**: internally, `scripts/export-public.sh <out>`
creates the export described in the release checklist; for a public-source
rehearsal, use a separate clean checkout of the release source. The export
must contain the committed release changes, and have its own clean Git
commit before building, so the binary records that source's commit.
No login, upload or registry access is part of local preparation.

From that exported checkout, explicitly set `CARGO_TARGET_DIR` to its own
`target/`, overriding any user Cargo `build.target-dir` setting. cargo-dist
0.32.0 writes `target/distrib` there even with a target-dir override and may
replace its previous artifacts.
The channel acceptance runner also reads `target/distrib`, `target/package`
and `target/wheels` from this checkout; do not mix artifacts from another
source or redirect just part of the preparation:

```sh
cd <out>
export CARGO_TARGET_DIR="$PWD/target" CARGO_NET_OFFLINE=true
# Keep the machine's CARGO_BUILD_JOBS setting; run one Cargo command at a time.
cargo build --release --locked --offline -p mdkg
target/release/mdkg --version
dist plan
dist generate --check
dist build --artifacts=all --target aarch64-apple-darwin
cargo package --workspace --locked --offline
maturin build --release --locked --offline --jobs "${CARGO_BUILD_JOBS:-4}" --out target/wheels
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

## Updating public main

The first release started `mdkg-dev/mdkg`'s history from an export made
with `--init`. Every later release adds a commit to that history and
keeps the public repository's own `.mdkg` graph: another `--init` export
would start a new graph, with a new identity, beside it.

1. **Export without `--init`**, from the development repository, into a
   new folder. It must exit 0:

   ```sh
   scripts/export-public.sh --rev <release commit> <export>
   ```

2. **Lay it over public `main`** on a release branch, keeping the top
   `.git` and `.mdkg`. The leading `/` anchors both excludes to the top:
   the conformance fixtures under `conformance/` have `.mdkg` folders of
   their own, which must ship. `--delete` removes files the release
   dropped:

   ```sh
   cd <clean clone of mdkg-dev/mdkg>
   git switch main && git pull --ff-only
   git switch -c release-<version>
   rsync -a --delete --exclude /.git --exclude /.mdkg <export>/ ./
   git status                # .mdkg untouched; read every deletion
   git add -A
   git commit -m "mdkg <version>"
   ```

   Commit with the same public identity as the earlier releases (a
   GitHub no-reply address).

3. **Run CI on the branch.** With `main` protected, a commit reaches it
   only after `check (ubuntu-24.04)` and `check (macos-15)` have passed
   on that commit. CI runs on pushes to `main` and on pull requests, so
   start it by hand on the branch. This is the release's Linux gate:

   ```sh
   git push origin release-<version>
   gh workflow run CI --ref release-<version>
   gh run watch               # both checks green
   ```

   The push needs the `workflow` scope on the `gh` login (one-time
   setup, step 7) whenever the release changes `.github/workflows/`.

4. **Fast-forward `main`** to the same commit, with no force:

   ```sh
   git push origin release-<version>:main
   ```

   Then check that `main` is that commit, and that the workflows on it
   (`publish.yml` among them) are the release's.

If CI fails, don't tag. Fix the problem in the development repository,
export again into a new folder, and repeat from step 2 (reset the branch
to `main` first, or make a new one).

## Publish steps

Run these from a clean checkout of `mdkg-dev/mdkg` at the commit being
released. `<version>` is the release's version, as in
`crates/mdkg-cli/Cargo.toml` (`0.7.2`, say).

1. **Check the crates locally.** This publishes nothing. Use a new,
   empty target folder:

   ```sh
   CARGO_TARGET_DIR="$(mktemp -d)" cargo publish --workspace --dry-run --locked
   ```

   A target folder that an earlier dry run used can hold a compiled
   registry crate of the same version (cargo treats a registry version
   as fixed and never rebuilds it), so the dry run would check that old
   copy, not this commit's. The `crates-verify` job starts from an empty
   folder.

   Every crate the binary depends on is published with it: crates.io
   refuses a crate whose path dependencies aren't on crates.io. The
   library crates are internal to mdkg, with no stable API. If this step
   fails, fix it before tagging.

2. **Tag the release,** on the commit public `main` now points at. This
   publishes the GitHub Release and Homebrew,
   starts the PyPI build, and starts the `Publish` workflow, which waits
   for the GitHub Release:

   ```sh
   git tag -a v<version> -m "mdkg <version>"
   git push origin v<version>
   ```

   Watch the `Release` workflow:
   - **plan, build, host:** these build the archives and checksums, then
     create the GitHub Release.
   - **publish-homebrew-formula:** this pushes `Formula/mdkg.rb` to the
     tap.

   If a publish job fails after `host`, rerun the failed jobs from the
   workflow page; once the GitHub Release exists, never re-tag. If a build
   fails before `host`, nothing is published yet:
   1. Cancel the tag's `PyPI` and `Publish` runs, which are waiting, so
      nothing of the bad tag is uploaded.
   2. Delete the tag: `git push origin :v<version>`, then `git tag -d v<version>`.
   3. Fix the problem and tag again.

3. **npm:** once the GitHub Release exists, the `Publish` workflow's
   `npm` job waits for approval in the `npm` environment. Approved, it
   checks that the release's `mdkg-npm-package.tar.gz` is the one the
   tag's `Release` run built, then publishes it with provenance, and
   it becomes npm's `latest` (a pre-release goes to `next`). Check it:
   `npm view mdkg version`. A rerun skips a version already on npm.

4. **PyPI:** the `PyPI` workflow builds the wheels and the sdist, then
   waits at `publish`. Approve it only after the GitHub Release and npm
   are out.

5. **The previous major's users:** retire the old npm versions as the
   release plan says, for example `npm deprecate mdkg@"<0.7.0" "<message>"`.
   It's safe only once the new version is live.

6. **crates.io:** the `Publish` workflow's `crates-verify` job packages
   and verifies every crate (`cargo publish --workspace --dry-run
   --locked`); then its `crates-io` job waits for approval in the
   `crates-io` environment. Approved, it gets a 30-minute token from
   crates.io and runs `scripts/publish-crates.py`, which publishes one
   crate at a time in dependency order:

   1. `mdkg-format`
   2. `mdkg-git`
   3. `mdkg-graph`
   4. `mdkg-index`
   5. `mdkg-log`
   6. `mdkg-coord`
   7. `mdkg-coord-sqlite`
   8. `mdkg-ops`
   9. `mdkg-run`
   10. `mdkg-wire`
   11. `mdkg-presence`
   12. `mdkg-serve`
   13. `mdkg` (the binary)

   The libraries are internal to mdkg, with no stable API: each one's
   description and README say so, and point at the `mdkg` binary. A crate
   whose version is already there is skipped, so if the job stops part
   way, rerun it. When crates.io rate-limits a publish (HTTP 429), the
   script waits until the time crates.io names, if that's within 25
   minutes of the job's start (the token lasts 30); a longer wait fails
   the job with the time to rerun after. `scripts/publish-crates.py
   --plan` prints the order locally and publishes nothing; only
   `--publish` publishes.

7. **Verify each channel on a clean machine or container.** Each one must
   print the new version, then read a graph from the previous release:

   ```sh
   brew install mdkg-dev/tap/mdkg
   npm install --global mdkg
   curl --proto '=https' --tlsv1.2 -LsSf https://github.com/mdkg-dev/mdkg/releases/download/v<version>/mdkg-installer.sh | sh
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

- **`publish.yml` and `pypi.yml` are written by hand,** and pinned by the
  same script. `publish.yml` names the two jobs that can publish; only
  they have `id-token: write`, each behind its environment. The npm
  trusted publisher and each crate's name `publish.yml`, so renaming it
  means changing those entries first. npm publishes from it rather than
  from `release.yml` because cargo-dist 0.32's npm job reads an
  `NPM_TOKEN` secret, with no environment or OIDC option, so
  `dist-workspace.toml` leaves npm out of `publish-jobs`. It still builds
  the npm package, which the GitHub Release carries.
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
