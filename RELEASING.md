# Releasing

Nothing is pushed, tagged or released without the maintainer's go. A release is built by
GitHub Actions (`.github/workflows/release.yml`) from a tag, never on a workstation.

## Once per repository

- A tag ruleset for `v*`: only the maintainer may create, update or delete such tags; no
  force-push of `main`. GitHub Actions has no way around it.
- Actions settings: workflows get a read-only token by default; Actions may not create or approve
  pull requests; fork pull requests need approval before they run.

## Each release

1. On `main`, the working tree clean (`git status` shows nothing).
2. Every check passes here:
   ```
   tests/lint.sh
   scripts/cargo test --locked
   scripts/cargo build --release --locked && tests/cli.sh
   sandbox/run.sh
   JOBS=1 tests/mutate.sh
   ```
3. The version: `version` in `Cargo.toml`, then `scripts/cargo update --workspace` (only
   claude-usage's own line in `Cargo.lock` changes: check with `git diff Cargo.lock`).
4. `CHANGELOG.md`: rename `## Unreleased` to `## [X.Y.Z] - YYYY-MM-DD` (the release notes are
   that section), and start a new empty `## Unreleased` above it.
5. Commit (`release: vX.Y.Z`), push `main`, and wait for `ci` to pass on that commit.
6. Tag and push the tag:
   ```
   git tag -a vX.Y.Z -m "claude-usage vX.Y.Z"
   git push origin vX.Y.Z
   ```
   `release.yml` runs every check of `ci.yml` again, checks that the tag is `Cargo.toml`'s
   version on `main` with a CHANGELOG section, builds the tarball, attests its provenance and
   publishes the release with it. Versions 0.x are pre-releases.
7. Check what was published, from a new empty folder:
   ```
   gh release download vX.Y.Z --repo AnapatAut/Claude-Usage
   gh attestation verify claude-usage-vX.Y.Z-x86_64-linux.tar.gz --repo AnapatAut/Claude-Usage
   tar -tvzf claude-usage-vX.Y.Z-x86_64-linux.tar.gz   # owner 0/0; 755 claude-usage and install.sh
   ```
   Then, in a checkout of the tag, install it in the sandbox:
   ```
   RELEASE=/path/to/claude-usage-vX.Y.Z-x86_64-linux.tar.gz sandbox/run.sh
   ```
   And `strings` of the binary holds no local user, host or path. The build is reproducible:
   `scripts/tarball.sh` in that checkout should give the same sha256 as the download (if not,
   find out why before announcing the release).
8. A release that fails a check here is withdrawn (`gh release delete vX.Y.Z`, the tag too) and
   the next one gets a new version: a published version is never rebuilt.
