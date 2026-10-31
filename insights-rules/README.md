# Community Insights rules

Each rule here is one more section on CloudBridge's Insights page: a check
that reads the period's bill and resource scan and lists the resources worth a
look, priced from the bill. Built-in checks (stopped instances, idle public
addresses, idle NAT Gateways, unclaimed resources, billed but not in the scan)
ship in the app. These ship from here, without an app release.

The desktop app installs them from **Settings → Insights rules → Browse
catalog**. CI builds this folder on every merge to `main` and publishes the
result to the rolling
[`insight-rules`](https://github.com/JetSquirrel/cloudbridge/releases/tag/insight-rules)
release, which the catalog reads. The browser demo runs only the built-in
rules.

| Rule | Finds |
| --- | --- |
| [`extended-support`](rules/extended-support/rule.ts) | RDS, Aurora and EKS paying Extended Support for an old version |
| [`gp2-volumes`](rules/gp2-volumes/rule.ts) | EBS volumes still on gp2, and what gp3 would save |
| [`idle-load-balancers`](rules/idle-load-balancers/rule.ts) | Load balancers billed by the hour that used no capacity units |

## How a rule works

A rule is `rules/<id>/rule.ts`, an ES module with two exports:

```ts
import type { Finding, Input, Rule } from "../../types/cloudbridge-insights";

export const rule: Rule = {
  title: "Volumes still on gp2",          // section heading
  label: "GP2 VOLUMES",                   // stat card label
  explanation: "What it found, and what to do about it.",
  version: "1.0.0",
  author: "your-github-handle",
  providers: ["AWS"],
  apiVersion: 1,
};

export function evaluate(input: Input): Finding[] {
  return input.billed
    .filter((r) => r.lines.some((l) => /\(gp2\)/.test(l.description ?? "")))
    .map((r) => ({ id: r.id, evidence: "Billed as gp2" }));
}
```

`input` is the reporting period's bill, one entry per resource, each with its
charge lines (description, unit, quantity, cost), and the resource scan, when
one was imported. A finding names the resource by `id` and says in a sentence
what it rests on; its cost, name, region and type are filled in from the bill
or scan unless the rule sets them. [`types/cloudbridge-insights.d.ts`](types/cloudbridge-insights.d.ts)
documents every field.

Rules run in QuickJS inside the app, one runtime each, with no module loader,
no file or network access, 64 MiB of heap and two seconds. A rule can import
from other files here, because the build bundles it into one; it cannot import
a package at run time.

## Adding a rule

```bash
cd insights-rules
npm ci
mkdir -p rules/my-rule/fixtures   # lower case letters, digits and dashes
# write rules/my-rule/rule.ts and at least one fixture
npm test                          # type-check, build, run every fixture
```

Try it in the app before opening a pull request: copy `dist/my-rule.js` into
the rules folder (**Settings → Insights rules → Open folder**) and open
Insights. The page lists a rule that fails to load or run, with its error.

### Fixtures

Every rule needs at least one case in `rules/<id>/fixtures/*.json`. The build
runs each one, in Node and then in the app's QuickJS, and fails if the
findings differ:

```json
{
  "description": "A gp2 volume is flagged; a gp3 volume is not.",
  "input": { "apiVersion": 1, "period": "2026-10", "currency": "USD", "scan": null,
             "resources": [], "billed": [ ... ] },
  "expect": [ { "id": "arn:aws:ec2:...:volume/vol-0gp2", "evidence": "..." } ]
}
```

Include a resource that should be found and one that looks similar but should
not. Take charge descriptions from a real bill, so the match is tested against
what providers actually write, but **never commit real account ids, resource
names or tags**: use `111111111111` and made-up names.

### What makes a good rule

- **It names something to do.** "Delete it", "move it to gp3", "upgrade the
  engine". A rule that only describes spend belongs on another page.
- **It rests on the bill.** Prefer the charge lines, which every account has,
  over the scan, which not every account imports. Say in the evidence what
  the finding rests on.
- **It does not guess.** If the bill cannot tell idle from busy, the rule
  should not call it idle. A missed finding costs less than a wrong one.
- **Its explanation reads in the app.** Sentence case, one or two sentences,
  no exclamation marks, no `·` separators.

### Changing a rule

Raise `version` with every change that reaches users: the patch number for a
fix, the minor for a new finding, the major for a change in what it means.
The app offers the new version as an update and never overwrites a copy
someone edited without asking.

### Review

A maintainer checks that the rule passes CI, that its fixtures cover a miss as
well as a hit, that its evidence and explanation are accurate for the provider
it names, and that nothing in it identifies a real account.

## The catalog

`npm test` writes `dist/`: `<id>.js` per rule and `index.json`, listing each
rule's metadata, size and SHA-256. The app downloads a rule only from the
`insight-rules` release, checks it against the index's checksum before
writing it, and records what it installed in the rules folder's
`catalog.json`, so it can tell an update from a local edit.

`apiVersion` is the version of `Input` a rule was written for. The app runs a
rule only if it knows that version; adding a field to `Input` does not change
it, removing or redefining one does.
