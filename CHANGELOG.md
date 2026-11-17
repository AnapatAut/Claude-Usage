# Changelog

## Unreleased

### Added
- Repository skeleton: the command line shape (`get`, `fetch`, `status`, `install`,
  `uninstall`), each answering "not implemented" for now.
- `scripts/cargo`: builds, tests and lints in a throwaway container (pinned Rust 1.99.1 on
  Alpine, static musl binary): your uid, no network, the source and `.git` read-only, a fresh
  `CARGO_HOME` each run (only crate downloads persist), `--init` so Ctrl-C stops it, one setup
  at a time. `fetch`, `generate-lockfile` and `update` run with network in a copy of the
  source and return only `Cargo.lock`. Builds land in `./target`.
- `tests/lint.sh`: rustfmt, clippy, bash syntax, shellcheck (the installed one, else a pinned
  container), and the privacy check of every tracked file. `tools/install-hooks.sh` installs
  the privacy hooks (pre-commit, commit-msg, pre-push).
