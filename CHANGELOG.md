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
- `tests/lint.sh`: rustfmt, clippy, bash syntax, shellcheck (the installed one, else a pinned
  container), and the privacy check of every tracked file. The privacy rules also refuse
  Tailscale IPv6 addresses, `/root/` and `C:\Users\` paths, JWTs, long bearer tokens and SSH
  public keys. `tools/install-hooks.sh` installs
  the privacy hooks (pre-commit, commit-msg, pre-push).
