# Changelog

## Unreleased

### Added
- Repository skeleton: the command line shape (`get`, `fetch`, `status`, `install`,
  `uninstall`), each answering "not implemented" for now.
- `scripts/cargo`: builds, tests and lints in a throwaway container (pinned Rust 1.99.1 on
  Alpine, static musl binary), with no network and a read-only source.
- `tests/lint.sh`: rustfmt, clippy, bash syntax, shellcheck, and the privacy check of every
  tracked file. `tools/install-hooks.sh` installs the privacy hooks (pre-commit, commit-msg,
  pre-push).
