# claude-usage

One shared, cached fetcher for Claude usage limits (the 5-hour and the weekly window).

Status line scripts, dashboards and other tools each used to call the usage endpoint
themselves: more calls (and HTTP 429 sooner), different numbers in each tool, and one copy of
the OAuth token handling per tool. `claude-usage` is the only program that calls the endpoint.
Every tool asks it, and every tool works without it.

**Status: pre-release (0.x).** Every command works; the interface below is a draft
(`schema: 0`) and may still change.

## Interface (draft)

```
claude-usage get [--refresh]     the cached numbers as JSON; never touches the network.
                                 --refresh also starts a detached fetch when one is due.
claude-usage fetch [--now]       fetch if due (run by a systemd user timer at :00 and :30)
claude-usage status              human-readable state
claude-usage install [--timer | --no-timer]   install or upgrade, for you (see Install)
claude-usage install --status                 check the install
claude-usage uninstall [--purge]              remove it (--purge: its state too)
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

## Install

From a release tarball, as yourself (never with `sudo`). First check that GitHub Actions built it
from this repository (`SHA256SUMS` inside the tarball checks only that the files arrived whole):

```
gh attestation verify claude-usage-v<version>-x86_64-linux.tar.gz --repo AnapatAut/Claude-Usage
tar -xzf claude-usage-v<version>-x86_64-linux.tar.gz
claude-usage-v<version>-x86_64-linux/install.sh            # or --no-timer
```

`install.sh` checks the files against `SHA256SUMS`, then runs `claude-usage install`, which
places `~/.local/bin/claude-usage`, the systemd user units `claude-usage.service` and
`claude-usage.timer` in `~/.config/systemd/user/`, and a manifest of what it wrote (with each
file's sha256) in `~/.local/share/claude-usage/`. The timer (every :00 and :30) is enabled unless
you pass `--no-timer`; the choice is kept across reruns and upgrades until `--timer` or
`--no-timer` changes it. Without the timer, tools that call `get --refresh` start fetches while
someone looks.

- Rerun it to upgrade, or to finish an install that stopped part-way (it never rolls back).
- It refuses, changing nothing, when a file at one of its paths is not its own: another
  program's, a unit you edited (keep your changes in a drop-in,
  `~/.config/systemd/user/claude-usage.service.d/`), or a symbolic link. A symlinked
  `~/.local/bin` or `~/.config` (dotfiles) is followed. Its folders must be writable by you
  alone: group-writable only for your private group (named as you, nobody else in it) and
  without an ACL. Run it with your own `HOME` (the one in the passwd file).
- Without a running systemd user manager (no session, or no systemd), the files are installed
  and the timer is enabled by its link; it starts with the manager.
- Without linger (`loginctl enable-linger`), the timer runs only while you are logged in.
- If you set `CLAUDE_CONFIG_DIR`, the user manager needs it too:
  `systemctl --user set-environment CLAUDE_CONFIG_DIR=…`, or a drop-in.
- `claude-usage install --status` checks every file against the manifest and the timer
  (exit 0 healthy, 1 not installed, 4 problems).

`claude-usage uninstall` stops and disables the timer, removes the program, waits up to 15 s
for a running fetch to finish, then removes the units and the manifest. A file changed since
the install, and your drop-ins, are kept and named. The state in `~/.local/state/claude-usage/`
stays unless you pass `--purge`. Claude Code's files are never touched.

Exit codes of `install` and `uninstall`: 0 done, 1 refused (nothing changed), 2 usage, 3 stopped
part-way (run the same command again to finish; for `install` also: the files are in place but
the timer is not as asked).

## Security

- The OAuth token is read from Claude Code's credentials file at fetch time and given to curl
  on its standard input: never on a command line, in the environment, a file or a log. The
  buffers that held it are zeroed before they are freed.
- `claude-usage` never writes, refreshes or copies Claude Code's credentials. `get` only
  `stat`s the file.
- curl is never looked up on `PATH`: `/usr/bin/curl`, `/bin/curl` or `/usr/local/bin/curl` in
  root-owned folders, run with `-q` (no `.curlrc`), an empty environment and no redirects.
  Another curl can be named in `~/.config/claude-usage/config` (`curl = /absolute/path`, in
  folders only you or root can write). That file is read only when `$HOME` is your home in the
  passwd file, so a tool that points `HOME` into a project cannot choose the curl.
- Its own state is owner-only, under `~/.local/state/claude-usage/`, written through folder
  file descriptors (no symlink is followed there).
- Every write (`fetch`, `install`, `uninstall`) refuses to run as root, setuid, or with a
  `$HOME` or credentials file that is not the user's.
- `install` and `uninstall` change only files their manifest records at the recorded hash,
  write each file atomically, and run `systemctl --user` from `/usr/bin` or `/bin` only, with an
  empty environment. The process is not dumpable and makes no core dumps.

## Development

Everything builds and runs in containers; nothing is installed on the host but Docker.

```
scripts/cargo build --release    # static binary in ./target/release/claude-usage
scripts/cargo test
scripts/cargo fetch              # after changing dependencies (the only mode with network)
tests/lint.sh                    # rustfmt, clippy, bash syntax, shellcheck, privacy check
tests/cargo-wrapper.sh           # scripts/cargo's own guarantees, against a hostile crate
tests/cli.sh                     # the contract, black box, against a fake curl (in a container)
sandbox/run.sh                   # real systemd, real curl, an HTTPS stand-in (in a container)
tests/mutate.sh                  # each safeguard broken once: its named check must fail
scripts/tarball.sh               # the release tarball, reproducible, in target/dist/
scripts/key-names.sh [--go]      # once, by the maintainer: the endpoint's key names and types
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

CI (`.github/workflows/`) runs the same scripts. Releases are built there from a tag, never on a
workstation: see [RELEASING.md](RELEASING.md).

## License

MIT, see [LICENSE](LICENSE).
