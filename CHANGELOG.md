# Changelog

## Unreleased

### Added
- Repository skeleton: the command line shape (`get`, `fetch`, `status`, `install`,
  `uninstall`), each answering "not implemented" for now.
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
- `tests/cli.sh`: the contract as black-box tests (175 checks in 13 cases: the `get` JSON, the
  state file, the fetch schedule and every backoff, token handling, account switches, config
  and state-folder refusals, curl choice, detaching, `--now`, robustness), against a fake curl
  that checks its own argument list and refuses any token but a test one. Runs only inside a
  container with no network, no real curl and no real home; checks the fake before the cases,
  counts spawned processes by pid, and sweeps every writable folder for the test tokens. The
  binary does not pass them yet.
- `tests/lint.sh`: rustfmt, clippy, bash syntax, shellcheck (the installed one, else a pinned
  container), and the privacy check of every tracked file. The privacy rules also refuse
  Tailscale IPv6 addresses, `/root/` and `C:\Users\` paths, JWTs, long bearer tokens and SSH
  public keys. `tools/install-hooks.sh` installs
  the privacy hooks (pre-commit, commit-msg, pre-push).
