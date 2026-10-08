---
title: "0.5.0: it keeps watching"
description: "CloudBridge stays in the menu bar and refreshes on a schedule, Cloudflare joins split down to the bucket and Durable Object, and Insights scans AWS and Cloudflare for what to cut."
date: 2026-10-08
tag: Release
---

A cost alert is worth something only while the money is still being spent. Until now CloudBridge fetched a bill when someone opened it and pressed Refresh, so an alert could sit unread for as long as the window stayed closed. This release is about that gap: the app keeps running after its window closes, the bill it reads goes down to the resource that spent it, and a refresh that goes wrong no longer costs anything twice.

## It keeps running

On macOS and Windows, closing the window leaves CloudBridge in the menu bar (the notification area on Windows). The icon's title is the month's spend, with the number of open alerts beside it when there are any. Clicking it opens a panel with the month to date, the month-end forecast, the open alerts and the services that moved most against last month.

Behind it, a schedule wakes every 15 minutes, fetches the accounts that are due, runs the alert rules and posts a system notification for each alert that fires. It still waits out the refresh interval before asking a provider again, so it costs what a daily Refresh would. **Open at login**, under Settings → Background, starts it in the menu bar after a restart without opening the window.

Linux builds have no tray icon and still quit with their window; the schedule and notifications run while it is open.

## Cloudflare, down to the resource

Cloudflare is the seventh source. An account ID and an API token with Billing · Read read its billable usage: spend per service, per day. Daily matters here. A Worker or Durable Object that starts looping is in the ledger the next day, not on the invoice at the end of the month.

The bill stops at the service, though: "R2 Class B operations, 734 on 3 October", not which bucket. Add Account Analytics · Read to the token and CloudBridge asks Cloudflare's GraphQL Analytics API for each resource's usage of the meters the bill names, then splits each day's row by those shares. R2 buckets, Workers, D1 databases and Durable Object namespaces each get their part, and the parts still add up to what Cloudflare billed. They are marked as estimates, because analytics are sampled and a free allowance belongs to the account, not to any one resource.

Usage inside the free allowance is kept too, with a cost of zero beside it. That is usually where a runaway starts: usage climbs for days before the first cent is billed.

## Insights: what to cut

Insights compares the bill with what is actually running. It is optional: **Set up and scan** asks first, then installs a pinned, checksummed build of [corkscrew](https://github.com/JetSquirrel/corkscrew/tree/cloudbridge-dist), our fork, which scans each account read-only.

For AWS it finds stopped instances whose volumes still bill, idle public IPv4 addresses, resources with no owner tag and no stack managing them, and resources that are billed but missing from the scan. Each one is priced from the bill when the account reads its Data Export. For Cloudflare it lists Workers, R2 buckets, D1 databases, KV namespaces, zones and Durable Objects, each with its share of the bill and its usage this period.

A large account no longer floods the page. Every card opens on a summary by type, with counts and cost, and lists resources only when you open a type, fifty at a time.

## Nothing paid for twice

We audited what CloudBridge itself can cost you, now that it refreshes on its own:

- **A failed ingest is not fetched again within the refresh interval.** If AWS answered a request and storing the answer then failed (a full disk, a mapping bug), the schedule used to buy the same data again every 15 minutes. Now it waits out the interval and says why; Force Refresh still retries straight away.
- **A failing account is retried less and less often.** The wait starts at 15 minutes and doubles up to the refresh interval, and a notification says the account has stopped refreshing.
- **An AWS Data Export month that hasn't changed is not downloaded again.** Its files' ETags are checked first.
- **Cost Explorer months are read whole.** A large month comes back in pages, and only the first page used to be read. Each extra page is a billed request, capped at 20.

## Smaller things

- **Service categories mean the same thing on every cloud.** ECS, EC2 and Volcengine's ECS are all Compute / Virtual Machines, so the Attribution page's Category view compares clouds. An existing ledger is migrated on first open.
- **A homepage that shows what it reads.** The docs site lists every supported cloud and model platform with its logo.

The full list is in the [changelog](https://github.com/JetSquirrel/cloudbridge/blob/main/CHANGELOG.md).

## What's next

Findings of Cloudflare's own, starting with Durable Object namespaces whose objects keep alarms set, need a real scan to judge them by. The billing APIs for Volcengine, OpenAI and Anthropic are still owed; until then, their exports import.
