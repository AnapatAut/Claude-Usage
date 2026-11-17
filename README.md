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

## Security

- The OAuth token is read from Claude Code's credentials file at fetch time and given to curl on
  its standard input: never on a command line, in the environment, a file or a log.
- `claude-usage` never writes, refreshes or copies Claude Code's credentials.
- Its own state is owner-only, under `~/.local/state/claude-usage/`.

## Note on Anthropic's terms

The usage endpoint is not documented by Anthropic and can change or go away without notice;
`claude-usage` then keeps showing the last numbers with an `error`. Claude Code's
[legal and compliance](https://code.claude.com/docs/en/legal-and-compliance) page says OAuth
authentication is designed for ordinary use of Claude Code and Anthropic's own applications.
`claude-usage` reads only your own token on your own machine, makes no model requests, and only
reads your usage. Decide for yourself whether that fits your use.

## License

MIT, see [LICENSE](LICENSE).
