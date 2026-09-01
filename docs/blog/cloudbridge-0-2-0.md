---
title: "0.2.0: a cost viewer becomes a ledger"
description: "Charges from every source now land in one FOCUS-shaped fact table, raw payloads are kept so a mapping fix costs nothing to replay, and a total is finally a single currency."
date: 2026-09-01
tag: Release
---

Version 0.1 showed you a number per provider and called it a total. If you had an AWS account in dollars and an Alibaba Cloud account in yuan, the "total" added the two. Credits read as usage. A balance read as spend. 0.2.0 rebuilds the ground those numbers stand on.

## One fact table

Every source now normalizes into a single fact table, `fct_charge`, in a new `billing.duckdb`. The columns are named after [FOCUS](https://focus.finops.org/), the FinOps Foundation's open cost and usage specification, so a credit is a `Credit`, a refund is a refund, taxes and support fees are labelled as themselves, and amounts keep their sign. When an Alibaba Cloud voucher covers part of a charge, the gross charge and the voucher each get their own row, and the rows sum to what you actually paid. A gap that can't be explained becomes one `Adjustment` row instead of vanishing.

Because the schema already speaks FOCUS, a real bill export — an AWS CUR or an Alibaba Cloud bill file — can drop in later with no schema change.

## Keep the raw answers

Cost Explorer bills $0.01 per request, so asking twice for the same thing is a design flaw, not a usage pattern. Every provider response is now written to disk unchanged, as Hive-partitioned Parquet under `raw/` — the same layout a bill export bucket uses. Normalization is a pure function from a stored batch to ledger rows. Two consequences: a mapping fix replays what's on disk instead of paying for another fetch, and billing logic is testable from a recorded response.

## One currency

Charges are stored in the currency they were billed in and converted for display, each at a rate dated no later than the charge itself. Totals come from a view, `v_charge_normalized`, so switching the reporting currency in Settings rebuilds a view and rewrites nothing. A charge in a currency no rate covers is reported on the dashboard — never silently counted at par.

## Credentials leave the database

Credentials now live only in the OS keyring (Windows Credential Manager, macOS Keychain). The v1 migration moves any that were still in the database and drops the columns that held them. The database can be copied, backed up, or inspected without carrying your keys with it.

## Fetch and normalize, split

A billing source used to be an enum with five `match` arms. It's now a table row with a capability descriptor, and its two methods are deliberately separated: `fetch` touches the network and interprets nothing; `normalize` interprets and touches nothing. Adding a source is adding rows and a mapping, not editing a switchboard.

## Also in 0.2.0

- macOS ships as a `.dmg` holding `CloudBridge.app` instead of a zipped bare binary that Finder took for a document.
- A refresh now ingests the current and previous billing period in two Cost Explorer calls, where the dashboard previously made three.
- DeepSeek balances are recorded as snapshots; a rise in the topped-up balance between observations is derived as a `Purchase`.
- The Dark Mode switch actually changes the theme.

## What's next

The [roadmap](https://github.com/JetSquirrel/cloudbridge/blob/main/docs/roadmap.md) lays out P1: a bill file export channel (S3 / OSS + Parquet), tag-based allocation with an explicit "unallocated" node, and a Sankey cost flow. The ledger was the prerequisite for all three — it's now in place.
