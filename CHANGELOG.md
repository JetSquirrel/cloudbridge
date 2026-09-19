# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **AWS Data Exports (FOCUS 1.2) straight from S3.** An AWS account can
  carry the `s3://bucket/prefix` URI of its CUR 2.0 "FOCUS 1.2 with AWS
  columns" export; refresh then reads the Parquet objects from the bucket
  — resource-level rows with tags, and no per-request Cost Explorer cost.
  Accounts without a URI keep the Cost Explorer channel. A period the
  export has not delivered yet is skipped, never written as an empty
  month, and the downloaded objects live in the raw store so
  normalization replays offline.
- Account form: optional "Data export S3 URI" field for AWS accounts.
- **CloudBridge in a browser.** The same crate now builds for wasm32 as a
  demo: the pages, the view models and the alerting rules are the desktop's
  own code, with an in-memory ledger seeded with the demo bill underneath
  them in place of DuckDB, the provider APIs and the OS keyring.
  `scripts/build-web.sh` builds it, and the docs site publishes it at
  `/demo/` — linked from the navigation, the hero, the download section, the
  FAQ and the install guide. Everything the browser build needs is under
  `web/`: the static shell in `web/site/`, the `smol::unblock` bridge in
  `web/smol-bridge/`

### Changed
- The statistics every backend has to agree on — the run-rate forecast, its
  confidence bands, the period-over-period comparison, the cost-change
  decomposition, the trailing daily average, balance burn and the
  data-quality findings — are one shared `analytics` module rather than a
  copy per backend. The desktop groups its charges in SQL and the browser
  folds over vectors, and from there both compute the same arithmetic from
  the same code, tested once
- The demo bill is likewise one `demo_data` module, so the two seeders write
  the same rows rather than two lists that have to be kept equal by hand
- A cost-change decomposition now ranks buckets of equal swing by name
  instead of by hash order, so the same ledger reads the same way twice
- CI builds the web demo as a required job: `src/ui/`, `app.rs` and
  `alerts.rs` are compiled for both targets, and a change that drops one of
  them fails the build rather than waiting to be noticed
- The web build's nightly toolchain and its icon catalogue are pinned to
  exact versions — the nightly by date (`WEB_TOOLCHAIN` in
  `scripts/build-web.sh`), the icons to what `Cargo.lock` resolved

### Fixed
- DuckDB's json and parquet extensions are compiled into the binary
  (`features = ["bundled", "json", "parquet"]`) instead of being autoloaded
  at runtime. A signed macOS build cannot map an extension signed by another
  team — library validation fails with "different Team IDs" — and a CI
  runner with no route to the extension repository cannot download one at
  all, which is how the Parquet re-read in `cloud::raw` failed there

## [0.3.1] - 2026-09-12

A consistency pass over the whole interface: one shared set of cards,
buttons, pills and table headers in place of per-page copies that had
already started to drift — and a signed, notarized macOS build.

### Changed
- Every page now draws from the same theme components — cards, page and
  section titles, stat cards, table headers, primary/outline/danger
  button variants, pills and the range control each have exactly one
  definition. Settings, the last page still hand-rolling its own styles,
  is rebuilt on them
- Amount, relative-time and change-percent formatting are single shared
  implementations; the four diverging copies are gone
- "Biggest movers" ranks by the size of the change rather than the size
  of the spend — a large but flat service no longer holds the list
  forever
- Refresh is the primary action on the Overview; Force Refresh, the
  expensive one, is demoted to a secondary button
- The account detail page merges its SPEND / USAGE / CREDITS cards into
  one SPEND card that breaks the total down in a line
- Terminology is unified on "Unallocated" where the UI previously mixed
  it with "Untagged"
- Docs site: duplicate "How it works" heading renamed, repeated
  explanations deduplicated, hardcoded colors moved into the design
  tokens, and the navigation's left edge aligned with the content
- macOS release builds are now signed and notarized in CI

### Fixed
- Accounts state column no longer gives Healthy the brightest badge
  while Anomaly and Low balance render as plain text — severity now
  decides prominence
- Critical alerts are at least as prominent as warnings; warning yellow
  and alert red are reserved for actual alerts, not neutral labels
- Delete actions and delete confirmations use the danger style; alert
  action buttons are styled by what they do, not by their position, so
  Dismiss can never become the primary button
- A failed toggle or delete on the Rules page no longer replaces the
  whole rule list with an error banner — errors sit inline above the
  list, with a Retry button
- The status bar's freshness dot reflects whether the next fetch is due,
  rather than merely that a sync once happened
- "Resolved this month" shows the date an alert was resolved, not the
  date it was raised
- Yen amounts no longer display meaningless decimals
- Theme and refresh-interval changes no longer fire a redundant
  "Settings saved" banner, and the banner that remains sits under the
  page title where the action happened

## [0.3.0] - 2026-09-11

The release that gives the ledger a second way in and a face worth
reading. Bills you downloaded from a console import into the same
`fct_charge` rows a fetch produces; the Overview stops confusing what a
credit covered with what was consumed; and the interface is rebuilt on
GPUI Kit at desktop density.

### Added
- **Bill file import** — a second channel into the ledger, beside the
  billing APIs. A bill export downloaded from a provider's console is
  parsed into the same `fct_charge` rows a fetch produces, so an imported
  month is indistinguishable downstream from a fetched one.
  - **Alibaba Cloud bill detail (账单明细)** — the finer of its two
    channels. `QueryBillOverview` reports one row per product per month,
    so Model Studio (百炼) arrives as a single figure; the export carries
    the billing item, the instance and the usage quantity, so the same
    month becomes one row per model and `pricing_unit` finally holds
    `Tokens`
  - **Volcengine bill detail (账单明细)** — new source, added for Ark
    (火山方舟), which the bill reports per endpoint and token type
  - **OpenAI cost or usage export** — new source; spend is attributed to
    the project, recorded as `billing_account_id`
  - **Anthropic (Claude) cost or usage export** — new source; spend is
    attributed to the workspace, and cache reads and cache writes stay
    separate priced units rather than being folded into an input total
  - A **usage** export carries token counts and no money. Those rows are
    recorded with `billed_cost` NULL and `cost_basis = absent`: pricing
    them at list would put a number in `billed_cost` that nobody was
    charged. A **cost** export keeps a quantity only where exactly one
    token column is filled in, since input and output tokens are priced
    differently and their sum is not what the amount was charged for
  - One file may span several months; each is imported as its own
    whole-period replacement. An import therefore **replaces** the months
    it covers rather than adding to them — the export is the provider's own
    bill, so adding it to an API reading of the same month would double the
    total
  - Imported files are copied into the raw store under the same
    Hive-partitioned layout as fetched payloads, so a mapping fix replays
    from the copy CloudBridge kept
  - Column names are matched through alias lists covering the Chinese
    console, the English console and each provider's API field names. A
    column that matches nothing is an error naming the columns the file
    does have — none of these exports is a documented file format
  - Non-UTF-8 files are refused with the instruction that fixes them,
    rather than decoded on a guess that would mangle every name in the bill
- A source can now exist without a billing API. Volcengine, OpenAI and
  Anthropic ask for no credentials, and the dashboard does not try to
  refresh them over the network
- `ingest_batch.channel` records whether a period was fetched or imported
  (ledger schema v2, an additive column; an existing ledger is migrated in
  place and its rows read as fetches). An imported month is never replaced
  by an automatic fetch, **including under Force Refresh** — the export is
  the finer reading, and force means "do not trust the freshness window",
  not "discard what I imported". Re-import to update it
- **DeepSeek bill import** — its billing API reports a balance and nothing
  about what the spend was for, so the console's usage download is the
  source's entire view of what it went on: one row per day per model, in
  CNY. The console hands out a **zip**, and the zip imports as it
  downloaded — CloudBridge reads the `cost-*.csv` in it and refuses
  `amount-*.csv`, whose `amount` column is a token count that read as money
  would book 10,787 tokens as ¥10,787
- **An Overview you can set the range of.** MTD, the rolling last 30 days,
  or the last 12 calendar months, as a segmented control in the header;
  every number, chart and ranking on the page is computed for the window
  you picked
- **Gross usage beside the net total.** The headline stays net, but the
  change percent, the chart, "Where it went" and the movers all run on
  `charge_category = 'Usage'`: an account whose usage is fully offset by
  credits nets to ≈ $0, and trends computed on that base are noise. The
  usage and credit buckets are shown next to the total that hides them
- **Demo data**, in **Settings → Demo data**: three accounts and twelve
  months of realistically-shaped fake ledger, loaded and cleared with a
  button, so the app can be reviewed and demonstrated without an empty
  shell or a real bill. Everything demo is keyed under a `demo-` prefix;
  demo accounts carry no credentials and are skipped by refresh, so no
  demo row ever reaches a provider API
- **"Resolved this month"** on the Alerts page keys on when an event was
  resolved, not when it was raised (application schema v6 adds
  `alert_event.resolved_at`; an existing database gains the column in
  place). Events that closed before the stamp existed carry none and drop
  out of the list rather than being filed under a month that would be a
  guess
- **Account detail page** — an account name on the Accounts page is now a
  link: one account's daily or monthly usage trend with the same MTD /
  30d / 12m range control as the Overview, a net/gross/credits stat row,
  and a per-service table with each service's share and its change against
  the comparison window
- **The trend charts answer the mouse.** Hovering snaps to the nearest
  point and draws a guide line, a dot, and a tooltip with the bucket's
  date and amount. The hover machinery is a shared component
  (`chart::ChartHover`), used by the Overview and the account page alike

### Changed
- **The refresh interval is 24 hours, and configurable.** It was a fixed 6
  hours. A provider's bill does not move faster than a day in any way worth
  paying for — Cost Explorer bills per request — so the default is now a
  day, changeable in **Settings → Refreshing** (6, 12, 24 or 48 hours).
  `AppConfig::refresh_interval_minutes` was persisted and never acted on;
  it is replaced by `refresh_interval_hours`, and the rename is deliberate
  rather than a change of unit — reading the stored 60 as a *minute* window
  would have quietly moved every existing install to refreshing hourly. An
  old config starts at the new default and keeps every setting the user did
  choose
- Alibaba Cloud's API normalizer and both bill-detail parsers now share one
  decomposition of a "gross, deductions, net" bill line, so a credit is
  labelled identically whichever channel it arrived through and the two
  reconcile against each other
- **The interface is built on [GPUI Kit](https://crates.io/crates/gpui-kit)
  0.6**, one dependency in place of `gpui` + `gpui-component` +
  `gpui-component-assets`. The tree-sitter grammars stay off: a cost
  dashboard does not need a syntax-highlighting stack compiled into it
- **Desktop density.** A 13px base font and a 6px card radius, applied at
  the application level over whatever a theme file ships, because this is a
  dense data tool and the framework defaults read as web-sized in a desktop
  window. Sizes are rem-derived, so the interface scales with the base font
- The sidebar's sync card is now a **status bar** along the bottom of the
  window — one muted line, in the desktop convention, instead of a card
  competing with the navigation for the eye
- **Credentials are one keychain item per account, not two.** On macOS each
  keychain read can raise a password prompt, and a refresh that fetches
  several periods was making one read per period per key. The pair is now a
  single item, read once per session and cached in memory; a pair still
  stored in the old two-entry form is migrated on first read
- Settings writes and the currency switch run **off the UI thread**, with
  the controls that would race them disabled while one is in flight. A
  failed save is shown in its own color, so it cannot be mistaken for a
  saved one; if the ledger rebuild fails, the currency selection is put back
  to match what the ledger still shows
- Amounts adapt their precision instead of rounding sub-dollar spend to
  `$0`: whole units from 100 up, two decimals from a cent up, and `<$0.01`
  below a cent
- A slow page load can no longer clobber a newer one — each load carries a
  generation and a late result is discarded
- Per-account actions (validate, delete, import) disable their own row's
  button while in flight, and a result whose account was deleted meanwhile
  is dropped rather than reported against a row that is gone

### Fixed
- The warning badge was tinted green. Warnings are yellow
- `usize::MAX` as "every row" wrapped to a negative SQL `LIMIT`, which
  DuckDB refuses outright
- The Sankey with more than a handful of services per provider read as
  crossing ribbons: each provider now keeps its top five services and
  merges the tail — tag breakdown included — into one `Other` node, and
  every column stacks largest-first so the thick flows sit low and
  parallel. Nodes too thin for a text line no longer wear a label that
  spills over their neighbors
- The selected filter chip on the Alerts page paired a foreground token
  with a background it was not designed for, which rendered its label
  invisible in any theme where the two coincide
- The Accounts page's bottom cards could push past the viewport on a long
  raw-store path (a flex child without `min_w_0` cannot shrink below its
  content), and a 96px source column wrapped "Amazon Web Services" onto
  three lines
- `cargo audit` is clean again: eighteen advisories closed by targeted
  upgrades — bytes, h2, quinn-proto, rustls-webpki, tar, time, quick-xml
  (0.30 and 0.37 both gone; the tree carries 0.41 alone), and rkyv, which
  left the tree entirely when rust_decimal moved to 1.43

## [0.2.0] - 2026-09-01

The release that turns a multi-cloud cost viewer into a ledger. Charges
from every source land in one FOCUS-shaped fact table, raw payloads are
kept so a mapping fix costs nothing to replay, and a total is finally a
single currency. See [docs/roadmap.md](docs/roadmap.md) for what comes
next.

### Added
- **FOCUS billing ledger** (roadmap P0/PR2)
  - New `billing.duckdb` with `fct_charge`, `ingest_batch`,
    `fct_balance_snapshot` and `dim_fx_rate`, named after
    [FOCUS](https://focus.finops.org/) columns
  - Transactional whole-period replacement keyed by
    (source, account, billing period), with deterministic charge ids so a
    repeated ingest of an unchanged bill is a no-op
  - Amounts stored in the currency they were billed in; conversion is left
    to a view (PR6)
- **Raw payload store** (roadmap P0/PR3)
  - `fetch` persists provider responses unchanged as Hive-partitioned
    Parquet under `raw/provider=…/account=…/billing_period=…/batch=…/`,
    the same layout a bill export bucket uses
  - `normalize` is a pure function from a stored batch to FOCUS rows, so
    billing logic is testable from a recorded response and a mapping fix
    replays payloads on disk instead of paying for another fetch
- **AWS charges land as FOCUS rows** (roadmap P0/PR4)
  - One Cost Explorer call now carries `UnblendedCost`, `AmortizedCost` and
    `UsageQuantity`, grouped by service and record type
  - `charge_category` comes from the record type, so credits, refunds,
    taxes and support fees are each labelled as themselves instead of all
    reading as usage; amounts keep their sign
- **Alibaba Cloud and DeepSeek land in the ledger** (roadmap P0/PR5)
  - Each Alibaba Cloud voucher, coupon and discount becomes its own
    `Credit` row beside a gross usage charge, so a product's rows sum to
    what was actually charged; an unexplained gap becomes one `Adjustment`
    row instead of vanishing
  - DeepSeek balances are recorded as snapshots, and a rise in the
    topped-up balance between observations is derived as a `Purchase`
- **The dashboard reads the ledger** (roadmap P0/PR6)
  - Totals come from `v_charge_normalized`, which converts each charge at a
    rate dated no later than the charge itself, so cross-cloud figures are
    in one currency instead of adding dollars to yuan
  - Reporting currency is a setting; switching it rebuilds a view and
    rewrites nothing
  - Charges in a currency no rate covers are reported on the dashboard
    rather than being counted at par
- **DeepSeek Integration**
  - DeepSeek API integration for balance queries
  - Display account balance instead of cost for DeepSeek accounts
  - Balance breakdown showing granted and topped-up balances
  - Support for multiple currencies (CNY, USD)
- **Billing source registry** (roadmap P0/PR1) — a source is a table row
  with a capability descriptor, not an enum variant with five `match` arms
- **Reporting currency setting**, with a built-in dated rate table
- **Project roadmap** at `docs/roadmap.md`, and a rebuilt documentation
  site

### Changed
- The response cache tables are gone (application schema v2). A refresh
  checks when a period was last ingested, which the ledger already records
- A billing source only fetches and normalizes now; `get_cost_summary` and
  `get_cost_trend` are gone, along with the per-call trend fetch — the
  trend chart reads rows the refresh already stored
- `CloudService` is now `BillingSource`, with `fetch` and `normalize` split
  apart: the first touches the network and interprets nothing, the second
  interprets and touches nothing
- An unknown source id is skipped with a warning instead of being read as
  AWS
- Application database is versioned and rebuilt at schema v1: the dead
  `cost_data` table and the credential columns are gone, `provider` is now
  `source_id`, and accounts and budgets are carried across
- A refresh now ingests the current and previous billing period in two
  Cost Explorer calls, where the dashboard previously made three
- Alibaba Cloud's trend window covers two billing periods rather than
  seven days, because its bill overview reports one row per product per
  month
- macOS ships as a `.dmg` holding `CloudBridge.app`, ad-hoc signed, rather
  than a zipped bare executable that Finder took for a document

### Fixed
- **Cross-cloud totals no longer add dollars to yuan.** Every amount on
  the dashboard is converted through `v_charge_normalized` at a rate dated
  no later than the charge
- A DeepSeek balance is no longer counted as a month's spend
- Dark Mode switch actually changes the theme
- Version display in Settings, and the refresh-interval row that did
  nothing is gone
- Illegible active item in the sidebar

### Security
- Credentials live in the OS keyring only. The v1 migration moves any that
  were still in the database and drops the columns that held them
- Raw billing payloads are written to a local directory and nowhere else

## [0.1.2] - 2026-02-13

### Added
- GitHub Pages documentation site

### Changed
- macOS release artifact is packaged as a zip

### Fixed
- AccessKey input on the account form ignored what was typed into it
- Download links for artifacts the release never built

## [0.1.1] - 2024-12-10

### Added
- Initial release preparation
- Comprehensive documentation

## [0.1.0] - 2024-12-03

### Added
- **AWS Integration**
  - AWS Cost Explorer API integration with manual AWS Signature V4 signing
  - Current/previous month cost comparison
  - Per-service cost breakdown
  - 30-day cost trend visualization

- **Alibaba Cloud Integration**
  - Alibaba Cloud BSS API integration with HMAC-SHA1 signing
  - Bill overview and instance bill queries
  - Per-product cost breakdown
  - Monthly cost trend visualization

- **Dashboard**
  - Cost overview cards with month-over-month change
  - Account-level cost summaries
  - Expandable service-level details
  - Cost trend charts with statistics

- **Account Management**
  - Add/remove cloud accounts
  - Credential validation before saving
  - Support for AWS and Alibaba Cloud

- **Data Management**
  - DuckDB local storage
  - AES-256-GCM credential encryption
  - 6-hour intelligent caching
  - Force refresh capability

- **User Interface**
  - GPUI-based modern desktop UI
  - Dark theme
  - Responsive sidebar navigation
  - Settings panel

### Security
- All credentials encrypted at rest using AES-256-GCM
- No network transmission except direct cloud API calls
- Local-only data storage

### Known Issues
- Windows only (macOS/Linux support planned)
- Requires Windows SDK for building (fxc.exe shader compiler)

---

## Version History

- **0.3.0** - Bill file import, a range-selectable Overview, and a GPUI Kit interface
- **0.2.0** - DeepSeek support, and a FOCUS billing ledger behind the dashboard
- **0.1.2** - Documentation site and packaging fixes
- **0.1.0** - Initial release with AWS and Alibaba Cloud support
