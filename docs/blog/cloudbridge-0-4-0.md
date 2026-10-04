---
title: "0.4.0: more ways in, more ways to read it"
description: "AWS Data Exports straight from S3, a Models page for what each LLM model costs per token, a read-only SQL console, monthly budgets — and the whole app running in a browser."
date: 2026-10-05
tag: Release
---

0.3.0 put every bill into one ledger; 0.3.1 made the interface agree with itself. 0.4.0 works on both ends of that ledger. On the way in, an AWS account can now read its bill from the export AWS writes to S3 rather than asking Cost Explorer for it. On the way out, there are four new ways to read what is already there: per model, per dimension, against a budget, and in SQL. And the same application now runs in a browser, so you can try all of it before installing anything.

## AWS Data Exports, read from S3

Cost Explorer answers with totals, and every question costs a request. AWS's Data Exports (CUR 2.0, in the "FOCUS 1.2 with AWS columns" shape) write the full bill — resource IDs, tags, the linked account each row was billed under — into an S3 bucket you own. Give an AWS account the export's `s3://bucket/prefix` URI and Refresh reads from there instead: Parquet or gzipped CSV, whichever the export was set to deliver.

A few things it gets right that are easy to get wrong:

- **A month is counted once.** An export set to "create new" keeps every delivery side by side. CloudBridge reads only the files the period's manifest names, so twelve refreshes of September are still one September.
- **Not delivered is not zero.** A period the export has not written yet is skipped, never stored as an empty month.
- **A payer's export splits by member account.** Each row keeps its `SubAccountId` and `SubAccountName`.

S3 storage and request fees still apply, and the Cost Explorer policy does not grant S3 access — the [permissions page](https://cloudbridge.jetsquirrel.cloud/policies.html#aws-s3) has the narrow policy the export channel needs. Accounts without a URI keep using Cost Explorer.

## The Models page

The ledger has been storing token quantities beside LLM costs since 0.2.0; nothing read them. The new Models page does. For the range you pick, each model gets its cost and change, its input, output and cache tokens, a blended cost per million tokens and its cache share, with a daily token chart beneath.

It also says what stands out. A model whose cache reads are under 5% of its input side is paying input price for what could be cache price. One model carrying more than 70% of model spend is worth knowing about. A model billed with no token metering is flagged as such, because its unit cost cannot be computed. And a spend rise is checked against the two periods' token counts: when the extra tokens explain the rise it is usage, and only when they do not is it reported as a rising unit price.

Model names buried in Alibaba Cloud and Volcengine billing-item text are not extracted yet; OpenAI, Anthropic and DeepSeek imports name their models directly.

## Other ways to read the ledger

- **Query.** A read-only SQL console over the local DuckDB ledger, with starter templates. Only reading statements run, and the ledger is opened read-only for them in any case. Desktop only.
- **Budgets.** The Rules page holds a monthly budget per account, and an "Account budget" rule alerts on a percentage or an amount, against cost to date or the month-end forecast. Anomaly rules can now be scoped to one account too.
- **Breakdown by dimension.** Attribution switches between tag, service, region and service category, drawn as a treemap — area is cost, colour is the change — or as share bars, with the period's costliest resources beside it. An account's detail page has the same drill-down.
- **Data health.** Findings for what makes a total less trustworthy than it looks: currencies the rate table cannot convert, usage with no attribution tag or no region, adjustments no charge explains. Dismiss one and it stays dismissed for that period.

## CloudBridge in a browser

The crate now also builds for WebAssembly. The [demo](https://cloudbridge.jetsquirrel.cloud/demo/) is not a mock-up: the pages, view models and alert rules are the desktop's own code, running over an in-memory ledger seeded with the demo bill in place of DuckDB, the provider APIs and the keyring. Query, Rules and Settings stay on the desktop; everything else, the Models page included, is there.

To keep it that way, CI builds the browser version as a required check, and the statistics both builds compute — forecasts, comparisons, decompositions, findings — live in one shared, tested module rather than a copy per backend.

## Smaller things

- **Deleting an account asks about its history.** Before, the charges stayed behind silently and kept counting in totals with no account left to explain them. The delete dialog now offers to remove them, and history left by an earlier delete is listed on the Accounts page where it can be removed.
- **A first run that goes somewhere.** An empty Overview offers to add an account or load the demo bill, and saving an API account fetches its bill straight away.
- **Keyboard shortcuts.** ⌘1–⌘9 switch pages, ⌘R reloads.
- **The window opens first.** Stores open and pages load in the background, each page on its first visit.
- **The spend chart's fill** no longer cuts a diagonal wedge across the chart.
- **Security:** rustls and quick-xml are upgraded past three RUSTSEC advisories.
- **Rust 1.95** is the declared minimum; the old declaration was never buildable.

The full list is in the [changelog](https://github.com/JetSquirrel/cloudbridge/blob/main/CHANGELOG.md).

## What's next

The [roadmap](https://github.com/JetSquirrel/cloudbridge/blob/main/docs/roadmap.md) turns from bills to the resources behind them: cost per resource first, read from the rows the S3 export already provides, and then the relationships between resources — cost rolled up along ownership, orphans that still cost money. The billing APIs for Volcengine, OpenAI and Anthropic are still owed; until then, their exports import.
