# capscan-leaderboard

A scheduled job that tracks a curated list of popular crates.io crates over
time and publishes what capability signals (unsafe, FFI, process/network/
filesystem access, build scripts, proc-macros) their latest releases carry
-- and which ones just gained or lost some. Built on
[capscan](https://crates.io/crates/capscan).

**Live site:** https://poglesbyg.github.io/capscan-leaderboard/

## How it works

There's no database. State is the git repo itself:

- [`crates.txt`](crates.txt) -- the curated list of tracked crate names, one
  per line. Add or remove crates by editing this file.
- `data/snapshot.json` -- the last-known `{version, full capscan scan}` for
  every tracked crate, committed after each run.
- `data/history.jsonl` -- append-only log of every version change detected,
  with the capability diff summary (worst severity, signals added/removed,
  dependencies added/removed).
- `docs/index.html` -- a single self-contained static page rendered from
  the two files above, served by GitHub Pages straight from this repo
  (`main` branch, `/docs`).

[`.github/workflows/update.yml`](.github/workflows/update.yml) runs daily
(and on demand via `workflow_dispatch`): resolve each tracked crate's
current latest published version via `capscan::latest_version`, and for any
that moved since last run, scan the new version, diff it against the
stored snapshot, log the result, and update the snapshot. It commits
`data/` and `docs/` back to the repo -- so the diff history *is* the git
history, inspectable with a normal `git log -p data/history.jsonl`.

**It only commits when something real changed.** `docs/index.html` embeds
a generation timestamp, so it differs on every single run even when zero
tracked crates moved -- staging it unconditionally used to mean every
scheduled run (and every push touching `src/**`) produced a commit, purely
from that timestamp. The workflow now checks `git diff -- data/`
specifically, before staging anything, and skips the commit entirely if
the underlying data didn't change; the freshly-regenerated `docs/` in that
run is just discarded. The tradeoff: the page's "last change" date can be
older than today even though the check itself ran today -- that's
intentional, and the page copy says so.

## Getting notified instead of checking back

Two ways to find out about a change without visiting the page:

- **Atom feed**: [`docs/feed.xml`](docs/feed.xml), auto-discoverable by
  feed readers via the `<link rel="alternate">` in the page `<head>`, or
  subscribe directly at
  `https://poglesbyg.github.io/capscan-leaderboard/feed.xml`. One entry
  per recorded version change, same data as the "Recent changes" table.
- **Webhook alert on high severity**: set the `ALERT_WEBHOOK_URL` repo
  secret to a Slack incoming-webhook or Discord webhook URL, and the
  workflow will POST a message whenever a tracked crate's latest release
  introduces a **high** severity capability (unsafe fn/impl, FFI, process
  spawn, build.rs, proc-macro crate, native linkage, `mem::transmute`,
  exported symbol) -- not medium/low, to keep it to things actually worth
  an interruption. Unset by default; nothing else about the run changes
  if you don't configure it. Posts via `curl` (one JSON body with both
  `text` and `content` fields, so either Slack or Discord picks up the
  field it recognizes) rather than adding an HTTP client dependency, same
  reasoning as shelling out to `cargo` elsewhere in the capscan family.

## Run it locally

```
cargo run --release
```

First run scans every crate in `crates.txt` cold (no prior snapshot to
compare against, so nothing shows up under "recent changes" yet -- just
the baseline capability profile). Every run after that only re-scans
crates whose latest version actually changed.

## Why a leaderboard and not just per-project `cargo capscan audit`

`audit` (in [capscan](https://github.com/poglesbyg/capscan) and
[capscan-mcp](https://github.com/poglesbyg/capscan-mcp)) answers "what
would updating *my* dependencies do." This answers a different question:
"across crates everyone depends on, what changed recently, before it ever
shows up in anyone's `cargo update`." Same underlying scanner, different
lens -- one project-scoped and on-demand, one ecosystem-scoped and
continuous.

## Limitations

Same as capscan itself: signal detection is heuristic AST matching, not
real name resolution. A high signal count on a crate isn't a verdict --
`openssl-sys`/`ring`/`libc` are *supposed* to be FFI-heavy; that's the
entire current-profile table's point, to show the normal baseline so a
sudden change against it is more legible. Treat this as a place to start
looking, not a safety rating.
