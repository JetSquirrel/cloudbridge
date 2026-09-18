# Contributing to CloudBridge

Thank you for considering contributing. This guide covers setting up a
working build, the checks your change has to pass, and where things live in
the source tree.

## Code of conduct

Be respectful, inclusive and constructive in all interactions.

## Reporting bugs and suggesting enhancements

Both are tracked as GitHub issues. Before opening one, check whether it
already exists. A good bug report has:

- a clear, descriptive title
- exact steps that reproduce the problem, and what you expected instead
- your environment: OS, and how you installed CloudBridge (official dmg,
  CI build, or your own build)
- screenshots where the issue is visual

An enhancement suggestion should describe the current behavior, the
behavior you want, and why it would be useful.

Redact credentials, account identifiers and sensitive billing details from
logs, screenshots and fixtures. Never post real API keys, signing keys or
unredacted billing exports in public issues or pull requests.

Official macOS releases are signed and notarized. Open the official dmg,
copy CloudBridge to Applications and launch it normally. If macOS blocks
it, report the exact warning, macOS version and release artifact; do not
remove quarantine attributes or disable Gatekeeper as a troubleshooting step.

## Development setup

### Prerequisites

- **Rust 1.95 or newer**, with `rustfmt` and `clippy`; current stable is
  recommended for development. The locked GPUI dependencies use
  `std::hint::cold_path`, which requires Rust 1.95. CI uses `stable` for its
  main checks and reads `Cargo.toml` for a separate, non-blocking MSRV check.
  The minimum-version check on Ubuntu does not validate Windows or macOS
  runtime compatibility.
- **macOS:** Xcode Command Line Tools for the C/C++ toolchain and SDK, plus
  CMake (`brew install cmake`). Native dependencies include bundled DuckDB.
- **Windows:** Visual Studio Build Tools with C++ support and the Windows
  SDK. The shader compiler (`fxc.exe`) from the SDK's `bin/<version>/x64`
  directory must be on `PATH`; the release workflow locates it explicitly.

The release matrix builds macOS Apple Silicon and Windows x64. The check
workflow runs on Ubuntu, but it is not a Linux desktop release or runtime
test. Linux and Intel Mac builds are not validated release targets.

### Building and testing

```bash
git clone https://github.com/YOUR_USERNAME/cloudbridge.git
cd cloudbridge

# Set up the pre-commit hook (formatting and clippy, see below)
git config core.hooksPath .githooks

cargo build          # desktop application; this is what a plain build means
cargo test
```

Note that `cargo build` at the repository root always means the desktop
application: the workspace's `default-members` is the root crate only.

### The checks

CI (`.github/workflows/ci.yml`) runs, on `stable`:

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo check
cargo test
```

and a separate security job runs `cargo audit`, plus `cargo outdated` and
`cargo geiger` as report-only. The pre-commit hook (`.githooks/pre-commit`)
runs the `fmt --check` and `clippy -- -D warnings` steps locally, so
formatting and warning-free clippy are enforced before anything reaches
CI.

### The web (wasm32) build

The same crate also compiles to a browser demo, but this path has
constraints the desktop build does not:

- **Nightly is required for the wasm build only** — `gpui-pre-web` pulls a
  dependency that uses a `stdarch_wasm_atomic_wait` feature stable does not
  have. The desktop build stays on stable.
- **`wasm-bindgen` CLI must match the `wasm-bindgen` crate version.** Read
  the version out of `Cargo.lock` and install the matching
  `wasm-bindgen-cli`.
- **Fonts are embedded:** the four fonts in `web/site/fonts/` are
  `include_bytes!`d from `src/wasm_entry.rs` and must exist to compile.
- **Icons are fetched, not embedded** — the build script copies the icon
  catalog out of the resolved `gpui-kit-assets` crate into `web/site/assets/`
  (gitignored), and the running app requests SVGs from the served
  directory at runtime.

Install the wasm toolchain and target once:

```bash
rustup toolchain install nightly
rustup target add wasm32-unknown-unknown --toolchain nightly
```

Install `wasm-bindgen-cli` with `cargo install wasm-bindgen-cli --version
<VERSION> --locked`, substituting the `wasm-bindgen` version in `Cargo.lock`.
Then build and serve:

```bash
./scripts/build-web.sh              # debug; use --release for an optimized build
python3 -m http.server 8000 --directory web/site
```

Open `http://localhost:8000/`. The script runs `cargo +nightly build --lib
--target wasm32-unknown-unknown`, generates web bindings, and replaces the
generated `web/site/assets/` directory. Do not keep hand-authored assets there.
The browser backend uses in-memory demo data, not real provider credentials.

A change to anything under `src/ui/`, `app.rs`, `alerts.rs`, `model.rs`,
`analytics.rs` or `demo_data.rs` is shared by both targets, so it must keep
building for wasm, not just for the desktop. CI enforces it.

The split inside a read is worth knowing: each backend selects and groups
charges its own way — SQL on the desktop, a fold over vectors in the browser
— and everything computed from those groups lives in `analytics.rs`, once.
A new statistic goes there, not into a `query.rs`.

## Project structure

```
cloudbridge/
├── src/
│   ├── main.rs            # Desktop entry point
│   ├── lib.rs             # Library root; selects the backend per target (cfg)
│   ├── app.rs             # Application state and actions
│   ├── model.rs           # Domain types shared by both targets
│   ├── analytics.rs       # Statistics both backends compute: forecasts,
│   │                      # comparisons, decomposition, data quality
│   ├── demo_data.rs       # The demo bill, as rows; each target writes them
│   ├── config.rs          # Application configuration
│   ├── db.rs              # Desktop: application database (accounts, rules, events)
│   ├── store.rs           # Handle to the backend the UI talks to
│   ├── ingest.rs          # Fetch / import / renormalize pipeline
│   ├── alerts.rs          # Alerting rules, evaluated against the ledger
│   ├── desktop.rs         # Desktop wiring
│   ├── secret_store.rs    # OS keyring credentials
│   ├── cloud/             # Billing sources (desktop only)
│   │   ├── mod.rs         # BillingSource trait, SourceContext, Normalized
│   │   ├── registry.rs    # SourceDescriptor table: every registered source
│   │   ├── aws.rs         # AWS Cost Explorer channel
│   │   ├── aws_focus.rs   # AWS Data Exports (FOCUS) channel
│   │   ├── s3.rs          # Standalone S3 client used by the export channel
│   │   ├── aliyun.rs      # Alibaba Cloud
│   │   ├── deepseek.rs    # DeepSeek balance API
│   │   ├── billfile/      # Bill export import: one parser per provider
│   │   ├── raw.rs         # Raw payload batches, persisted to Parquet
│   │   └── testdata/      # Recorded responses the normalizers are tested against
│   ├── ledger/            # DuckDB ledger: fct_charge, views, fx rates, schema
│   ├── ui/                # Pages: overview, accounts, account detail,
│   │                      # attribution, alerts, rules, settings, charting
│   └── web/               # wasm backends: in-memory ledger, no keyring
├── scripts/
│   ├── build-web.sh       # wasm build + wasm-bindgen + icon catalog
│   └── locked-version.py  # The version Cargo.lock resolved, for a package
├── web/
│   ├── site/              # Static shell for the browser demo
│   └── smol-bridge/       # `smol::unblock` for both targets
└── themes/                # Theme files
```

## How billing data gets in

Both channels normalize into the same ledger: charges in `fct_charge`,
and balances in `fct_balance_snapshot` rather than in spend totals.

1. **Network fetch.** A source implements `BillingSource`
   (`src/cloud/mod.rs`):

   ```rust
   pub trait BillingSource: Send + Sync {
       fn validate_credentials(&self) -> Result<bool>;
       fn fetch(&self, period: &BillingPeriod) -> Result<Fetched>;
       fn normalize(&self, batch: &RawBatch) -> Result<Normalized>;
   }
   ```

   `fetch` retrieves raw payloads unchanged; `src/ingest.rs` persists them
   before calling `normalize`. Normalization is a pure function from a
   raw batch to charges and balances, with no clock, network or database.
   Test it against sanitized fixtures in `src/cloud/testdata/`.

   The AWS S3 export implementation also uses this trait. Its code is in
   the working tree but remains **unreleased**; see `CHANGELOG.md`.

2. **Bill file import.** A source opts in through a `BillFileFormat` on
   its descriptor. The parser is a pure function over a raw batch.
   Volcengine, OpenAI and Anthropic have only this channel and require no
   credentials.

Preserve these import contracts when changing parsers or ingestion:

- An import replaces the **entire month for the selected source and
  account**, not matching rows. A filtered or partial-month export can
  remove existing charges omitted from the file; use complete exports.
- Automatic fetch and Force Refresh preserve imported months.
- Usage-only rows keep `billed_cost` NULL and `cost_basis = absent`;
  token counts multiplied by a list price are not authoritative spend.
- Text imports require UTF-8; unsupported encodings must produce an
  actionable error rather than guessed text.
- DeepSeek's zip is accepted directly. Select its cost CSV, not the
  token-count CSV whose `amount` column is usage, not money.

## Adding a source

A desktop source is a `SourceDescriptor` in the `SOURCES` table in
`src/cloud/registry.rs`. Keep shared UI behavior capability-driven rather
than adding provider-name branches. To add one:

1. Add a descriptor with a unique, stable `id` (persisted in the accounts
   database), display names, credential labels and `Reporting` mode.
2. For a network channel, implement `BillingSource` in `src/cloud/` and
   register the module. Set `build` to a constructor using `SourceContext`.
   Otherwise leave `build` as `None`; the UI must not request unused keys.
3. For an optional file channel, add and register a parser under
   `src/cloud/billfile/`. Provide a `BillFileFormat` with period detection
   and normalization functions, then set the descriptor's `bill_file`.
4. Add sanitized fixture tests for normalization, period boundaries,
   credits, currencies, missing amounts and malformed inputs. The existing
   registry tests check unique IDs, at least one channel, optional
   credentials for file-capable sources, and credential lookup behavior.
5. Keep the browser's metadata registry in `src/web/cloud/mod.rs` aligned
   where relevant. Do not introduce native provider clients into wasm.
   Build both targets and document supported formats and permissions.

The registry module's tests document these contracts precisely — read them
before adding a row.

## Pull requests

1. Fork the repo and create your branch from `main`.
2. Add tests for code that should be tested; billing normalizers are
   always tested against recorded fixtures.
3. Update documentation if you changed behavior the docs describe.
4. `cargo fmt`, and keep `cargo clippy -- -D warnings` clean — the
   pre-commit hook and CI both enforce this.
5. If you touched shared code (`src/ui/`, `app.rs`, `alerts.rs`,
   `model.rs`), confirm the wasm build still compiles
   (`./scripts/build-web.sh`).
6. Open the pull request.

### Commit messages

- Present tense, imperative mood: "Add feature", not "Added feature".
- First line 72 characters or less.
- Reference issues and pull requests after the first line.

## Questions

Open an issue tagged `question`.
