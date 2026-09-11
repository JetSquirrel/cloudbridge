---
title: "0.3.1: one of everything"
description: "A consistency pass over the whole interface — one shared set of cards, buttons, pills and table headers — plus a signed, notarized macOS build."
date: 2026-09-12
tag: Release
---

0.3.0 rebuilt the interface on GPUI Kit, but it rebuilt it page by page. Each page got the new look; each page also kept its own copy of what a card, a primary button or a table header is. Copies drift: the Settings page still had the pre-rebuild styles, and elsewhere the same "pick one of three" control appeared in four different shapes. 0.3.1 is the pass that makes the design system real — one definition per element, in the theme, with every page drawing from it.

## Severity decides prominence

The pass turned up places where the visual hierarchy argued with the information. On the Accounts page, **Healthy** — the one state that asks nothing of you — wore the brightest badge, while Anomaly and Low balance rendered as plain text. On the Alerts page, a Critical alert was less prominent than a Warning. Both are now inverted the right way: the louder the state, the louder the style, and alert red and warning yellow are reserved for actual alerts rather than neutral labels.

The same logic applied to actions. Refresh, the cheap everyday operation, is now the Overview's primary button; Force Refresh, the expensive one, is a secondary. Delete buttons and delete confirmations use the danger style. And an alert's action buttons are styled by what they do, not by where they happen to sit in a list — so Dismiss can never end up as the big primary button just because it came first.

## Errors that don't evict the content

A failed toggle on the Rules page used to replace the entire rule list with a red banner. Errors now sit inline above the list, the list stays put, and every error offers Retry.

## Smaller corrections

- **"Biggest movers"** now ranks by the size of the change, not the size of the spend — a large but flat service no longer holds the list forever.
- The status bar's freshness dot reflects whether the next fetch is due, instead of staying green forever after the first sync.
- **"Resolved this month"** shows the date an alert was resolved, not the date it was raised.
- Yen amounts no longer display meaningless decimals.
- The account detail page merges its SPEND / USAGE / CREDITS cards into one SPEND card that breaks the total down in a line.
- The docs site got the same treatment: a duplicated heading renamed, repeated explanations deduplicated, hardcoded colors moved into the design tokens.

## A signed macOS build

Release builds for macOS are now signed and notarized in CI. The Gatekeeper workaround in the install notes — Control-click, "Open Anyway", the `xattr` line — stops being part of installing CloudBridge; download the DMG and drag it to Applications.

## What's next

Unchanged from 0.3.0: the [roadmap](https://github.com/JetSquirrel/cloudbridge/blob/main/docs/roadmap.md) still owes a bill file *export* channel (S3 / OSS + Parquet), tag-based allocation with an explicit unallocated node, and the billing APIs for Volcengine, OpenAI and Anthropic.
