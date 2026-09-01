# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

- **0.2.0** - DeepSeek support, and a FOCUS billing ledger behind the dashboard
- **0.1.2** - Documentation site and packaging fixes
- **0.1.0** - Initial release with AWS and Alibaba Cloud support
