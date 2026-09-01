# CloudBridge Roadmap

CloudBridge is growing from a multi-cloud cost viewer into a personal
finance platform for everything an individual developer spends on
infrastructure and AI: public cloud, model-provider APIs, token plans and
subscriptions — all in one ledger, under one currency, on one machine.

DuckDB is the engine because the interesting questions are analytical:
where does the money flow (Sankey), what changed and why (attribution),
what looks wrong (anomaly detection).

## Scope

**In scope.** Single-machine desktop app. Individual developers and very
small teams. Local-first: credentials and billing data never leave the
machine.

**Out of scope**, and we will close issues asking for them:
multi-user or shared deployments, invoice reconciliation, a general
chargeback/showback rule engine, a separate collector daemon, team
collaboration features.

**FOCUS.** We follow the [FOCUS](https://focus.finops.org/) column naming
so that a future ingest of a real CUR / Alibaba Cloud bill export needs no
schema change. We do *not* implement the full specification. Three concepts
carry their weight for a personal ledger:

- `BilledCost` — what was actually charged
- `EffectiveCost` — after amortization of commitments
- `ChargeCategory` — `Usage` / `Purchase` / `Credit` / `Tax` / `Adjustment`

## Where we are

**P0 is done.** A source is a registry row rather than an enum variant.
`billing.duckdb` holds `fct_charge`, `ingest_batch`,
`fct_balance_snapshot` and `dim_fx_rate` behind a transactional
whole-period write. Every source fetches raw payloads to Parquet and
normalizes them through a pure function, tested against a recorded
response. The dashboard reads `v_charge_normalized`, so every figure on it
is in one currency, converted per charge at a rate dated no later than the
charge itself.

The acceptance test holds: all three sources land in one `fct_charge`
table, `SELECT sum(billed_cost_base) FROM v_charge_normalized WHERE
billing_period = ?` is the cross-cloud total, and a repeated ingest of an
unchanged bill produces identical rows.

All three structural problems are closed:

1. ~~**`CloudProvider` is a compile-time enum**~~ — replaced by the source
   registry in PR1. An unrecognized source id is skipped with a warning
   instead of being silently read as AWS.
2. ~~**Amounts are summed across currencies.**~~ — fixed in PR6. Charges
   are stored in the currency they were billed in and converted in a view,
   so a rate correction or a change of reporting currency costs nothing.
   A charge in a currency no rate covers is counted nowhere and reported
   on the dashboard rather than quietly folded in at par.
3. ~~**`fetch` and `normalize` are fused.**~~ — split in PR3. `fetch`
   persists what the provider returned and interprets nothing;
   `normalize` interprets and touches nothing, so a mapping fix replays
   payloads already on disk instead of paying Cost Explorer again.

What P0 did *not* do, and P1 owns: instance-level detail. Alibaba Cloud's
bill overview is one row per product per month, so its trend chart is as
coarse as its source data.

## P0 — FOCUS normalization

The one-sentence acceptance test for the whole phase:

> All three providers land in a single `fct_charge` table; one SQL query
> returns a cross-cloud, cross-currency monthly total; and running ingest
> twice produces identical results.

The six changes are a dependency chain — land them in order.

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

- **Bill file export channel (S3 / OSS + Parquet).** Replaces per-request
  API polling with the providers' own bill exports: instance-level detail,
  no per-call cost, full history. Only a new `fetch` implementation.
- **Tag allocation with an explicit "unallocated" node.** The unallocated
  share is the number that matters — it tells you how much of the bill you
  cannot yet explain.
- **Sankey.** Funding source → provider → service → tag → unallocated.

A warning to surface in the UI before it becomes a bug report: AWS cost
allocation tags must be activated by hand in the Billing console and are
**not** applied retroactively. Alibaba Cloud has comparable activation
rules. When a tag view is empty, say so and link the documentation rather
than rendering a blank chart.

## P2

- **Three-tier anomaly detection with attribution** — period-over-period at
  the account, service and resource level, always answering "what changed"
  rather than only "something changed".
- **Budget alerts.**
- **Month-end snapshot freezing**, built on `ingest_batch`.

A desktop app cannot alert while it is closed. Budget alerts are scoped as
"notify on open, plus a monthly review", or a lightweight tray resident —
we will not promise real-time alerting in the README.

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
— that guarantee belongs in the README next to "credentials never leave
your machine". **Subscriptions:** under a flat monthly plan the marginal
cost of a session is near zero, so multiplying tokens by list price is not
what was spent. Model it as a commitment drawdown and report the shadow
cost — what the same usage would have cost on demand — against the actual
subscription fee.
