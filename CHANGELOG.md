# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - TBD

### Planned
- Azure support
- Google Cloud Platform support
- Cost alerts and notifications
- Budget tracking

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
- **DeepSeek Integration**
  - DeepSeek API integration for balance queries
  - Display account balance instead of cost for DeepSeek accounts
  - Balance breakdown showing granted and topped-up balances
  - Support for multiple currencies (CNY, USD)

### Changed
- `CloudService` is now `BillingSource`, with `fetch` and `normalize` split
  apart: the first touches the network and interprets nothing, the second
  interprets and touches nothing
- Billing source registry replaces the `CloudProvider` enum; an unknown
  source id is skipped with a warning instead of being read as AWS
- Application database is versioned and rebuilt at schema v1: the dead
  `cost_data` table and the credential columns are gone, `provider` is now
  `source_id`, and accounts and budgets are carried across. Credentials
  live in the OS keyring only.

### Fixed

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

- **0.1.0** - Initial release with AWS and Alibaba Cloud support
