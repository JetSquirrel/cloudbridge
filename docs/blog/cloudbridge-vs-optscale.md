---
title: "CloudBridge vs OptScale"
description: "One is a FinOps server for an organization, the other a desktop ledger for one person's bills. Which one fits depends less on features than on who is asking."
date: 2026-09-24
tag: Comparison
---

Both projects are open source and both read cloud bills. They are built for different people. [OptScale](https://github.com/hystax/optscale), from Hystax, is a self-hosted FinOps platform an organization runs on a server so that engineering and finance can share one view of cloud spend and act on it. CloudBridge is a desktop app one person runs on their own machine, reading their own bills into a local file.

Facts about OptScale below are taken from its README, repository and documentation as of September 2026, and linked at the end. If something here has gone stale, the sources win.

## The short answer

If several people need access, if your spend is on Azure, Google Cloud or Kubernetes, or if what you want is a list of instances to resize and commitments to buy, use OptScale. CloudBridge does none of those things.

If you are one developer or a small team with one person watching the bill, your spend is spread across AWS, Alibaba Cloud, Volcengine and model APIs such as OpenAI, Anthropic or DeepSeek, and you would rather not run a server to find out what you spent, CloudBridge is the smaller tool for that.

## What OptScale is

OptScale describes itself as an open-source FinOps and cloud cost optimization platform for AWS, Microsoft Azure, Google Cloud, Alibaba Cloud and Kubernetes clusters. It ingests billing and usage data and, on top of cost dashboards, detects unused and idle resources, recommends rightsizing, analyzes Reserved Instance, Savings Plan and Spot usage, and can stop non-production VMs outside working hours on a power schedule. Governance is organization-shaped: pools of resources with quotas and budgets, anomaly detection policies, tagging policies, shared environments, and four base roles (Member, Engineer, Manager, Organization manager). It also covers Databricks and S3 storage analysis.

It is a server made of many services. Its repository has around twenty top-level service directories, among them `rest_api`, `auth`, `ngui` for the web interface, `risp` for commitments, `bumiworker` for recommendations, and Slack and Jira integrations. Production installs go onto Kubernetes, set up by the project's Ansible playbooks on Ubuntu 24.04, with a stated minimum of 8 CPU cores, 16 GB RAM and 150 GB SSD. A Docker Compose mode exists for evaluation only, on the same 8 cores and 16 GB. It is licensed Apache 2.0.

## What CloudBridge is

CloudBridge is a native desktop app written in Rust with [GPUI](https://gpui.rs/), released for macOS on Apple Silicon and Windows x64. There is also a [browser demo](https://cloudbridge.jetsquirrel.cloud/demo/) running on sample data. It pulls costs from the AWS Cost Explorer API, Alibaba Cloud's billing API and DeepSeek's balance API, and imports bill files downloaded from Alibaba Cloud, Volcengine, OpenAI, Anthropic and DeepSeek. Everything lands in one local DuckDB ledger, using column names from [FOCUS](https://focus.finops.org/). Credentials go in the OS keyring. There is no server, no account, no sync and no telemetry.

On top of the ledger: an Overview with month-to-date, 30-day and 12-month views, attribution by service, model and tag with an explicit unallocated share, alert rules for unusual daily spend, low prepaid balances and unallocated cost, and one reporting currency built from a dated exchange-rate table. The main branch adds monthly budget rules, confidence bands around the month-end forecast, a read-only SQL console over the ledger, and reading AWS FOCUS exports straight from S3. None of these is in a tagged release yet; 0.3.2 is the latest. The license is MIT.

## Side by side

| | OptScale | CloudBridge |
| --- | --- | --- |
| What you run | A server: Kubernetes in production, Docker Compose to evaluate | A desktop app on macOS or Windows |
| Stated footprint | 8+ cores, 16 GB RAM, 150+ GB SSD | Your laptop |
| Users | Organizations, with roles and pools | One person per install |
| Where data lives | Your OptScale server | A DuckDB file in your app-data directory |
| Clouds | AWS, Azure, GCP, Alibaba Cloud, Kubernetes | AWS, Alibaba Cloud, Volcengine |
| Model API bills | Not listed in the open-source README | OpenAI, Anthropic, DeepSeek, Alibaba Model Studio (百炼), Volcengine Ark (方舟) |
| How data comes in | Connected accounts | Billing APIs, or bill files you download |
| Optimization advice | Idle resources, rightsizing, RI/SP/Spot, power schedules | None |
| Anomalies and alerts | Anomaly detection, quota and budget policies | Daily-spend, balance and unallocated-cost rules; budget rules on main |
| Cost allocation | Pools, owners, tagging policies, Kubernetes namespaces and labels | Service, model and tag, with the unallocated share shown |
| Integrations | Slack, Jira, Databricks | None |
| License | Apache 2.0 | MIT |
| Cost of the software | Free to self-host; you pay for the server | Free; provider API fees are separate |

## Where OptScale is the better choice

**More than one person needs the numbers.** OptScale has users, roles and an organization tree, so finance and engineering can see the same figures with access set by role. CloudBridge is one ledger on one machine. Its README says outright that shared deployments and team collaboration are out of scope.

**Your spend is on Azure, Google Cloud or Kubernetes.** CloudBridge supports neither Azure nor Google Cloud, and has no idea what a namespace costs. OptScale covers all three, and it can split a cluster's cost by namespace, workload and label.

**You want to be told what to change.** CloudBridge tells you where the money went. It does not tell you an instance is oversized, a volume is unattached, or a Savings Plan would pay for itself. That is the core of OptScale's value, and CloudBridge has no counterpart to it.

**You need governance.** Quotas per pool, tagging policies, power schedules, and alerts sent to Slack or Jira are organization tools. CloudBridge's rules run when the app is open and after a refresh. Nothing runs while it is closed.

A note on ML: Hystax has presented OptScale as an MLOps platform too, with experiment tracking, leaderboards and model versioning. In June 2025 the `arcee` and `bulldozer` services, the ML profiling agent and the bulk experiment runner, were removed from the repository, and the current README does not list experiment tracking. If that is why you are looking at OptScale, check what the version you deploy actually includes.

## Where CloudBridge fits

**One person, one set of bills.** Download, open, and either add an API key or import an export. There is no cluster to install and nothing to upgrade except the app. For one developer, the server that OptScale's minimum requirements describe is more machine than the question needs.

**Model APIs next to cloud.** For many small projects the biggest bill is no longer compute. It is tokens. CloudBridge reads OpenAI and Anthropic cost exports, DeepSeek's daily spend, Model Studio models inside an Alibaba Cloud bill and Ark endpoints inside a Volcengine bill, into the same ledger as the AWS account. A token-only usage export stays usage: it is not priced into a made-up bill.

**Chinese clouds, from their own exports.** Alibaba Cloud's 账单明细 and Volcengine's bill files import as they come out of the console, with Chinese and English column names both recognized. OptScale supports Alibaba Cloud. Volcengine is not on its list.

**Data that stays put.** The ledger and the raw provider responses stay in your app-data directory, and credentials stay in the OS keyring. Nothing goes to a third party, including us. Local is not the same as encrypted, though. CloudBridge does not encrypt the ledger, so disk encryption is still your job.

## Can you use both?

Yes, and they do not interfere. Each reads the provider's billing data on its own, and neither writes to a cloud account. A team could run OptScale for shared AWS, Azure and Kubernetes spend, while one engineer keeps CloudBridge for the model API bills and Chinese-cloud accounts OptScale does not read. Two things to watch: AWS Cost Explorer charges per API request, so two tools polling it pay twice, and the two ledgers will not match line for line, because they map and convert charges differently.

## Links

- CloudBridge: [browser demo](https://cloudbridge.jetsquirrel.cloud/demo/) · [documentation](https://cloudbridge.jetsquirrel.cloud/docs.html) · [source](https://github.com/JetSquirrel/cloudbridge)
- OptScale: [repository and README](https://github.com/hystax/optscale) · [documentation](https://hystax.com/documentation/optscale/) · [roles and permissions](https://hystax.com/documentation/optscale/roles-and-permissions.html) · [anomaly detection](https://hystax.com/documentation/optscale/anomaly-detection.html) · [quotas and budgets](https://hystax.com/documentation/optscale/quotas-and-budgets.html) · [live demo](https://my.optscale.com/live-demo)
- The commit that removed OptScale's ML services: [OSN-943](https://github.com/hystax/optscale/commit/a20a22259582bda9181956618f05f2f2072a871d)
