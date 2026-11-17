# claude-usage

One shared, cached fetcher for Claude usage limits (the 5-hour and the weekly window).

Status line scripts, dashboards and other tools each used to call the usage endpoint
themselves: more calls (and HTTP 429 sooner), different numbers in each tool, and one copy of
the OAuth token handling per tool. `claude-usage` is the only program that calls the endpoint.
Every tool asks it, and every tool works without it.

**Status: under construction.** Nothing here fetches yet; every command answers
"not implemented". The interface below is a draft (`schema: 0`) and may still change.

## Interface (draft)

```
claude-usage get [--refresh]     the cached numbers as JSON; never touches the network.
                                 --refresh also starts a detached fetch when one is due.
claude-usage fetch [--now]       fetch if due (run by a systemd user timer at :00 and :30)
claude-usage status              human-readable state
claude-usage install [--timer | --no-timer] [--status]
claude-usage uninstall [--purge]
```

`claude-usage get` prints:

```json
{
  "schema": 0,
  "fetched_at": 1794893100,
  "error": null,
  "windows": {
    "five_hour": { "pct": 30.4, "resets_at": 1794900300 },
    "seven_day": { "pct": 12,   "resets_at": 1795152300 }
  }
}
```

- `schema` is `0` while the interface is a draft, `1` once it is frozen. Refuse a schema you do
  not know; ignore keys you do not know.
- `fetched_at` and `resets_at` are Unix seconds. A window the account does not report is `null`.
- `error` is `null` when the last attempt worked, else one fixed word (such as `rate_limited`,
  `token_expired`, `account_changed`). Treat an unknown word as a generic error.
- Numbers fetched with other credentials (another account, or before a logout) are never shown.

## Requirements

- Linux on x86_64. macOS and Windows are not supported.
- curl 7.84 or newer in `/usr/bin`, `/bin` or `/usr/local/bin`.
- Logged in to Claude Code with a Claude account (it reads Claude Code's `.credentials.json`;
  API-key setups have no usage limits to show).
- systemd user services for the timer (optional: tools that call `get --refresh` start fetches
  themselves while someone looks).

## Security (design; not built yet)

- The OAuth token will be read from Claude Code's credentials file at fetch time and given to
  curl on its standard input: never on a command line, in the environment, a file or a log.
- `claude-usage` will never write, refresh or copy Claude Code's credentials.
- Its own state will be owner-only, under `~/.local/state/claude-usage/`.

## Development

Everything builds and runs in containers; nothing is installed on the host but Docker.

```
scripts/cargo build --release    # static binary in ./target/release/claude-usage
scripts/cargo test
scripts/cargo fetch              # after changing dependencies (the only mode with network)
tests/lint.sh                    # rustfmt, clippy, bash syntax, shellcheck, privacy check
tests/cargo-wrapper.sh           # scripts/cargo's own guarantees, against a hostile crate
tests/cli.sh                     # the contract, black box, against a fake curl (in a container)
tools/install-hooks.sh           # once per clone: privacy hooks for commit and push
scripts/cargo --down             # remove the build image and the crate cache
```

`scripts/cargo` runs each cargo command in a new container: your uid, no network, the source
read-only and `.git` hidden, a fresh `CARGO_HOME`, the toolchain pinned, memory and processes
capped. `fetch`, `generate-lockfile` and `update` get the network but see only the tracked
`Cargo.toml` and `Cargo.lock` files, and return only `Cargo.lock`. Git dependencies are not
supported. Build output (`./target`) and the crate cache persist: after building code you do
not trust, run `scripts/cargo clean` and `scripts/cargo --down`. See the comment at the top of
`scripts/cargo`.

## License

MIT, see [LICENSE](LICENSE).
