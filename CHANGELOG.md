# Changelog

All notable changes to this repo are documented here.
Format: Added / Changed / Deprecated / Removed / Fixed / Security, newest first.

This is a high-level, human-facing summary. For the detailed narrative of
individual PRs (rationale, alternatives considered, exact diffs), see
[RELEASE_NOTES.md](./RELEASE_NOTES.md). No versioned release has been cut
yet, so everything so far is under Unreleased.

## [Unreleased]

### Added
- **Knowledge pipeline runner** (`dbs-pipeline`, #1308): Raindrop source,
  live/Raindrop-copy/Wayback fetch chain, resumable JSONL ledger.
- **Clip core** (`dbs-clip`, #1306): pure HTML-to-markdown clipper with
  Obsidian frontmatter, ported from url2obs, for the knowledge pipeline.
- **Core engine** (`dbs-core`): data model, error hierarchy, `Connector`
  contract/capabilities, cursor/checkpoint-safe engine with idempotent
  upsert + content-hash change classification, revision history,
  soft-delete sweep with a configurable safety fraction, crash-recovery
  reaper, least-privilege secrets scoping, a managed HTTP client
  (backoff/`Retry-After`/rate limiting), cooperative cancellation, and a
  `BackupService` façade shared by the CLI and web UI.
- **Dynamic plugin registry**: connectors run as separate
  `dbs-connector-<type>` subprocess binaries speaking a line-delimited
  JSON-IPC protocol (handshake → run → stream), not compiled into the
  host — see [ADR-0001](./docs/adr/0001-dynamic-plugin-registry.md).
  Per-source `[sources.NAME]` config crosses the same wire via
  `Connector::configure` — see
  [ADR-0002](./docs/adr/0002-per-source-connector-config.md).
- **14 connectors**, each a real subprocess binary with its own
  `configure()`: Raindrop, GitHub, Pinboard, Readwise, Mastodon, Bluesky,
  Spotify, Pocket Casts, generic Podcast (RSS/Atom + OPML), Vimeo, Udemy,
  Reddit, Skool, and YouTube. Reddit/Skool drive a Playwright-based
  browser-session helper for login capture and page scraping; Vimeo/Udemy
  optionally shell out to `yt-dlp` for video downloads.
- **SQLite storage**: schema/migrations, upsert/classify/revisions,
  browse/query with FTS5 full-text search, metrics aggregation, and
  maintenance (`VACUUM`, WAL checkpoint, `PRAGMA optimize`, snapshot).
- **Export formats**: JSON, NDJSON, CSV, Markdown, a zip archive bundle,
  Obsidian vault, and a cross-linked wiki — plus incremental per-item
  Markdown notes, passphrase-encrypted export/decrypt (scrypt +
  AES-256-GCM), and export-profile overrides per source.
- **`dbs` CLI**: `init`, `backup` (single source or `--all`, with
  `--parallel`, `--only-due`, `--force-full`, `--reconcile`, `--dry-run`),
  `status`, `history`, `items`, `stats`, `export`/`export-notes`/
  `export-profiles`/`export-wiki`, `verify`, `restore`, `decrypt`,
  `doctor`, `update-ytdlp`, `maintain`, `schedule`, `serve`, `capture`,
  `sources` (`list`/`add`/`check`), `connectors` (`list`/`describe`), and
  `research` (`youtube`/`youtube-backup`).
- **`dbs serve` web UI** (`dbs-web`, axum): a vanilla-JS SPA dashboard,
  a full `/api` surface (status, items/media browsing, sources,
  connectors, secrets management, backup trigger + SSE progress stream,
  export, in-UI setup and browser-login capture, research), a background
  scheduler (`--schedule`), and bearer-token auth for non-localhost binds.
- **`dbs research`** (`dbs-research`): ad-hoc NotebookLM pipelines over
  fresh YouTube searches or already-backed-up videos, rendering a
  Markdown research report.
- **Webhook notifications**: `[dbs] notify_url`/`notify_on` POSTs a
  Slack/Discord-compatible summary after each backup batch.
- **Source lifecycle management**: `dbs sources remove`/`enable`/`disable`
  (CLI) and matching `DELETE`/`PATCH /api/sources/:name` + Remove/Enable
  buttons (web UI) — previously only `add` existed, so removing or pausing
  a source required hand-editing `dbs.toml`.

### Changed
- The web UI's setup now happens on one guided **Add source** screen:
  pick a service, then install, paste its key, sign in and set the
  required options on a single form. Optional settings fold under
  **More options**. Connectors, API keys and Verify moved into a
  collapsed **Advanced** sidebar group. Dark-mode tertiary text now meets
  WCAG AA contrast, all UI text is at least 11px (sidebar group labels,
  tile labels, table headers and Library item details were 10–10.5px),
  and links default to the accent colour instead of the browser's blue.
- `[dbs] http_timeout`/`http_rate_limit_per_min` now reach the connector
  subprocess's real HTTP client instead of being parsed and unused.
- `[dbs] batch_max` now drives the engine's actual commit-flush cadence,
  replacing a previously hardcoded constant.
- Every connector whose `configure()` reads options from `[sources.NAME]`
  now declares a real `config_schema` (was always `null`), so the web UI's
  Add Source form renders typed guided fields with descriptions and
  defaults instead of requiring hand-written JSON in the Advanced textarea.

### Fixed
- Sources added, removed, enabled or disabled in the web UI now take
  effect immediately. Previously the server kept serving the config it
  started with, so a new source didn't appear and **Back up now** was
  refused with "no such source" until `dbs serve` restarted.
- The web UI's Install button appeared on every connector, including ones
  with nothing to install, because it read a `ready` field the API never
  sent. `/api/connectors` now reports `needs_install` and
  `install_requirements`.
- The web UI's empty-state "Add a source" link was nearly invisible in
  dark mode. It is now a primary button.
- The `dbs init` config template and the Research page no longer tell
  users to `pip install 'daily-backup-system[...]'`, which is the Python
  reference package's install command and not part of this port. They now
  name the real dependencies (Playwright, yt-dlp, nodejs-wheel) and point
  to the Connectors page's Install button.
- The Research page's "From my backups" mode now warns upfront (and
  disables "Run research") when no YouTube-type source is configured,
  instead of only failing after a wasted round trip through a job that
  starts and then errors.
- Several rounds of stale doc-comments describing already-landed wiring
  as still-open work, caught and corrected as later features shipped.
- Dead/orphaned helper code removed once its real caller landed instead
  of left unreachable.
- A duplicated event-listener block was making every sidebar/nav click
  fire the target tab's data loader twice — doubling every API call and
  visibly duplicating rendered content on the Connectors, Library,
  Research, and API-keys pages.
- The research feature's example topic (`"claude code skills"`,
  referencing an unrelated product) replaced with a neutral one,
  consistently across the web UI placeholder, `dbs research`'s `--help`
  text, and its own tests.
- The Library table's `MEDIA` count (and the CLI's `dbs items` `[N
  media]` suffix) now counts stored media only, matching the summary
  card and item detail panel — previously it counted every declared
  media entry even when nothing was actually downloaded.
- The web UI's sidebar is reachable again on mobile viewports (≤760px)
  via a new hamburger toggle — it was previously hidden with no way to
  reopen it, leaving every page but Overview unreachable on a phone.
- The web UI no longer shows Windows' raw `\\?\`-prefixed canonicalized
  path in the sidebar footer / API-keys page.
- The Connectors page now shows an explanatory empty state instead of
  silently rendering nothing when no connectors are discoverable; the
  API-keys page no longer claims "none required" when it actually
  couldn't resolve a configured source's connector type.
- A connector's `.env`-configured secret (`token_env`) now actually
  reaches the connector subprocess — it was previously resolved from
  the process environment only, so a token written via `dbs init`'s
  documented flow or the web UI's "API keys" page never worked despite
  `dbs doctor` reporting it as set.
- Test suite made deterministic on Windows: connector-discovery tests no
  longer depend on the host's ambient `PATH`/an unbuilt workspace, and
  Unix-only subprocess fixtures (`#!/bin/sh` scripts) are properly gated
  instead of failing outright.
- Add Source's Name/Advanced-options placeholders now reflect the
  selected connector Type instead of always showing Raindrop's example.
- `/api/export`/`/api/items` reject an inverted `since`/`until` range
  instead of silently returning an empty result.
- `dbs schedule --interval <typo>` now errors instead of silently
  falling back to the daily preset.
- The Library Table view is usable at tablet widths again — Title
  ellipsis-truncates instead of wrapping into 5+ lines per row, and the
  table now scrolls horizontally to reach every column instead of being
  squeezed unreadably narrow.
- "Back up all"/"Run" now disable synchronously on click instead of
  staying clickable through the network round-trip before the server
  marks a run in progress.
- `dbs doctor`'s VPN-guard warning is OS-aware instead of always
  suggesting Linux-only `sudo`/`systemctl` commands.
- `dbs backup --dry-run`'s result line no longer labels its intentional
  no-op `error: dry-run`.
- The web UI's `document.title` now reflects the current tab instead of
  staying `Daily Backup System` everywhere.
- The item detail drawer now renders a readable description block
  instead of requiring the field be found inside the raw JSON dump.

### Security
