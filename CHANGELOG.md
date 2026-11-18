# Changelog

## Unreleased

### Added
- `get [--refresh]`, `fetch [--now]` and `status`. `get` prints the cached numbers as the
  contract's JSON (schema 0) and always exits 0; numbers fetched under other credentials are
  never shown. `fetch` calls the usage endpoint through curl when due: the :00/:30 boundaries,
  a 5 s floor, the 429, 401/403, 5xx and other-error backoffs, `--now` within its limits, one
  fetch at a time per config folder, the attempt recorded before the call, a 10 s watchdog on
  curl and a 25 s deadline on the whole fetch. `get --refresh` starts one detached fetch when
  one is due. `install` and `uninstall` still answer "not implemented".
- Dependencies: `libc`, `serde` (no derive), `serde_json`, `sha2`.
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
- `tests/cli.sh`: the contract as black-box tests (204 checks in 13 cases: the `get` JSON, the
  state file, the fetch schedule and every backoff, token handling, account switches, config
  and state-folder refusals, curl choice, detaching, `--now`, robustness), against a fake curl
  that checks its own argument list and refuses any token but a test one. Runs only inside a
  container with no network, no real curl and no real home; checks the fake before the cases,
  counts spawned processes by pid, and sweeps every writable folder for the test tokens. All
  pass.
- `tests/lint.sh`: rustfmt, clippy, no Debug formatting in `src/` (it could print a credential
  type), bash syntax, shellcheck (the installed one, else a pinned
  container), and the privacy check of every tracked file. The privacy rules also refuse
  Tailscale IPv6 addresses, `/root/` and `C:\Users\` paths, JWTs, long bearer tokens and SSH
  public keys. `tools/install-hooks.sh` installs
  the privacy hooks (pre-commit, commit-msg, pre-push).
