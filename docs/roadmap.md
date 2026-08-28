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

The DuckDB file today is a cache, not a ledger. `cost_data` is dead code;
the live path is two per-account cache tables holding JSON blobs. There is
no fact table, so Sankey, attribution, anomaly detection and month-end
freezing have nothing to build on.

Three structural problems block everything downstream:

1. **`CloudProvider` is a compile-time enum** with 48 references across 7
   files. Adding a source means editing five `match` arms. `db.rs` also
   silently coerces an unknown provider string to `AWS`.
2. **Amounts are summed across currencies.** The dashboard total adds AWS
   USD to Alibaba Cloud CNY and shows the result as one number.
3. **`fetch` and `normalize` are fused.** `get_cost_summary()` returns a
   display-shaped struct straight from the API. Cost Explorer charges per
   request, so any schema change means paying to re-fetch, and there is no
   way to unit-test the billing logic.

## P0 — FOCUS normalization

The one-sentence acceptance test for the whole phase:

> All three providers land in a single `fct_charge` table; one SQL query
> returns a cross-cloud, cross-currency monthly total; and running ingest
> twice produces identical results.

The six changes are a dependency chain — land them in order.

### PR1 · Source registry

Replace the `CloudProvider` enum with a `SourceId` plus a descriptor table
carrying a `Capabilities` struct. The unknown-provider fallback becomes a
skip-with-warning instead of a silent rewrite to AWS. The UI stops
branching on provider identity — a source is rendered as a balance because
its granularity is `SnapshotOnly`, not because it is called DeepSeek.

Pure refactor, no behavior change. It comes first because every later PR
would otherwise have to edit the same 48 sites.

### PR2 · New database, `fct_charge`, batch table

A fresh `billing.duckdb` with a `schema_version` table; credentials move to
their own store. Four tables: `fct_charge`, `ingest_batch`,
`fct_balance_snapshot`, `dim_fx_rate`. The three cache tables are dropped —
the data is re-fetchable, so no migration is written.

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

### PR3 · Split fetch from normalize, land raw Parquet

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

### PR4 · AWS to FOCUS

Cost Explorer currently requests only `UnblendedCost`, grouped by `SERVICE`.
Request `UnblendedCost`, `AmortizedCost` and `UsageQuantity` in a single
call — each call is billed, so do not split it — and add `RECORD_TYPE` to
the grouping to populate `charge_category`. `cost_basis` is `authoritative`.

### PR5 · Alibaba Cloud and DeepSeek

Alibaba Cloud `QueryBillOverview`: `PretaxAmount` to `billed_cost`,
`PretaxGrossAmount` to `list_cost`, each voucher/deduction as its own
`Credit` row. Currency CNY.

DeepSeek reports a balance, which is state, not a charge. It moves to
`fct_balance_snapshot`; only top-ups become `fct_charge` rows with
`charge_category = Purchase`. The current code stuffs the balance into
`current_month_cost`, which is semantically wrong and blocks any correct
total.

### PR6 · Read through views, fix cross-currency

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
