# CloudBridge Roadmap

CloudBridge is growing from a multi-cloud cost viewer into a personal
finance platform for everything an individual developer spends on
infrastructure and AI: public cloud, model-provider APIs, token plans and
subscriptions — all in one ledger, under one currency, on one machine.

DuckDB is the engine because the interesting questions are analytical:
where does the money flow (Sankey), what changed and why (attribution),
what looks wrong (anomaly detection).

## Scope

**In scope.** Single-machine desktop app for individual developers and
very small teams. Credentials are stored in the OS keyring and used to
authenticate requests to the configured providers. The desktop app has no
CloudBridge sync service or telemetry. Billing databases and raw payload
files stay local, but are not encrypted by the app; protect the device,
its backups and exported files accordingly.

**Out of scope:** multi-user or shared deployments, invoice reconciliation,
a general chargeback/showback rule engine, a separate collector daemon,
and team collaboration features.

**FOCUS.** We use [FOCUS](https://focus.finops.org/) terminology to give
API responses and bill exports a common ledger representation. We do
*not* implement the full specification. Three central concepts are:

- `BilledCost` — what was actually charged
- `EffectiveCost` — after amortization of commitments
- `ChargeCategory` — `Usage` / `Purchase` / `Credit` / `Tax` / `Adjustment`

## Current status

**Released:** P0's normalized ledger shipped in 0.2.0. P1's local bill
imports, tag allocation and Sankey shipped in 0.3.0, together with initial
alert rules from P2. Version 0.3.1 brought interface consistency improvements
and signed, notarized macOS releases. See the [changelog](../CHANGELOG.md)
for release boundaries.

**Working tree / unreleased:** AWS Data Exports (CUR 2.0, FOCUS 1.2 with
AWS columns) can be read from S3. This extends P1's export channel; it is
not part of 0.3.1. OSS collection remains future work. S3 reads avoid Cost
Explorer request charges, but S3 storage and request fees still apply.

A source is a `SourceDescriptor` registry entry rather than an enum
variant. API-backed sources implement `BillingSource`; file-only sources
provide a bill parser without an API client. The ingest pipeline persists
raw payloads and normalizes them into ledger rows. Charges go to
`fct_charge`, balances to `fct_balance_snapshot`, with provenance in
`ingest_batch` and conversion rates in `dim_fx_rate`.

Writes replace a whole billing period transactionally for a source and
account. Converted charge totals read `v_charge_normalized`, using a rate
dated no later than the charge. Missing exchange rates exclude the
unconvertible charges from converted totals and are surfaced separately;
usage records without an authoritative amount are not counted as spend.
Re-ingesting an unchanged bill should preserve its charge values, even
though batch identifiers and ingestion timestamps change.

The three structural problems addressed by P0 are closed:

1. **Provider enum:** replaced by a source registry. Unknown source IDs
   are skipped with a warning rather than silently treated as AWS.
2. **Mixed-currency totals:** original amounts remain in the ledger and
   conversion happens in a view, without refetching the bill.
3. **Coupled fetch and normalization:** raw payloads can be replayed after
   a mapping fix without another provider request.

Local bill import supports Alibaba Cloud, Volcengine, OpenAI, Anthropic
and DeepSeek. Alibaba Cloud's detail export adds instance and billing-item
rows, including model-level detail for Model Studio (百炼). DeepSeek's
API provides balances; its cost export supplies spend detail. The anomaly,
balance-floor and untagged-ratio rules evaluate the ledger on open and
after each ingest; opening the Alerts page only re-checks whether open
alerts have resolved, and nothing runs while the app is closed.

## P0 — FOCUS normalization (historical implementation notes)

The PR1–PR6 notes below preserve the original design sequence and the
intermediate states at each landing. References to "currently", "not yet"
and a later PR describe that historical stage, **not the current app**.
Names such as `Capabilities`, `SnapshotOnly` and `CloudService` belong to
that design history; the current contracts are `SourceDescriptor`,
`Reporting` and `BillingSource` in the source tree.

The original phase acceptance target was:

> All three providers land in a single `fct_charge` table; one SQL query
> returns a cross-cloud, cross-currency monthly total; and running ingest
> twice produces identical results.

Balances are stored separately from charges in the implemented design.
The six changes formed a dependency chain and landed in order.

### PR1 · Source registry — landed

Replace the `CloudProvider` enum with a `SourceId` plus a descriptor table
carrying a `Capabilities` struct. The unknown-provider fallback becomes a
skip-with-warning instead of a silent rewrite to AWS. The UI stops
branching on provider identity — a source is rendered as a balance because
its granularity is `SnapshotOnly`, not because it is called DeepSeek.

Pure refactor, no behavior change. It comes first because every later PR
would otherwise have to edit the same 48 sites.

### PR2 · New database, `fct_charge`, batch table — landed

A fresh `billing.duckdb` with a `schema_version` table; credentials move to
their own store. Four tables: `fct_charge`, `ingest_batch`,
`fct_balance_snapshot`, `dim_fx_rate`.

As landed, the two cache tables stay behind in `cloudbridge.duckdb` rather
than being dropped here: they are the only thing feeding the dashboard
until PR4 and PR5 normalize into `fct_charge`, and dropping them early
would mean paying Cost Explorer for a fetch on every launch in between.
`cost_data` — dead code — is gone, and so are the credential columns: the
application database is versioned and rebuilt at v1, keeping accounts and
budgets, with secrets in the OS keyring only.

Writes are transactional whole-period replacement keyed by
`(provider, account_id, billing_period)`. Providers re-issue a bill in full
mid-month and retroactively correct prior months; row-by-row upsert would
leave behind entries the provider has since deleted, and the total would
stop matching.

`ingest_batch` has no user-visible feature attached to it. It is the
foundation for P3 month-end snapshot freezing — freezing means pinning a
billing period to the state it had at a given batch. If we do not record
batches now, there is nothing to freeze later.

Two schema decisions that are free today and a full-table migration if
deferred:

- `billed_cost` is nullable, and a `cost_basis` column records whether a
  figure is `authoritative`, `derived`, `estimated`, or absent. This lets
  authoritative bills, unit-price-derived amounts and pure usage records
  share one table, and lets the UI mark derived figures so nobody reads a
  shadow cost as money actually spent.
- `pricing_unit` is not restricted to cloud units. Today it holds `GB-Mo`
  and `Hrs`; tomorrow it holds `Tokens`.

### PR3 · Split fetch from normalize, land raw Parquet — landed

`BillingSource` replaces `CloudService`. `fetch` retrieves and persists raw
payloads unchanged; `normalize` is a pure function from raw to FOCUS rows.

Raw data is partitioned as:

```
raw/provider=<p>/account=<a>/billing_period=<YYYY-MM>/batch=<id>/part-0.parquet
```

The same path semantics are used for a local directory and a remote bucket,
so P1's S3/OSS export channel only replaces the `fetch` implementation —
`normalize` and everything downstream are untouched.

A pure `normalize` is also the first time billing logic becomes testable:
record one API response per provider as a fixture and assert on the rows.

As landed, `CloudService` became `BillingSource` and all three sources
implement both halves, so the pipeline is whole end to end
(`ingest::ingest_period`) and re-normalizing without fetching is a
supported operation (`ingest::renormalize_period`). What the normalizers
do *not* do yet is the mapping detail PR4 and PR5 own: AWS asks only for
`UnblendedCost` and files everything as `Usage`, Alibaba Cloud records the
discount as the gap between `billed_cost` and `list_cost` rather than as
`Credit` rows, and DeepSeek writes balance snapshots without deriving
top-ups.

### PR4 · AWS to FOCUS — landed

Cost Explorer currently requests only `UnblendedCost`, grouped by `SERVICE`.
Request `UnblendedCost`, `AmortizedCost` and `UsageQuantity` in a single
call — each call is billed, so do not split it — and add `RECORD_TYPE` to
the grouping to populate `charge_category`. `cost_basis` is `authoritative`.

Three decisions worth recording:

- Amounts keep the sign Cost Explorer gives them, so credits and refunds
  stay negative and a period total is a plain sum.
- A record type this build does not recognize is an `Adjustment`, with a
  warning naming it. Money moved; calling it `Usage` would quietly inflate
  what reads as consumption.
- A row is only dropped when it is zero on *both* cost metrics. Usage
  covered by a commitment is zero unblended and non-zero amortized, and
  dropping it would lose what the commitment actually bought. Grouping by
  service also mixes usage types, and Cost Explorer says so by returning
  the unit `N/A`: a quantity like that is not stored, because it cannot be
  added to anything.

### PR5 · Alibaba Cloud and DeepSeek — landed

Alibaba Cloud `QueryBillOverview`: `PretaxAmount` to `billed_cost`,
`PretaxGrossAmount` to `list_cost`, each voucher/deduction as its own
`Credit` row. Currency CNY.

As landed, the usage row carries the **gross** amount and each deduction
is a negative `Credit` beside it. Putting the net amount on the usage row
*and* the deductions next to it would count them twice — Alibaba Cloud
reports both figures on the same line, unlike AWS, which bills the
discount as a line of its own. Decomposed this way a product's rows sum to
`PretaxAmount`, which is what was actually charged, and a total stays a
plain sum. Where the named deductions do not close the gap between gross
and net, the remainder becomes one `Adjustment` row rather than
disappearing.

DeepSeek reports a balance, which is state, not a charge. It moves to
`fct_balance_snapshot`; only top-ups become `fct_charge` rows with
`charge_category = Purchase`. The current code stuffs the balance into
`current_month_cost`, which is semantically wrong and blocks any correct
total.

Top-ups are *derived*, not stored: a rise in the topped-up balance between
two consecutive observations is the only evidence of a purchase such a
source gives, and re-ingesting a period recomputes them, since replacing a
period clears what was there. The first observation of an account yields
nothing — a balance that was simply there the first time it was looked at
was not witnessed being paid for. The display path still reports the
balance as `current_month_cost`; that is PR6's to fix, along with
everything else the dashboard reads.

### PR6 · Read through views, fix cross-currency — landed

Amounts are stored in their original currency. Conversion happens in a
view, never at write time, because rates get corrected and the user may
change their reporting currency:

```sql
CREATE VIEW v_charge_normalized AS
SELECT c.*, c.billed_cost * f.rate AS billed_cost_base
FROM fct_charge c
ASOF LEFT JOIN dim_fx_rate f
  ON f.from_ccy = c.billing_currency
 AND f.to_ccy = '<reporting currency>'
 AND f.rate_date <= c.charge_period_start::DATE;
```

Ships with a built-in rate table and a reporting-currency setting. This is
where the cross-currency total is actually fixed.

As landed, the view also carries `effective_cost_base` and the `fx_rate` it
used, and a charge already in the reporting currency converts at 1.0
without needing a row in the rate table. A charge whose currency no rate
covers keeps a NULL `billed_cost_base`: it is left out of every converted
total and counted separately, so the dashboard can say how many charges it
is not showing rather than under-reporting silently.

The freshness window moved into the ledger with the same change:
`ingest_batch` records when each period was last written, which is what a
refresh checks. The two response cache tables are gone, and so are
`get_cost_summary` and `get_cost_trend` — a source now fetches and
normalizes, and nothing else.

## P1

- **Bill file import — landed.** The providers' own bill exports, read from
  a file the user downloaded: instance-level detail, no per-call cost, and
  for a model service the only channel that reports a model at all.
  Alibaba Cloud (账单明细, which is what makes Model Studio 百炼 legible
  per model), Volcengine (for Ark 火山方舟), OpenAI, Anthropic and
  DeepSeek.

  As landed this is a parser plus a registry field, not a new `fetch`: an
  import writes through the same whole-period replacement, so an imported
  month supersedes a fetched one instead of being added to it. Important
  contracts:

  - Replacement covers the **entire month for the selected source and
    account**, not just matching rows. Use complete, unfiltered exports:
    a file narrowed to one product or a partial month replaces that month's
    existing charges with only the supplied rows.
  - Text exports must be UTF-8. Re-export or save as CSV UTF-8 if a console
    produces GBK or another encoding; the importer does not guess.

  - A source need not have a billing API. `SourceDescriptor::build` is
    optional, and Volcengine, OpenAI and Anthropic ask for no credentials
    at all — the file channel signs nothing, so requiring a key would put
    a secret in the keyring for nothing to use.
  - A usage export, which carries token counts and no money, lands with
    `billed_cost` NULL and `cost_basis = absent`. This is the first thing
    to use that column for what it was added for. Multiplying tokens by a
    list price would put a figure in `billed_cost` that nobody was charged.
  - Which channel a period arrived through is recorded
    (`ingest_batch.channel`), and an imported month is never replaced by an
    automatic fetch — Force Refresh included. The export is the finer
    reading, and "force" means "do not trust the freshness window", not
    "discard what I imported".
  - A console's download is not always a bare file. DeepSeek's is a zip of
    a cost CSV and a token-count CSV whose count column is named `amount`;
    the format names the member it reads, so the zip imports as it
    downloaded and the token file is refused rather than totalled as money.

- **Bill export collection (S3 / OSS + Parquet) — AWS landed in the
  working tree, unreleased.** An AWS account can point at its Data Exports
  (CUR 2.0, FOCUS 1.2 with AWS columns) bucket using an
  `s3://bucket/prefix` URI. Refresh reads resource-level rows with tags —
  Parquet or gzipped CSV, as the export was configured — rather than
  calling Cost Explorer. Each row keeps its linked account
  (`SubAccountId`). This avoids Cost Explorer
  request charges; S3 storage and request fees still apply, with other AWS
  charges possible depending on the bucket configuration. Downloaded
  objects are retained in the raw store for offline normalization. Accounts
  without an export URI continue to use Cost Explorer. OSS and other
  cloud export collection remain future work.
- **Tag allocation with an explicit "unallocated" node — landed.** The
  unallocated share is the number that matters, and it is on the Overview
  as its own figure: how much of the bill you cannot yet explain. The
  untagged-ratio alert rule watches it.
- **Sankey — landed.** Source → service or model → business line, with an
  explicit Unallocated node. Flows are gross usage: a net flow can be
  negative, which means nothing in a Sankey. Per provider the tail beyond
  the top few services merges into one `Other` node, so the ribbons stay
  legible and every column still sums to the same total.

A warning to surface in the UI before it becomes a bug report: AWS cost
allocation tags must be activated by hand in the Billing console and are
**not** applied retroactively. Alibaba Cloud has comparable activation
rules. When a tag view is empty, say so and link the documentation rather
than rendering a blank chart.

## P2

- **Three-tier anomaly detection with attribution** — period-over-period at
  the account, service and resource level, always answering "what changed"
  rather than only "something changed".

  *Partly landed early, in 0.3.0:* the alerting engine runs a daily-versus-
  7-day-trailing-baseline rule per `(source, service)`, and an alert says
  what it saw — the day's figure, the baseline it broke, the length of the
  streak, and the month-end figure if it holds. The period-over-period side
  has since landed on main: the cost-change decomposition in
  `analytics.rs` attributes the delta, and the Attribution page breaks a
  period down by service, region or service category. What is still owed is
  the resource level.
- **Budget alerts.** *Partly landed:* the balance-floor rule fires on a
  prepaid balance falling below an account's budget, or a default floor,
  and the budget UI is no longer missing — the Rules page edits a monthly
  budget per account, and an "Account budget" rule checks it against cost
  to date or the month-end forecast (landed on main, not yet released).
  A budget *per service or tag* is still owed.
- **Month-end snapshot freezing**, built on `ingest_batch`.

A desktop app cannot alert while it is closed. Budget alerts are scoped as
"notify on open, plus a monthly review", or a lightweight tray resident —
we will not promise real-time alerting in the README. As landed, rules are
evaluated on open and after each refresh, opening the Alerts page re-checks
whether open alerts have resolved, and the docs say so.

## Next — resources and the relationships between them

The bill already names resources: on a real FOCUS export, 98% of usage
cost carries a `ResourceId`, mostly an ARN (S3, Lambda, KMS, DynamoDB),
bare for EC2 volumes and some bucket rows. Per-resource cost therefore
needs no inventory at all — it is a read of `fct_charge` — and ships first.
What an inventory adds is what the bill cannot say: which resource belongs
to which, and which ones nothing uses any more.

- **Resource inventory from corkscrew, optional.** [corkscrew][corkscrew]
  (MIT) scans AWS, Azure, GCP and Kubernetes into DuckDB, keeping every
  scan as `resource_observations` and `relationship_observations`. It is an
  external tool the user installs, not something the app bundles or
  downloads:
  - Bundling means signing, hardening and notarizing two Go executables of
    roughly 120 MB between them inside our `.app`; downloading an
    executable at runtime runs into quarantine. Detect `corkscrew` on
    `PATH` or at a path set in Settings, and without it the pages that use
    an inventory say so.
  - The app runs `corkscrew scan --database <a path of ours>` with the
    account's keyring credentials passed as `AWS_ACCESS_KEY_ID` /
    `AWS_SECRET_ACCESS_KEY` in the child's environment, never on its
    command line. Scanning needs read access well beyond billing (Cloud
    Control, Resource Explorer, each service's Describe/List), so it gets
    its own template in `docs/policies.md`, and it is off until the user
    turns it on per account: it runs third-party code with cloud
    credentials.
  - The contract is two tables, not corkscrew's database. The scan is
    copied into `dim_resource` and `dim_resource_edge` in the ledger,
    stamped with the scan id and time, and nothing reads corkscrew's file
    afterwards. corkscrew writes with DuckDB 1.5 while the app links 1.4,
    and its schema is its own to change; a copy through a narrow read
    keeps either from breaking a page.
  - Matching a bill row to a resource normalizes both sides to the
    resource's own id: corkscrew records an ARN when Resource Explorer
    found the resource and a Cloud Control identifier (often a bare name)
    when it did not, and its `arn` column falls back to that identifier.
    The key is `(sub_account_id, region, service, id)`; a name alone is
    only unique within an account and a service.
  - *Measured* (2026-09 bill of one account, `scripts/match-inventory.sql`):
    99.9% of the usage cost that carries a `ResourceId` matched a scanned
    resource — 97.0% exactly, 2.9% after last-segment normalization, with
    no ambiguous matches; the rest was EC2 deleted before the scan. That
    took three corkscrew fixes first ([jlgore/corkscrew#22][corkscrew-pr]:
    each requested region scanned rather than the first one three times,
    Resource Explorer's type kept, a resource kept when `GetResource`
    fails). Before them the same scan matched 59.6%, or stored nothing.
  - *Not yet usable for relationships.* corkscrew calls `GetResource` with
    the ARN as identifier, which most Cloud Control types reject, so on
    the Resource Explorer path no resource gets its configuration and
    `relationship_observations` stays empty. The relationship questions
    below wait on that, or on deriving edges from each type's properties
    here.
  - The web demo seeds an inventory from `demo_data.rs`, so the pages keep
    compiling and rendering for wasm32.
- **Relationship questions, not a topology picture.** Computed in
  `analytics.rs` from the two tables, and shared by both targets:
  - *Cost rolled up along ownership* — a volume's cost counted to the
    instance it is attached to, and on to the group or business line that
    owns that. This is how the Unallocated share comes down without asking
    the user to tag everything.
  - *Orphans that still cost money* — unattached volumes, snapshots of
    deleted volumes, idle addresses: resources nothing references,
    ranked by what they cost this period.
  - *Resource-level change attribution* — the level P2's anomaly
    detection still owes.
- **The graph view** draws the neighbourhood of a selected resource or
  service, sized by cost, in GPUI the way the Sankey is drawn, with a
  layout written in plain Rust. Traversal is a recursive CTE or `petgraph`
  over a few thousand nodes at most. **Not DuckPGQ:** it is a loadable
  extension, which hardened-runtime library validation refuses to map for
  the same reason `json` and `parquet` are compiled in; it patches DuckDB's
  parser, so it lags every DuckDB release; and it does not exist for
  wasm32.

[corkscrew]: https://github.com/jlgore/corkscrew
[corkscrew-pr]: https://github.com/jlgore/corkscrew/pull/22

## P3

- **Linux build.**
- **Pluggable source adapters** — a source becomes a config entry plus a
  parser, so new providers can arrive as community PRs.
- **Multi-currency reporting refinements.**

## Reserved: local agent data

Local coding-agent token usage (Claude Code, Codex CLI, aider, a
self-hosted gateway) is deliberately **not** scheduled. It is designed for
as an extension point, and P0 pays the entire cost of keeping it cheap:

- `billed_cost` nullable plus `cost_basis` — usage without an authoritative
  amount is representable
- `pricing_unit` accepts `Tokens`
- the source registry is a table, not an enum

With those in place, adding local agent data is additive: one parser, one
registry entry. No schema change, nothing downstream to touch.

Two things to get right when it does happen. **Privacy:** session files
contain full prompts and source code. A parser must extract only
timestamps, model, token counts and tool names, and discard message bodies
— document that boundary alongside the local storage and provider
authentication model. **Subscriptions:** under a flat monthly plan the marginal
cost of a session is near zero, so multiplying tokens by list price is not
what was spent. Model it as a commitment drawdown and report the shadow
cost — what the same usage would have cost on demand — against the actual
subscription fee.
