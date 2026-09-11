---
title: "0.3.0: bring your own bill"
description: "A bill you downloaded from a console now imports into the same ledger the APIs feed, the Overview stops mistaking a covered cost for a cheap one, and the interface is rebuilt at desktop density."
date: 2026-09-11
tag: Release
---

0.2.0 gave CloudBridge one fact table and one currency. It left one thing out: a billing API is not always the best reading of a bill, and sometimes it is not a reading at all. Alibaba Cloud's `QueryBillOverview` reports Model Studio (百炼) as one figure a month. DeepSeek's API reports a balance and nothing about what the money went on. Volcengine, OpenAI and Anthropic have an admin API you may not have a key for.

But every one of those consoles has a Download button. 0.3.0 makes what comes out of it a first-class way into the ledger.

## A second channel, not a second schema

An imported export becomes the same `fct_charge` rows a fetch produces. Downstream — the dashboard, the alerts, the Sankey — an imported month is indistinguishable from a fetched one. What differs is recorded rather than hidden: `ingest_batch.channel` says whether a period arrived by fetch or by import, and an imported month is never overwritten by an automatic fetch, **including under Force Refresh**. The export is the finer reading; "force" means "don't trust the freshness window", not "throw away what I imported". To update an imported month, import a corrected export of it.

An import **replaces** every month the file covers. It has to: the export is the provider's own bill, so adding it to an API reading of the same month would count the month twice.

Five sources import today: Alibaba Cloud and Volcengine bill detail (账单明细), OpenAI and Anthropic cost or usage exports, and DeepSeek.

## Token counts are not money

A usage export carries token counts and no amounts. Those rows are stored with `billed_cost` NULL and `cost_basis = absent` — pricing them at list would put a number in `billed_cost` that nobody was ever charged.

DeepSeek's console makes the same point in a sharper way. Its download is a zip of two CSVs: `cost-*.csv`, whose money column is named `cost`, and `amount-*.csv`, whose column named `amount` is a *token count*. Read the second as money and a day of 10,787 cached input tokens books as ¥10,787. So CloudBridge opens the zip, reads the cost half, and refuses the other half by name. You import the zip exactly as it downloaded.

Column names, meanwhile, are matched through alias lists covering the Chinese console, the English console, and each provider's API field names — none of these exports is a documented file format. A column that matches nothing is an error that names the columns your file actually has. A non-UTF-8 file is refused with the instruction that fixes it, rather than decoded on a guess that would mangle every product name in the bill.

## What a credit hides

A total net of credits is the right headline: it is what you paid. It is the wrong basis for a trend. An account whose usage is fully covered by credits nets to about $0, and a month-over-month percentage computed on that is noise — one real account produced −318.9%.

So the Overview now splits the two. The headline stays net, with the gross usage and the credits that took it down shown next to it. Everything trended or ranked — the chart, the change percent, "Where it went", the biggest movers — runs on gross usage. What you consumed and what you were billed are different questions, and the page now answers both instead of averaging them.

The header also carries a range control: month to date, the rolling last 30 days, or the last 12 calendar months. Every number on the page is computed for the window you picked.

## Desktop density

The interface moves to [GPUI Kit](https://crates.io/crates/gpui-kit) 0.6 — one dependency in place of three, with the tree-sitter grammars switched off, because a cost dashboard has no business compiling a syntax-highlighting stack.

With that came a decision the framework leaves open: this is a dense data tool, not a web page. A 13px base font and a 6px card radius are applied at the application level over whatever a theme file ships. The sidebar's sync card became a status bar along the bottom of the window, the way a desktop app reports state. Sizes derive from the rem scale, so the whole interface still zooms with the base font.

And amounts stopped rounding sub-dollar spend to `$0`: whole units from 100 up, two decimals from a cent up, and `<$0.01` below a cent — a small number is now legible as a small number rather than as nothing.

## Also in 0.3.0

- **Demo data**, in Settings: three accounts and twelve months of realistically-shaped fake ledger, loaded and cleared with a button. Everything demo is keyed under a `demo-` prefix, holds no credentials, and is skipped by refresh, so no demo row ever reaches a provider API.
- **One keychain item per account, not two.** On macOS every keychain read can raise a password prompt — the item's access control names the binary that stored it, so a rebuild is a new binary. A refresh fetching several periods was paying that cost per period per key. The pair is now a single item, read once a session; a pair still in the old two-entry form migrates on first read.
- **The refresh interval is 24 hours, and configurable** (6, 12, 24 or 48). It was a fixed 6. A provider's bill does not move faster than a day in any way worth paying for.
- **"Resolved this month"** on the Alerts page keys on when an alert was resolved rather than when it was raised.
- Settings writes and the currency switch run off the UI thread, and a failed save is shown in its own color so it cannot be mistaken for a saved one.

## What's next

The [roadmap](https://github.com/JetSquirrel/cloudbridge/blob/main/docs/roadmap.md) has the rest of P1: a bill file *export* channel (S3 / OSS + Parquet) for the bills that arrive on a schedule instead of through a browser, and tag-based allocation with an explicit unallocated node. The billing APIs for Volcengine, OpenAI and Anthropic are still owed — those sources sit in the registry with the credential each will want already named.
