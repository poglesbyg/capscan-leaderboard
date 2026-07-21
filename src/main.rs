//! Scheduled job: track a curated list of popular crates.io crates over
//! time, and publish which ones gained (or lost) capability signals on
//! their latest release. State lives in git (`data/snapshot.json` +
//! `data/history.jsonl`), committed by the CI workflow after each run --
//! no external database needed. Output is a single self-contained static
//! page at `docs/index.html`, served by GitHub Pages.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use capscan::{diff_reports, latest_version, locate_or_fetch, scan_dir, CrateReport, Severity};
use serde::{Deserialize, Serialize};

const CRATES_LIST: &str = "crates.txt";
const SNAPSHOT_PATH: &str = "data/snapshot.json";
const HISTORY_PATH: &str = "data/history.jsonl";
const SITE_PATH: &str = "docs/index.html";
const FEED_PATH: &str = "docs/feed.xml";
const HISTORY_DISPLAY_LIMIT: usize = 100;
const FEED_ENTRY_LIMIT: usize = 50;
const ALERT_WEBHOOK_ENV: &str = "ALERT_WEBHOOK_URL";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CrateSnapshot {
    version: String,
    report: CrateReport,
    last_checked: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryEntry {
    name: String,
    old_version: String,
    new_version: String,
    checked_at: String,
    worst_severity: Option<String>,
    added_signals: usize,
    removed_signals: usize,
    added_dependencies: Vec<String>,
    removed_dependencies: Vec<String>,
}

fn read_crate_list(path: &str) -> Result<Vec<String>> {
    let content = fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect())
}

fn load_snapshot(path: &str) -> Result<BTreeMap<String, CrateSnapshot>> {
    if !Path::new(path).exists() {
        return Ok(BTreeMap::new());
    }
    let content = fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    serde_json::from_str(&content).with_context(|| format!("parsing {path}"))
}

fn save_snapshot(path: &str, snapshot: &BTreeMap<String, CrateSnapshot>) -> Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(snapshot)?)?;
    Ok(())
}

fn append_history(path: &str, entries: &[HistoryEntry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)?;
    }
    let mut existing = if Path::new(path).exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };
    for entry in entries {
        existing.push_str(&serde_json::to_string(entry)?);
        existing.push('\n');
    }
    fs::write(path, existing)?;
    Ok(())
}

fn load_history(path: &str) -> Result<Vec<HistoryEntry>> {
    if !Path::new(path).exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path)?;
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("parsing a history.jsonl line"))
        .collect()
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Drop any snapshot entry for a name no longer in `tracked_names` --
/// without this, removing a crate from the tracked list would leave it in
/// `data/snapshot.json` (and so in the published profile table) forever,
/// since the main loop only ever inserts, never removes. `history.jsonl`
/// is untouched by this: past entries for a since-removed crate are still
/// an accurate historical record.
fn prune_untracked(snapshot: &mut BTreeMap<String, CrateSnapshot>, tracked_names: &[String]) {
    let tracked: std::collections::BTreeSet<&str> =
        tracked_names.iter().map(String::as_str).collect();
    snapshot.retain(|name, _| tracked.contains(name.as_str()));
}

fn severity_counts(report: &CrateReport) -> (usize, usize, usize) {
    let (mut high, mut medium, mut low) = (0, 0, 0);
    for s in &report.signals {
        match s.kind.severity() {
            Severity::High => high += 1,
            Severity::Medium => medium += 1,
            Severity::Low => low += 1,
        }
    }
    (high, medium, low)
}

fn main() -> Result<()> {
    let names = read_crate_list(CRATES_LIST)?;
    let mut snapshot = load_snapshot(SNAPSHOT_PATH)?;
    let mut new_history_entries = Vec::new();

    for name in &names {
        print!("checking {name}... ");

        let latest = match latest_version(name) {
            Ok(Some(v)) => v,
            Ok(None) => {
                println!("could not resolve latest version, skipping");
                continue;
            }
            Err(e) => {
                println!("error resolving latest version: {e}");
                continue;
            }
        };

        let existing = snapshot.get(name).cloned();
        let needs_scan = existing.as_ref().is_none_or(|s| s.version != latest);
        if !needs_scan {
            println!("unchanged at {latest}");
            continue;
        }

        let scanned = (|| -> Result<CrateReport> {
            let path = locate_or_fetch(name, &latest)?;
            scan_dir(name, &latest, &path)
        })();
        let new_report = match scanned {
            Ok(r) => r,
            Err(e) => {
                println!("error scanning {name} {latest}: {e}");
                continue;
            }
        };

        match existing {
            Some(old) => {
                println!("updated {} -> {latest}", old.version);
                let diff = diff_reports(&old.report, &new_report);
                new_history_entries.push(HistoryEntry {
                    name: name.clone(),
                    old_version: old.version,
                    new_version: latest.clone(),
                    checked_at: now_iso(),
                    worst_severity: diff.worst_severity().map(|s| s.to_string()),
                    added_signals: diff.added.len(),
                    removed_signals: diff.removed.len(),
                    added_dependencies: diff.added_dependencies,
                    removed_dependencies: diff.removed_dependencies,
                });
            }
            None => println!("new: {latest}"),
        }

        snapshot.insert(
            name.clone(),
            CrateSnapshot {
                version: latest,
                report: new_report,
                last_checked: now_iso(),
            },
        );
    }

    prune_untracked(&mut snapshot, &names);

    save_snapshot(SNAPSHOT_PATH, &snapshot)?;
    append_history(HISTORY_PATH, &new_history_entries)?;

    let history = load_history(HISTORY_PATH)?;
    let html = render_html(&snapshot, &history);
    if let Some(parent) = Path::new(SITE_PATH).parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(SITE_PATH, html)?;

    let feed = render_atom_feed(&history);
    fs::write(FEED_PATH, feed)?;

    send_high_severity_alerts(&new_history_entries);

    println!(
        "\n{} crates tracked, {} new change(s) this run, site written to {SITE_PATH}",
        snapshot.len(),
        new_history_entries.len()
    );
    Ok(())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn render_html(snapshot: &BTreeMap<String, CrateSnapshot>, history: &[HistoryEntry]) -> String {
    let mut rows: Vec<(&str, &CrateSnapshot, usize, usize, usize)> = snapshot
        .iter()
        .map(|(name, snap)| {
            let (high, medium, low) = severity_counts(&snap.report);
            (name.as_str(), snap, high, medium, low)
        })
        .collect();
    rows.sort_by(|a, b| b.2.cmp(&a.2).then(b.3.cmp(&a.3)).then(a.0.cmp(b.0)));

    let mut profile_rows = String::new();
    for (name, snap, high, medium, low) in &rows {
        let name = html_escape(name);
        profile_rows.push_str(&format!(
            "<tr><td><a href=\"https://crates.io/crates/{name}\" target=\"_blank\" rel=\"noopener\">{name}</a></td>\
             <td>{version}</td><td class=\"sev-high\">{high}</td><td class=\"sev-medium\">{medium}</td><td class=\"sev-low\">{low}</td></tr>\n",
            version = html_escape(&snap.version),
        ));
    }

    let mut history_rows = String::new();
    for entry in history.iter().rev().take(HISTORY_DISPLAY_LIMIT) {
        let (sev_class, sev_label) = match entry.worst_severity.as_deref() {
            Some("high") => ("sev-high", "high"),
            Some("medium") => ("sev-medium", "medium"),
            Some("low") => ("sev-low", "low"),
            _ => ("sev-none", "none"),
        };
        let deps_note = if entry.added_dependencies.is_empty() {
            String::new()
        } else {
            format!(", +{} new dep(s)", entry.added_dependencies.len())
        };
        let date = entry.checked_at.get(..10).unwrap_or(&entry.checked_at);
        history_rows.push_str(&format!(
            "<tr><td>{date}</td><td><a href=\"https://crates.io/crates/{name}\" target=\"_blank\" rel=\"noopener\">{name}</a></td>\
             <td>{old} &rarr; {new}</td><td class=\"{sev_class}\">{sev_label}</td><td>+{added}/-{removed} signal(s){deps_note}</td></tr>\n",
            date = html_escape(date),
            name = html_escape(&entry.name),
            old = html_escape(&entry.old_version),
            new = html_escape(&entry.new_version),
            added = entry.added_signals,
            removed = entry.removed_signals,
        ));
    }
    if history_rows.is_empty() {
        history_rows = "<tr><td colspan=\"5\" class=\"empty\">No version changes detected yet -- check back after the next scheduled run.</td></tr>\n".to_string();
    }

    let generated_at = now_iso();
    let tracked_count = snapshot.len();

    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>capscan leaderboard</title>
<meta name="description" content="Capability surface tracking for popular crates.io crates, powered by capscan.">
<link rel="icon" href="data:image/svg+xml,<svg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 100 100%22><text y=%22.9em%22 font-size=%2290%22>%F0%9F%9B%A1%EF%B8%8F</text></svg>">
<link rel="alternate" type="application/atom+xml" title="capscan leaderboard" href="feed.xml">

<meta property="og:type" content="website">
<meta property="og:url" content="https://poglesbyg.github.io/capscan-leaderboard/">
<meta property="og:title" content="capscan leaderboard">
<meta property="og:description" content="Capability surface of {tracked_count} popular crates.io crates, tracked over time by capscan.">
<meta property="og:image" content="https://poglesbyg.github.io/capscan-leaderboard/og-image.jpg">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:title" content="capscan leaderboard">
<meta name="twitter:description" content="Capability surface of {tracked_count} popular crates.io crates, tracked over time by capscan.">
<meta name="twitter:image" content="https://poglesbyg.github.io/capscan-leaderboard/og-image.jpg">
<style>
  :root {{
    color-scheme: light dark;
    --bg: #ffffff; --fg: #1a1a1a; --muted: #6b7280; --border: #e5e7eb;
    --card: #f9fafb; --link: #2563eb;
    --high: #b91c1c; --medium: #b45309; --low: #4b5563; --none: #9ca3af;
  }}
  @media (prefers-color-scheme: dark) {{
    :root {{
      --bg: #0f1115; --fg: #e5e7eb; --muted: #9ca3af; --border: #262b36;
      --card: #171a21; --link: #60a5fa;
      --high: #fca5a5; --medium: #fcd34d; --low: #cbd5e1; --none: #6b7280;
    }}
  }}
  * {{ box-sizing: border-box; }}
  body {{
    margin: 0; padding: 2rem 1rem 4rem; background: var(--bg); color: var(--fg);
    font: 15px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
  }}
  main {{ max-width: 920px; margin: 0 auto; }}
  h1 {{ font-size: 1.5rem; margin-bottom: 0.25rem; }}
  h2 {{ font-size: 1.1rem; margin-top: 2.5rem; }}
  p.lede {{ color: var(--muted); margin-top: 0; }}
  a {{ color: var(--link); }}
  .table-wrap {{ overflow-x: auto; border: 1px solid var(--border); border-radius: 8px; }}
  table {{ border-collapse: collapse; width: 100%; font-size: 0.9rem; }}
  th, td {{ padding: 0.5rem 0.75rem; text-align: left; white-space: nowrap; }}
  th {{ background: var(--card); border-bottom: 1px solid var(--border); font-weight: 600; }}
  tr:not(:last-child) td {{ border-bottom: 1px solid var(--border); }}
  td.empty {{ color: var(--muted); white-space: normal; text-align: center; padding: 1.5rem; }}
  .sev-high {{ color: var(--high); font-weight: 600; }}
  .sev-medium {{ color: var(--medium); font-weight: 600; }}
  .sev-low {{ color: var(--low); }}
  .sev-none {{ color: var(--none); }}
  footer {{ margin-top: 3rem; color: var(--muted); font-size: 0.85rem; }}
  code {{ background: var(--card); padding: 0.1rem 0.35rem; border-radius: 4px; }}
</style>
</head>
<body>
<main>
  <h1>capscan leaderboard</h1>
  <p class="lede">Capability surface of {tracked_count} popular crates.io crates,
     checked daily against crates.io by <a href="https://github.com/poglesbyg/capscan-mcp">capscan</a>.
     This page only republishes when a tracked crate's version actually
     changes, so "last change" can be older than today even though the
     check itself runs every day -- last change recorded {generated_at}.</p>

  <h2>Recent changes</h2>
  <p class="lede">Every version bump detected on a tracked crate's latest release,
     newest first, with the worst new capability severity it introduced.
     <a href="feed.xml">Subscribe via Atom feed</a> instead of checking back.</p>
  <div class="table-wrap">
  <table>
    <thead><tr><th>Date</th><th>Crate</th><th>Version</th><th>Worst new severity</th><th>Detail</th></tr></thead>
    <tbody>
{history_rows}    </tbody>
  </table>
  </div>

  <h2>Current capability profile</h2>
  <p class="lede">Every tracked crate's latest scanned version, ranked by
     how much raw capability (unsafe/FFI/process/build-script signals first,
     then network/filesystem/env, then low-severity) it carries right now --
     not a judgment of risk, since something like <code>openssl-sys</code>
     is expected to be FFI-heavy by design.</p>
  <div class="table-wrap">
  <table>
    <thead><tr><th>Crate</th><th>Version</th><th>High</th><th>Medium</th><th>Low</th></tr></thead>
    <tbody>
{profile_rows}    </tbody>
  </table>
  </div>

  <footer>
    Generated by <a href="https://github.com/poglesbyg/capscan-leaderboard">capscan-leaderboard</a>,
    built on <a href="https://crates.io/crates/capscan">capscan</a>. Signal
    classification is heuristic AST matching, not real type resolution --
    treat this as a starting point for investigation, not a safety proof.
  </footer>
</main>
</body>
</html>
"##
    )
}

fn render_atom_feed(history: &[HistoryEntry]) -> String {
    let updated = now_iso();
    let mut entries = String::new();
    for entry in history.iter().rev().take(FEED_ENTRY_LIMIT) {
        let sev = entry.worst_severity.as_deref().unwrap_or("none");
        let title = format!(
            "{}: {} -> {} ({sev} severity)",
            entry.name, entry.old_version, entry.new_version
        );
        let deps_note = if entry.added_dependencies.is_empty() {
            String::new()
        } else {
            format!(
                " New dependencies: {}.",
                entry.added_dependencies.join(", ")
            )
        };
        let summary = format!(
            "+{} / -{} signal(s). Worst new severity: {sev}.{deps_note}",
            entry.added_signals, entry.removed_signals
        );
        // Stable per-entry id: nothing in this data is a natural UUID, but
        // (name, new_version, checked_at) together never repeat.
        let id = format!(
            "https://poglesbyg.github.io/capscan-leaderboard/#{}-{}-{}",
            entry.name, entry.new_version, entry.checked_at
        );
        entries.push_str(&format!(
            "  <entry>\n    <title>{title}</title>\n    <link href=\"https://crates.io/crates/{name}\"/>\n    <id>{id}</id>\n    <updated>{checked_at}</updated>\n    <summary>{summary}</summary>\n  </entry>\n",
            title = html_escape(&title),
            name = html_escape(&entry.name),
            id = html_escape(&id),
            checked_at = entry.checked_at,
            summary = html_escape(&summary),
        ));
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <feed xmlns=\"http://www.w3.org/2005/Atom\">\n\
         \x20 <title>capscan leaderboard</title>\n\
         \x20 <link href=\"https://poglesbyg.github.io/capscan-leaderboard/\"/>\n\
         \x20 <link href=\"https://poglesbyg.github.io/capscan-leaderboard/feed.xml\" rel=\"self\"/>\n\
         \x20 <id>https://poglesbyg.github.io/capscan-leaderboard/</id>\n\
         \x20 <updated>{updated}</updated>\n\
         {entries}</feed>\n"
    )
}

/// Which of this run's newly-recorded changes are worth interrupting
/// someone for -- currently just "high" severity, since that's the
/// threshold at which capscan itself treats a change as CI-gate-worthy
/// everywhere else (capscan/capscan-mcp both exit non-zero at this level).
fn high_severity_entries(entries: &[HistoryEntry]) -> Vec<&HistoryEntry> {
    entries
        .iter()
        .filter(|e| e.worst_severity.as_deref() == Some("high"))
        .collect()
}

fn format_alert_message(entry: &HistoryEntry) -> String {
    format!(
        "capscan leaderboard: {} {} -> {} introduced a HIGH severity capability (+{} signal(s), -{} signal(s)). https://poglesbyg.github.io/capscan-leaderboard/#{}",
        entry.name,
        entry.old_version,
        entry.new_version,
        entry.added_signals,
        entry.removed_signals,
        entry.name,
    )
}

/// Posts via `curl` rather than adding an HTTP client dependency -- same
/// reasoning as the rest of the capscan family shelling out to `cargo`
/// instead of reimplementing registry access. Sends both `text` (Slack
/// incoming webhooks) and `content` (Discord webhooks) in one JSON body;
/// each side only reads the field it recognizes and ignores the other.
fn post_webhook(url: &str, message: &str) -> Result<()> {
    let payload = serde_json::json!({ "text": message, "content": message });
    let body = serde_json::to_string(&payload)?;
    let status = std::process::Command::new("curl")
        .args([
            "-sS",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
            url,
        ])
        .status()
        .context("running curl to post a webhook alert")?;
    if !status.success() {
        anyhow::bail!("curl exited with {status}");
    }
    Ok(())
}

/// Opt-in: does nothing unless `ALERT_WEBHOOK_URL` is set (e.g. as a repo
/// secret passed through by the workflow) and at least one of this run's
/// new changes is high severity. A webhook failure is logged, not fatal --
/// a broken notification shouldn't fail the whole scheduled run.
fn send_high_severity_alerts(new_entries: &[HistoryEntry]) {
    let Ok(webhook_url) = std::env::var(ALERT_WEBHOOK_ENV) else {
        return;
    };
    if webhook_url.trim().is_empty() {
        return;
    }

    for entry in high_severity_entries(new_entries) {
        let message = format_alert_message(entry);
        if let Err(e) = post_webhook(&webhook_url, &message) {
            eprintln!(
                "warning: failed to send webhook alert for {}: {e}",
                entry.name
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capscan::{Signal, SignalKind};

    fn report_with_kinds(kinds: &[SignalKind]) -> CrateReport {
        CrateReport {
            name: "x".to_string(),
            version: "1.0.0".to_string(),
            files_scanned: 1,
            lines_scanned: 1,
            dependencies: vec![],
            signals: kinds
                .iter()
                .map(|&kind| Signal {
                    kind,
                    file: "src/lib.rs".to_string(),
                    line: 1,
                    detail: "x".to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn severity_counts_buckets_correctly() {
        let report = report_with_kinds(&[
            SignalKind::UnsafeFn,    // high
            SignalKind::Ffi,         // high
            SignalKind::UnsafeBlock, // medium
            SignalKind::EnvRead,     // low
            SignalKind::EnvRead,     // low
        ]);
        assert_eq!(severity_counts(&report), (2, 1, 2));
    }

    #[test]
    fn severity_counts_empty_report_is_all_zero() {
        assert_eq!(severity_counts(&report_with_kinds(&[])), (0, 0, 0));
    }

    #[test]
    fn html_escape_escapes_special_characters() {
        assert_eq!(
            html_escape(r#"<script>alert("hi") & "bye"</script>"#),
            "&lt;script&gt;alert(&quot;hi&quot;) &amp; &quot;bye&quot;&lt;/script&gt;"
        );
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.json");

        let mut snapshot = BTreeMap::new();
        snapshot.insert(
            "anyhow".to_string(),
            CrateSnapshot {
                version: "1.0.104".to_string(),
                report: report_with_kinds(&[SignalKind::UnsafeBlock]),
                last_checked: "2026-01-01T00:00:00Z".to_string(),
            },
        );

        save_snapshot(path.to_str().unwrap(), &snapshot).unwrap();
        let loaded = load_snapshot(path.to_str().unwrap()).unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["anyhow"].version, "1.0.104");
    }

    #[test]
    fn load_snapshot_missing_file_returns_empty_map() {
        let snapshot = load_snapshot("/nonexistent/path/snapshot.json").unwrap();
        assert!(snapshot.is_empty());
    }

    #[test]
    fn render_html_handles_empty_state_without_panicking() {
        let html = render_html(&BTreeMap::new(), &[]);
        assert!(html.contains("No version changes detected yet"));
        assert!(html.contains("capscan leaderboard"));
    }

    #[test]
    fn render_html_sorts_profile_by_severity_then_name() {
        let mut snapshot = BTreeMap::new();
        snapshot.insert(
            "low-sev".to_string(),
            CrateSnapshot {
                version: "1.0.0".to_string(),
                report: report_with_kinds(&[SignalKind::EnvRead]),
                last_checked: "2026-01-01T00:00:00Z".to_string(),
            },
        );
        snapshot.insert(
            "high-sev".to_string(),
            CrateSnapshot {
                version: "1.0.0".to_string(),
                report: report_with_kinds(&[SignalKind::UnsafeFn]),
                last_checked: "2026-01-01T00:00:00Z".to_string(),
            },
        );

        let html = render_html(&snapshot, &[]);
        // Match the href, not the bare name: "low-sev" is also a substring
        // of "low-severity" in the page's prose (found the hard way -- this
        // assertion originally used a bare `find("low-sev")`, which matched
        // that prose above the table instead of the actual row and made the
        // test pass or fail for the wrong reason).
        let high_pos = html.find("/crates/high-sev\"").unwrap();
        let low_pos = html.find("/crates/low-sev\"").unwrap();
        assert!(
            high_pos < low_pos,
            "higher-severity crate should be listed first"
        );
    }

    fn history_entry(name: &str, severity: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            name: name.to_string(),
            old_version: "1.0.0".to_string(),
            new_version: "2.0.0".to_string(),
            checked_at: "2026-01-01T00:00:00+00:00".to_string(),
            worst_severity: severity.map(str::to_string),
            added_signals: 3,
            removed_signals: 1,
            added_dependencies: vec!["new-dep".to_string()],
            removed_dependencies: vec![],
        }
    }

    #[test]
    fn atom_feed_contains_entry_details_and_is_escaped() {
        let entry = history_entry("weird<>&name", Some("high"));
        let feed = render_atom_feed(&[entry]);

        assert!(feed.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
        assert!(feed.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\">"));
        assert!(feed.contains("weird&lt;&gt;&amp;name"));
        assert!(!feed.contains("weird<>&name")); // must not appear unescaped
        assert!(feed.contains("1.0.0 -&gt; 2.0.0"));
        assert!(feed.contains("high severity"));
        assert!(feed.contains("New dependencies: new-dep."));
    }

    #[test]
    fn atom_feed_handles_empty_history() {
        let feed = render_atom_feed(&[]);
        assert!(feed.contains("<feed"));
        assert!(feed.contains("</feed>"));
    }

    #[test]
    fn atom_feed_respects_entry_limit() {
        let entries: Vec<HistoryEntry> = (0..(FEED_ENTRY_LIMIT + 10))
            .map(|i| history_entry(&format!("crate-{i}"), None))
            .collect();
        let feed = render_atom_feed(&entries);
        assert_eq!(feed.matches("<entry>").count(), FEED_ENTRY_LIMIT);
    }

    #[test]
    fn high_severity_entries_filters_by_severity() {
        let entries = vec![
            history_entry("a", Some("high")),
            history_entry("b", Some("medium")),
            history_entry("c", None),
            history_entry("d", Some("high")),
        ];
        let high = high_severity_entries(&entries);
        let names: Vec<&str> = high.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a", "d"]);
    }

    #[test]
    fn alert_message_includes_key_details() {
        let entry = history_entry("anyhow", Some("high"));
        let message = format_alert_message(&entry);
        assert!(message.contains("anyhow"));
        assert!(message.contains("1.0.0"));
        assert!(message.contains("2.0.0"));
        assert!(message.contains("HIGH"));
        assert!(message.contains("+3 signal"));
        assert!(message.contains("capscan-leaderboard"));
    }

    #[test]
    #[ignore = "posts to a real HTTP endpoint; set ALERT_TEST_URL to a local \
                listener to run: ALERT_TEST_URL=http://127.0.0.1:PORT/path \
                cargo test --release -- --ignored post_webhook_sends_expected_json_body"]
    fn post_webhook_sends_expected_json_body() {
        let url = std::env::var("ALERT_TEST_URL")
            .expect("set ALERT_TEST_URL to a local echo server to run this test");
        post_webhook(&url, "hello from a real test").unwrap();
    }

    #[test]
    fn send_alerts_is_a_noop_without_webhook_url_env_set() {
        // Just needs to not panic and not attempt any network/process call
        // when the env var isn't set -- can't easily assert "no curl ran"
        // without a mock, but a real webhook URL would make this test
        // actually hang/fail on network access, so a silent return is the
        // only correct behavior for a unit test to exercise here.
        std::env::remove_var(ALERT_WEBHOOK_ENV);
        send_high_severity_alerts(&[history_entry("x", Some("high"))]);
    }

    #[test]
    fn prune_untracked_removes_crates_no_longer_in_the_list() {
        let mut snapshot = BTreeMap::new();
        snapshot.insert(
            "kept".to_string(),
            CrateSnapshot {
                version: "1.0.0".to_string(),
                report: report_with_kinds(&[]),
                last_checked: "2026-01-01T00:00:00Z".to_string(),
            },
        );
        snapshot.insert(
            "removed-from-list".to_string(),
            CrateSnapshot {
                version: "1.0.0".to_string(),
                report: report_with_kinds(&[]),
                last_checked: "2026-01-01T00:00:00Z".to_string(),
            },
        );

        prune_untracked(&mut snapshot, &["kept".to_string()]);

        assert_eq!(snapshot.len(), 1);
        assert!(snapshot.contains_key("kept"));
        assert!(!snapshot.contains_key("removed-from-list"));
    }
}
