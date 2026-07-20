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
