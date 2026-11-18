# Changelog

## Unreleased

### Added
- `get [--refresh]`, `fetch [--now]` and `status`. `get` prints the cached numbers as the
  contract's JSON (schema 0) and always exits 0; numbers fetched under other credentials are
  never shown. `fetch` calls the usage endpoint through curl when due: the :00/:30 boundaries,
  a 5 s floor, the 429, 401/403, 5xx and other-error backoffs, `--now` within its limits, one
  fetch at a time per config folder, the attempt recorded before the call, a 10 s watchdog on
  curl and a 25 s deadline on the whole fetch. `get --refresh` starts one detached fetch when
  one is due.
- `install [--timer|--no-timer]`, `install --status` and `uninstall [--purge]`, for the user who
  runs them (root refused): `~/.local/bin/claude-usage`, the two user units and a manifest of
  each file's sha256 in `~/.local/share/claude-usage/`. dpkg style: the manifest is written
  first and marked pending, every file atomically, no rollback; a rerun finishes a run that
  stopped. A file at one of its paths that is not its own (another program's, a unit changed
  since the install, a symlink) is refused before anything is written, and kept by uninstall.
  A refused install leaves no folder behind. Its folders must be the user's and writable by
  nobody else: group-writable only for the user's private group (named as the user, no members
  listed) and without an ACL. `HOME` must be the passwd home (the user manager and `%h` use it).
  The timer is enabled through `systemctl --user` (from `/usr/bin` or `/bin`, empty
  environment, 60 s timeout), or by its link when no user manager runs; the `--no-timer`
  choice is kept across reruns and upgrades. Uninstall stops the timer, removes the program,
  waits up to 15 s for running fetches, then removes the units and the manifest; `--purge`
  also removes the state, only when it holds nothing but claude-usage's files. Exit 3 when the
  timer could not be set as asked, or uninstall could not remove a file of its own (the
  manifest then stays, pending, and a rerun finishes).
- `install.sh` (in the release tarball): Linux x86_64 only, not as root, `SHA256SUMS` checked,
  then `claude-usage install`. `scripts/tarball.sh` makes the tarball
  `claude-usage-vX.Y.Z-x86_64-linux.tar.gz`, reproducibly.
- CI (`.github/workflows/ci.yml`): lint, unit, contract and sandbox tests and `cargo audit`
  (pinned), on pushes to `main` and pull requests; read-only token, actions pinned by commit,
  no checkout credentials kept. Mutation tests when started by hand.
- Releases (`.github/workflows/release.yml`, `RELEASING.md`): a `vX.Y.Z` tag on `main` reruns
  every check, then builds the tarball, attests its provenance (`gh attestation verify`) and
  publishes it; 0.x are pre-releases. Only that last job gets write permissions.
  `RELEASE=FILE sandbox/run.sh` installs a downloaded tarball in the sandbox.
- `scripts/key-names.sh`: a one-off for the maintainer that records the endpoint's answer as key
  paths and value types only (the token on curl's stdin, never a value printed).
- Dependencies: `libc`, `serde` (no derive), `serde_json`, `sha2`.
- `systemd/claude-usage.service` and `claude-usage.timer`: a user timer at every :00 and :30
  starting a one-shot `fetch`, with a 30 s start timeout, `LogLevelMax=notice` (routine runs
  leave no journal line; a refusal's warning stays) and the hardening a user manager can apply
  (no new privileges, seccomp `@system-service` without `@privileged`,
  `MemoryDenyWriteExecute`, `UMask=0077`, no namespaces, realtime or SUID, at most 32 tasks
  and 128 MiB).
- `sandbox/run.sh`: a throwaway container booting real systemd, with the real curl against an
  HTTPS stand-in for `api.anthropic.com` on loopback (nginx, HTTP/2, a test CA). Checks the
  request (one Bearer header, the beta header, HTTP/2), a 429's `Retry-After`, no redirect
  followed, `~/.curlrc` and `SSLKEYLOGFILE` ignored (with a control), the timer, the hardening
  as the service's processes get it (a probe against an unhardened unit), readers and the
  timer at one boundary (one call), `get --refresh` starting a fetch or not (new pids), the
  journal, every file checked for the test tokens, and what needs
  root: running as root, credentials, state folder or `usage.json` of another user, a
  read-only or full state folder. It installs from a real tarball with `install.sh` and checks
  root refused, `--timer`/`--no-timer` through systemctl, an upgrade keeping the timer choice,
  kills mid-install, a user without a user manager (the timer starts once linger is on), and
  `uninstall --purge` during a running fetch with readers racing it, leaving exactly the files
  that were there before. The container has no network but loopback, no mounts, fewer
  capabilities; its whole HostConfig is compared before it starts. 58 checks, all
  pass; removes everything it made.
- `scripts/cargo`: builds, tests and lints in a throwaway container (pinned Rust 1.99.1 on
  Alpine, static musl binary). Build modes: your uid, no network, the source read-only, `.git`
  hidden, a fresh `CARGO_HOME`, the crate cache read-only, the toolchain pinned (a
  `rust-toolchain.toml` cannot choose another), `cargo-fmt` and `cargo-clippy` called directly
  (a cargo alias cannot replace them), memory, pids and tmpfs sizes capped, `--init` so Ctrl-C
  stops it. A symlinked, tracked or foreign `./target` is refused. `fetch`,
  `generate-lockfile` and `update` get the network but only the tracked manifests, and return
  only a checked `Cargo.lock`. Setup, `--down` and the network modes lock out other runs; the
  image and volume carry labels and are refused when not made by the script.
- `tests/cargo-wrapper.sh`: replays each of those guarantees against a hostile crate.
- `tests/mutate.sh` and `tests/mutants.txt`: each of 52 safeguards broken once, in a copy
  of the working tree; the check that names it must fail (not merely any check). A safeguard
  of two layers (a file's mode at open and the umask, or the install lock taken twice) is broken in
  both. All 52 caught.
- `tests/cli.sh`: the contract as black-box tests (264 checks in 14 cases: the `get` JSON, the
  state file, the fetch schedule and every backoff, token handling, account switches, config
  and state-folder refusals, curl choice, detaching, `--now`, robustness, install and
  uninstall), against a fake curl
  that checks its own argument list and refuses any token but a test one. Runs only inside a
  container with no network, no real curl and no real home; checks the fake before the cases,
  counts spawned processes by pid, and sweeps every writable folder for the test tokens. All
  pass. One check takes 25 s: the deadline ends a fetch blocked on a full stderr pipe.
- `tests/lint.sh`: rustfmt, clippy, no Debug formatting in `src/` (it could print a credential
  type), bash syntax, shellcheck (the installed one, else a pinned
  container), and the privacy check of every tracked file. The privacy rules also refuse
  Tailscale IPv6 addresses, `/root/` and `C:\Users\` paths, JWTs, long bearer tokens and SSH
  public keys. `tools/install-hooks.sh` installs
  the privacy hooks (pre-commit, commit-msg, pre-push).
