//! Ingest: fetch → persist raw → normalize → ledger.
//!
//! The order matters. Raw payloads are written *before* anything is
//! normalized, so a mapping bug costs a re-run of [`renormalize_period`]
//! rather than another round of paid API calls. Cost Explorer bills per
//! request; the payloads on disk do not.
//!
//! One ingest run is one `ingest_batch` row, one raw partition, and one
//! whole-period replacement in `fct_charge` — all under the same batch id.
//!
//! [`import_bill_file`] is the same pipeline fed from a file the user
//! downloaded instead of from the network. It writes through the same
//! [`record`], so an imported month is indistinguishable downstream from a
//! fetched one — which is what lets a bill export *replace* a coarser API
//! reading of the same month rather than be added to it.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, LazyLock, Mutex};

use crate::cloud::billfile::BillFileFormat;
use crate::cloud::raw::{self, RawBatch, RawPart};
use crate::cloud::registry::SourceDescriptor;
use crate::cloud::{BillingPeriod, CloudAccount, Normalized, SourceContext};
use crate::config::get_raw_data_dir;
use crate::ledger::{self, query, Channel, PeriodKey};

/// The ledger key an account's period is stored under.
pub fn period_key(account: &CloudAccount, period: &BillingPeriod) -> PeriodKey {
    PeriodKey::new(
        account.source_id.as_str().to_string(),
        account.id.clone(),
        period.label(),
    )
}

/// Why an account cannot be refreshed from the network, if it can't.
fn skip_reason(account: &CloudAccount) -> Option<String> {
    let descriptor = account.descriptor()?;
    if descriptor.fetches_from_api() {
        return None;
    }
    Some(match descriptor.bill_file {
        Some(format) => format!(
            "{} has no billing API in this build; import its {} instead",
            descriptor.display_name, format.display_name
        ),
        None => format!(
            "{} has no billing API in this build",
            descriptor.display_name
        ),
    })
}

/// What one ingest did, for logging and for the UI to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestOutcome {
    pub batch_id: String,
    pub charges: usize,
    pub balances: usize,
    /// Where the raw payloads were written.
    pub raw_path: PathBuf,
}

/// What one refresh of an account did.
#[derive(Debug, Default)]
pub struct RefreshOutcome {
    /// Each period fetched and landed, as `(period label, outcome)`.
    pub ingested: Vec<(String, IngestOutcome)>,
    /// Periods left alone because they were ingested recently enough.
    pub skipped_fresh: Vec<String>,
}

/// Bring an account's ledger up to date, skipping periods ingested
/// recently enough unless `force` says otherwise.
///
/// The current period and the one before it, because the UI shows both —
/// and because a provider keeps correcting last month for a while after it
/// ends. A source that reports only a balance has no history to backfill,
/// so it gets the current period alone.
///
/// "Recently enough" is the `refresh_interval_hours` setting: a provider's
/// bill does not move faster than that in any way worth paying for (Cost
/// Explorer bills per request). It also bounds a fetch that was answered
/// but never recorded — see [`PAID_FETCHES`].
///
/// One period failing does not stop the other: last month's trouble must
/// not leave this month, where a runaway shows up, unfetched. The failures
/// are returned together once both have been tried.
pub fn refresh_account(account: &CloudAccount, force: bool) -> Result<RefreshOutcome> {
    // Demo accounts carry fake data and no credentials; fetching them
    // would only produce a keyring error.
    if account.id.starts_with(crate::ledger::demo::DEMO_PREFIX) {
        tracing::debug!("Skipping demo account {}", account.id);
        return Ok(RefreshOutcome::default());
    }
    let descriptor = account.descriptor().ok_or_else(|| {
        anyhow!(
            "No billing source registered under '{}'",
            account.source_id.as_str()
        )
    })?;
    if let Some(reason) = skip_reason(account) {
        return Err(anyhow!(reason));
    }

    let now = Utc::now();
    let current = BillingPeriod::containing(now);
    let periods = if descriptor.is_snapshot() {
        vec![current]
    } else {
        vec![current.previous(), current]
    };

    let window = refresh_interval();
    let mut outcome = RefreshOutcome::default();
    let mut confirmed = false;
    let mut failures = Vec::new();
    let mut attempted = 0;
    let mut not_ready = 0;
    for period in periods {
        let key = period_key(account, &period);
        // Taken before the freshness check, so a caller that waited on
        // another's fetch of this period sees it landed and skips it.
        let _claim = PeriodClaim::take(&key);
        if !force {
            if is_fresh(&key, now, window)? {
                tracing::debug!(
                    "Skipping {} {}: ingested within the freshness window",
                    account.name,
                    period.label()
                );
                outcome.skipped_fresh.push(period.label());
                continue;
            }
            if let Some(paid) = unrecorded_paid_fetch(&key, now, window) {
                failures.push(anyhow!(
                    "{}: fetched {} but not recorded ({}). It is not fetched again \
                     before {} so the same data is not paid for twice; Force Refresh \
                     retries now",
                    period.label(),
                    paid.at.format("%H:%M UTC"),
                    paid.failure,
                    (paid.at + window).format("%Y-%m-%d %H:%M UTC"),
                ));
                continue;
            }
        }

        attempted += 1;
        match ingest_period(account, &period) {
            Ok(Some(ingested)) => outcome.ingested.push((period.label(), ingested)),
            Ok(None) => {
                confirmed = true;
                outcome.skipped_fresh.push(period.label());
            }
            // An export that has not delivered the period yet is a skip,
            // not a failure — and must never write an empty batch over a
            // month's rows. It is a failure when every period asked for is
            // missing: then the URI is probably wrong, and staying silent
            // would look like a successful no-op.
            Err(e) if crate::cloud::aws_focus::is_export_not_ready(&e) => {
                not_ready += 1;
                tracing::info!("{}", e);
                outcome.skipped_fresh.push(period.label());
            }
            Err(e) => failures.push(anyhow!("{}: {}", period.label(), e)),
        }
    }

    if attempted > 0 && not_ready == attempted {
        return Err(anyhow!(
            "{} has no export data for any of the period(s) asked for — \
             check the export S3 URI, or wait for the export's first delivery",
            account.name
        ));
    }

    if !outcome.ingested.is_empty() || confirmed {
        if let Err(e) = crate::db::mark_account_synced(&account.id, now) {
            tracing::warn!("Could not record the sync time for {}: {}", account.name, e);
        }
    }
    if !outcome.ingested.is_empty() {
        evaluate_alerts();
    }

    match failures.len() {
        0 => Ok(outcome),
        1 => Err(failures.remove(0)),
        _ => Err(anyhow!(
            "{}",
            failures
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )),
    }
}

/// How long a fetched period stays fresh: the `refresh_interval_hours`
/// setting.
fn refresh_interval() -> Duration {
    Duration::hours(i64::from(
        crate::config::load_config()
            .map(|settings| settings.refresh_interval_hours)
            .unwrap_or(crate::config::DEFAULT_REFRESH_INTERVAL_HOURS),
    ))
}

/// Whether a period was ingested recently enough to leave alone.
fn is_fresh(key: &PeriodKey, now: DateTime<Utc>, window: Duration) -> Result<bool> {
    Ok(query::last_ingest(key)?.is_some_and(|ingested_at| now - ingested_at < window))
}

/// A fetch the provider answered — and so, for Cost Explorer or an S3
/// export, one that was paid for — whose ingest then failed.
#[derive(Debug, Clone)]
struct PaidFetch {
    at: DateTime<Utc>,
    failure: String,
}

/// Fetches answered but not recorded, by period, for the life of the
/// process.
///
/// Freshness is read from the ledger's complete batches, so a fetch whose
/// payload could not be stored, normalized or written — a full disk, a
/// mapping bug — left no trace, and the background schedule would buy the
/// same data again every tick, for as long as the fault lasted. A period
/// here waits out the refresh interval like a recorded one; only Force
/// Refresh, which a person presses, goes around it.
static PAID_FETCHES: LazyLock<Mutex<HashMap<PeriodKey, PaidFetch>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn unrecorded_paid_fetch(
    key: &PeriodKey,
    now: DateTime<Utc>,
    window: Duration,
) -> Option<PaidFetch> {
    let fetches = PAID_FETCHES.lock().unwrap_or_else(|e| e.into_inner());
    fetches
        .get(key)
        .filter(|paid| now - paid.at < window)
        .cloned()
}

fn remember_paid_fetch(key: &PeriodKey, at: DateTime<Utc>, outcome: Result<(), String>) {
    let mut fetches = PAID_FETCHES.lock().unwrap_or_else(|e| e.into_inner());
    match outcome {
        Ok(()) => {
            fetches.remove(key);
        }
        Err(failure) => {
            fetches.insert(key.clone(), PaidFetch { at, failure });
        }
    }
}

/// Periods some caller is fetching right now.
///
/// The background schedule, Overview's Refresh and an account's first
/// fetch each call [`refresh_account`] on their own. A period counts as
/// fresh only once its batch is recorded, so without this two of them
/// overlapping would both find it stale and both pay for it.
static IN_FLIGHT: LazyLock<(Mutex<HashSet<PeriodKey>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(HashSet::new()), Condvar::new()));

/// One caller's hold on a period, released when dropped.
struct PeriodClaim(PeriodKey);

impl PeriodClaim {
    /// Wait for any other caller to finish this period, then take it.
    /// Blocking; every caller of [`refresh_account`] is already off the UI
    /// thread.
    fn take(key: &PeriodKey) -> Self {
        let (lock, released) = &*IN_FLIGHT;
        let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
        while held.contains(key) {
            held = released.wait(held).unwrap_or_else(|e| e.into_inner());
        }
        held.insert(key.clone());
        Self(key.clone())
    }
}

impl Drop for PeriodClaim {
    fn drop(&mut self) {
        let (lock, released) = &*IN_FLIGHT;
        lock.lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
        released.notify_all();
    }
}

/// The raw parts of a period's last complete API fetch, for a source to
/// check the provider's data against before downloading it again. Empty
/// when there is none, or it cannot be read: then the fetch just happens.
fn previous_fetch(key: &PeriodKey) -> Vec<RawPart> {
    let source = match query::api_batch_source(key) {
        Ok(Some(source)) => source,
        Ok(None) => return Vec::new(),
        Err(e) => {
            tracing::debug!("No previous batch for {:?}: {}", key, e);
            return Vec::new();
        }
    };
    match raw::read(Path::new(&source)) {
        Ok((parts, _)) => parts,
        Err(e) => {
            tracing::debug!("Could not read the previous batch at {}: {}", source, e);
            Vec::new()
        }
    }
}

/// Fetch one account's billing period and land it in the ledger.
///
/// `None` when the source found the provider's data unchanged since the
/// period's last complete fetch: nothing was downloaded, and that batch is
/// confirmed as current instead of being written again.
pub fn ingest_period(
    account: &CloudAccount,
    period: &BillingPeriod,
) -> Result<Option<IngestOutcome>> {
    let descriptor = account.descriptor().ok_or_else(|| {
        anyhow!(
            "No billing source registered under '{}'",
            account.source_id.as_str()
        )
    })?;
    let key = period_key(account, period);

    let source = descriptor.client(crate::db::account_context(account, descriptor)?)?;
    let Some(fetched) = source.fetch_changed(period, &previous_fetch(&key))? else {
        ledger::confirm_period(&key)?;
        tracing::info!(
            "{} {}: unchanged since its last fetch; not downloaded again",
            account.name,
            period.label()
        );
        return Ok(None);
    };

    // The provider has answered: from here this fetch has been paid for,
    // whatever happens to it next.
    let fetched_at = Utc::now();
    let landed = (|| {
        let batch = RawBatch {
            provider: descriptor.id.to_string(),
            account_id: account.id.clone(),
            period: *period,
            batch_id: ledger::new_batch_id(),
            fetched_at,
            parts: fetched.parts,
            payload_files: fetched.payload_files,
        };
        let raw_path = persist(&batch)?;
        let normalized = source.normalize(&batch)?;
        record(&batch, &normalized, &raw_path, Channel::Api)
    })();
    remember_paid_fetch(
        &key,
        fetched_at,
        landed.as_ref().map(|_| ()).map_err(|e| e.to_string()),
    );
    landed.map(Some)
}

/// Run the alert rules over what was just ingested. A failure here must
/// not fail the ingest that triggered it.
fn evaluate_alerts() {
    match crate::alerts::evaluate() {
        Ok(fired) if fired > 0 => tracing::info!("Alerts: {} new event(s)", fired),
        Ok(_) => {}
        Err(e) => tracing::warn!("Alert evaluation failed: {}", e),
    }
}

/// The normalizer for a source, whichever channel its payloads arrive by.
///
/// Normalizing is pure — no clock, no network, no credentials — so the
/// API client is built with empty keys: they would only be read if it
/// signed a request, which `normalize` never does. Keeping this off the
/// keyring is what makes "replay normalization" safe to click.
fn normalize_with(descriptor: &SourceDescriptor, batch: &RawBatch) -> Result<Normalized> {
    if let Some(build) = descriptor.build {
        let source = build(SourceContext {
            access_key_id: String::new(),
            secret_access_key: String::new(),
            region: None,
            export_uri: None,
        });
        source.normalize(batch)
    } else if let Some(format) = descriptor.bill_file {
        (format.normalize)(batch)
    } else {
        Err(anyhow!(
            "{} has no normalizer in this build",
            descriptor.display_name
        ))
    }
}

/// What one replay of the raw store did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReplayOutcome {
    /// Periods re-normalized.
    pub periods: usize,
    /// Charges written across them.
    pub charges: usize,
}

/// Re-normalize every stored payload without fetching anything — the
/// "Replay normalization" button. The newest batch of each
/// `(provider, account, period)` partition wins, and the period keeps the
/// channel it arrived through, so an imported month is not re-tagged as an
/// API fetch.
pub fn replay_all() -> Result<ReplayOutcome> {
    let root = get_raw_data_dir()?;
    let mut outcome = ReplayOutcome::default();

    for provider_dir in subdirectories(&root, "provider=")? {
        let provider = &provider_dir;
        let Some(descriptor) = crate::cloud::registry::get(provider) else {
            tracing::warn!("Raw: no source registered under {:?}, skipping", provider);
            continue;
        };

        for account_dir in subdirectories(&root.join(format!("provider={provider}")), "account=")? {
            let account = &account_dir;
            let periods_dir = root
                .join(format!("provider={provider}"))
                .join(format!("account={account}"));
            for label in subdirectories(&periods_dir, "billing_period=")? {
                let Some((year, month)) = label.split_once('-') else {
                    continue;
                };
                let (Ok(year), Ok(month)) = (year.parse::<i32>(), month.parse::<u32>()) else {
                    continue;
                };
                let period = BillingPeriod::new(year, month);

                let Some(batch_id) = raw::batches(&root, descriptor.id, account, &period)?.pop()
                else {
                    continue;
                };

                let batch = raw::read_batch(&root, descriptor.id, account, &period, &batch_id)?;
                let raw_path = batch.directory(&root).join("part-0.parquet");
                let normalized = normalize_with(descriptor, &batch)?;
                let channel = query::channel_of(&PeriodKey::new(
                    descriptor.id,
                    account.clone(),
                    period.label(),
                ))?;

                let ingested = record(&batch, &normalized, &raw_path, channel)?;
                outcome.periods += 1;
                outcome.charges += ingested.charges;
            }
        }
    }

    if outcome.periods > 0 {
        evaluate_alerts();
    }

    tracing::info!(
        "Raw replay: {} period(s), {} charge(s)",
        outcome.periods,
        outcome.charges
    );
    Ok(outcome)
}

/// The values of a partition directory's `prefix=value` children, in
/// directory order. A missing directory is empty, not an error.
fn subdirectories(dir: &Path, prefix: &str) -> Result<Vec<String>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut values = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if let Some(value) = name.strip_prefix(prefix) {
            values.push(value.to_string());
        }
    }
    values.sort();
    Ok(values)
}

/// What one bill file import did.
#[derive(Debug, Clone)]
pub struct ImportOutcome {
    /// The format the file was read as, for the message the UI shows.
    pub format: &'static str,
    /// Each billing period the file covered, oldest first, with what
    /// landing it did.
    pub periods: Vec<(String, IngestOutcome)>,
}

impl ImportOutcome {
    /// Total rows written across every period the file covered.
    pub fn charges(&self) -> usize {
        self.periods
            .iter()
            .map(|(_, outcome)| outcome.charges)
            .sum()
    }

    /// The periods it replaced, as `2026-08, 2026-09`.
    pub fn period_labels(&self) -> String {
        self.periods
            .iter()
            .map(|(label, _)| label.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Import a bill export the user downloaded from their provider's console.
///
/// One whole-period replacement per month the file covers — a date range
/// picked in a console has no reason to stop at a month boundary, and each
/// month is the unit the ledger replaces.
///
/// **This replaces those months.** The export is the provider's own bill,
/// so it supersedes whatever the API reported for the same month rather
/// than adding to it. The corollary is worth knowing before importing: a
/// file narrowed to one product in the console replaces the whole month
/// with that one product.
///
/// The file is copied into each month's raw partition before anything
/// interprets it, so a mapping fix replays [`renormalize_period`] against
/// the copy CloudBridge kept. The text is stored once per month it
/// contributes to, which duplicates a small file rather than splitting it
/// — a partition then holds the bill exactly as the provider wrote it,
/// which is the property the raw store exists for.
pub fn import_bill_file(account: &CloudAccount, path: &Path) -> Result<ImportOutcome> {
    let descriptor = account.descriptor().ok_or_else(|| {
        anyhow!(
            "No billing source registered under '{}'",
            account.source_id.as_str()
        )
    })?;
    let format = descriptor.bill_file.ok_or_else(|| {
        anyhow!(
            "{} publishes no bill export CloudBridge can read",
            descriptor.display_name
        )
    })?;

    let text = read_text(path, format)?;
    let periods = (format.periods)(&text)?;
    if periods.is_empty() {
        return Err(anyhow!(
            "{} holds no dated rows, so there is no billing period to import",
            path.display()
        ));
    }

    // Provenance: which file this came from, and a digest, so a re-import
    // of the same download is recognizable in the raw store.
    let request = format!(
        "file {} sha256={}",
        path.display(),
        hex::encode(Sha256::digest(text.as_bytes()))
    );

    let mut outcomes = Vec::new();
    for period in periods {
        let batch = RawBatch {
            provider: descriptor.id.to_string(),
            account_id: account.id.clone(),
            period,
            batch_id: ledger::new_batch_id(),
            fetched_at: Utc::now(),
            parts: vec![RawPart::new(format.part, request.clone(), text.clone())],
            payload_files: Vec::new(),
        };

        let raw_path = persist(&batch)?;
        let normalized = (format.normalize)(&batch)?;
        outcomes.push((
            period.label(),
            record(&batch, &normalized, &raw_path, Channel::File)?,
        ));
    }

    tracing::info!(
        "Imported {} into {}: {} period(s), {} charge(s)",
        path.display(),
        account.name,
        outcomes.len(),
        outcomes
            .iter()
            .map(|(_, outcome)| outcome.charges)
            .sum::<usize>()
    );

    evaluate_alerts();

    Ok(ImportOutcome {
        format: format.display_name,
        periods: outcomes,
    })
}

/// The file's text.
///
/// UTF-8 only, and deliberately so. Both Chinese consoles can produce a
/// GBK-encoded export, which these bytes will not decode as — and guessing
/// an encoding would silently mangle every product name in the bill.
/// Re-saving the file as UTF-8 is something the user can do; spotting a
/// quietly mis-decoded bill is not.
///
/// A zip is opened rather than refused: some consoles (DeepSeek) hand out
/// the export as an archive, and asking the user to unzip it first is a
/// step that exists only because this code did not. Which member is read
/// is the format's call — see [`BillFileFormat::zip_member`].
fn read_text(path: &Path, format: &BillFileFormat) -> Result<String> {
    let bytes =
        std::fs::read(path).map_err(|e| anyhow!("Cannot read {}: {}", path.display(), e))?;

    if bytes.starts_with(b"PK\x03\x04") {
        return read_zip_member(path, &bytes, format);
    }

    utf8(path, bytes)
}

fn utf8(path: &Path, bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|_| {
        anyhow!(
            "{} is not UTF-8 text. Re-export it as UTF-8, or open it and \
             save it again as CSV UTF-8 — an encoding guessed here would \
             mangle every name in the bill.",
            path.display()
        )
    })
}

/// The text of the one member of a zip the format reads.
fn read_zip_member(path: &Path, bytes: &[u8], format: &BillFileFormat) -> Result<String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| {
        anyhow!(
            "{} looks like a zip but does not open as one: {}",
            path.display(),
            e
        )
    })?;

    let names: Vec<String> = archive.file_names().map(str::to_string).collect();
    let wanted: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|name| match format.zip_member {
            Some(member) => name.contains(member),
            None => format
                .extensions
                .iter()
                .filter(|extension| **extension != "zip")
                .any(|extension| name.to_lowercase().ends_with(&format!(".{extension}"))),
        })
        .collect();

    let member = match wanted.as_slice() {
        [only] => (*only).to_string(),
        [] => {
            return Err(anyhow!(
                "{} holds none of the {} this import reads. It holds: {}",
                path.display(),
                format
                    .zip_member
                    .map(|member| format!("{member}*"))
                    .unwrap_or_else(|| format.extension_hint()),
                names.join(", ")
            ))
        }
        several => {
            return Err(anyhow!(
                "{} holds more than one file this import could read ({}). \
                 Unzip it and pick the {} instead.",
                path.display(),
                several.join(", "),
                format.zip_member.unwrap_or(format.display_name)
            ))
        }
    };

    let mut entry = archive
        .by_name(&member)
        .map_err(|e| anyhow!("Cannot read {member} in {}: {}", path.display(), e))?;
    let mut text = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut text)
        .map_err(|e| anyhow!("Cannot read {member} in {}: {}", path.display(), e))?;

    utf8(path, text)
}

/// Delete an account's billing history: its ledger rows, then its raw
/// payloads, which would otherwise bring the rows back on the next replay.
/// Returns how many charges were removed.
pub fn delete_account_history(account_id: &str) -> Result<usize> {
    let charges = ledger::delete_account_history(account_id)?;
    raw::delete_account(&get_raw_data_dir()?, account_id)?;
    Ok(charges)
}

/// Write the payloads under `raw/`, checking first that nothing in the path
/// came out of the database with a separator in it.
fn persist(batch: &RawBatch) -> Result<PathBuf> {
    raw::check_path_segment(&batch.provider, "source id")?;
    raw::check_path_segment(&batch.account_id, "account id")?;
    raw::check_path_segment(&batch.batch_id, "batch id")?;

    raw::write(&get_raw_data_dir()?, batch)
}

/// Replace the period in the ledger with what the normalizer produced.
///
/// Balances go in first. A source that reports only a balance reports no
/// purchases either, so its purchases are derived from the movement
/// between observations — including the one just made.
fn record(
    batch: &RawBatch,
    normalized: &Normalized,
    raw_path: &Path,
    channel: Channel,
) -> Result<IngestOutcome> {
    let key = PeriodKey::new(
        batch.provider.clone(),
        batch.account_id.clone(),
        batch.period.label(),
    );

    for balance in &normalized.balances {
        ledger::record_balance(balance)?;
    }

    let mut charges = normalized.charges.clone();
    if !normalized.balances.is_empty() {
        // Recomputed on every ingest rather than written once, because
        // replacing the period clears whatever was there before.
        charges.extend(ledger::top_up_charges(&key)?);
    }

    ledger::replace_period(
        &key,
        &batch.batch_id,
        &charges,
        Some(&raw_path.to_string_lossy()),
        channel,
    )?;

    // The rollup recomputes just this period's day range. A failure must
    // not fail the ingest that triggered it: the rollup reads as stale and
    // is rebuilt on the next start.
    if let Err(e) = ledger::rollup::refresh_for_period(&key) {
        tracing::warn!(
            "Daily rollup refresh failed for {}/{} {}: {}",
            key.provider,
            key.account_id,
            key.billing_period,
            e
        );
    }

    tracing::info!(
        "Ingested {} {}: {} charge(s), {} balance(s)",
        batch.provider,
        batch.period.label(),
        charges.len(),
        normalized.balances.len()
    );

    Ok(IngestOutcome {
        batch_id: batch.batch_id.clone(),
        charges: charges.len(),
        balances: normalized.balances.len(),
        raw_path: raw_path.to_path_buf(),
    })
}

// ==================== Resource scans (Insights) ====================

/// An account an Insights scan covers, and the regions to look in (none
/// for a provider scanned whole).
#[derive(Debug, Clone)]
pub struct ScanTarget {
    pub account: CloudAccount,
    pub regions: Vec<String>,
}

/// Whether this build can install the resource scanner at all.
pub fn scanner_supported() -> bool {
    crate::cloud::corkscrew::is_supported()
}

/// Whether the resource scanner is installed and ready.
pub fn scanner_installed() -> bool {
    crate::cloud::corkscrew::is_installed()
}

/// Download and install the resource scanner, unless it is there. Blocking.
pub fn install_scanner() -> Result<()> {
    crate::cloud::corkscrew::ensure_installed().map(|_| ())
}

/// The regions a scan covers: the ones chosen on the Insights page, or
/// every region AWS enables by default.
pub fn scan_regions() -> Vec<String> {
    crate::config::load_config()
        .ok()
        .and_then(|config| config.scan_regions)
        .filter(|regions| !regions.is_empty())
        .unwrap_or_else(|| {
            crate::model::AWS_DEFAULT_REGIONS
                .iter()
                .map(|r| r.to_string())
                .collect()
        })
}

/// The accounts an Insights scan covers — enabled, not demo, of a source
/// with a scanner plugin — a regional provider's each over
/// [`scan_regions`].
pub fn scan_targets() -> Result<Vec<ScanTarget>> {
    let regions = scan_regions();
    let mut targets = Vec::new();
    for account in crate::db::get_all_accounts()? {
        let Some(descriptor) = account.descriptor() else {
            continue;
        };
        if descriptor.inventory.is_none()
            || !account.enabled
            || account.id.starts_with(crate::ledger::demo::DEMO_PREFIX)
        {
            continue;
        }
        // A provider scanned whole takes no regions.
        let regions = match descriptor.inventory {
            Some(provider) if provider.regional => regions.clone(),
            _ => Vec::new(),
        };
        targets.push(ScanTarget { account, regions });
    }
    Ok(targets)
}

/// Scan one account with its keyring credentials. Returns where the scan
/// was written, for [`import_scans`]. Blocking; can take minutes.
pub fn scan_target(target: &ScanTarget) -> Result<PathBuf> {
    let descriptor = target
        .account
        .descriptor()
        .ok_or_else(|| anyhow!("No billing source registered for {}", target.account.name))?;
    let provider = descriptor.inventory.ok_or_else(|| {
        anyhow!(
            "{} has no resource scanner in this build",
            descriptor.display_name
        )
    })?;
    let credentials = crate::db::account_context(&target.account, descriptor)?;
    raw::check_path_segment(&target.account.id, "account id")?;
    let install = crate::cloud::corkscrew::ensure_installed()?;
    let out = crate::config::get_app_data_dir()?
        .join("inventory")
        .join(format!("scan-{}.duckdb", target.account.id));
    crate::cloud::corkscrew::scan(&install, provider, &credentials, &target.regions, &out)?;
    Ok(out)
}

/// Replace the inventory with the scans just taken, merged. Blocking.
pub fn import_scans(paths: &[PathBuf]) -> Result<crate::model::InventoryScope> {
    ledger::inventory::import_scans(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paid_key(account: &str) -> PeriodKey {
        PeriodKey::new("AWS", account, "2026-10")
    }

    /// A second caller reaching a period another is fetching waits for it,
    /// rather than fetching — and paying for — the same period alongside.
    #[test]
    fn a_period_being_fetched_holds_off_a_second_caller() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let key = paid_key("in-flight");
        let first = PeriodClaim::take(&key);

        let taken = Arc::new(AtomicBool::new(false));
        let second = std::thread::spawn({
            let key = key.clone();
            let taken = Arc::clone(&taken);
            move || {
                let _claim = PeriodClaim::take(&key);
                taken.store(true, Ordering::SeqCst);
            }
        });

        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!taken.load(Ordering::SeqCst), "took a period still held");

        drop(first);
        second.join().unwrap();
        assert!(taken.load(Ordering::SeqCst));

        // Released again once the second caller is done.
        drop(PeriodClaim::take(&key));
    }

    /// A fetch the provider answered and the ingest then lost is not
    /// bought again inside the refresh interval — the bill-safety half of
    /// a failed ingest.
    #[test]
    fn a_paid_fetch_that_failed_to_land_waits_out_the_interval() {
        let key = paid_key("paid-fails");
        let at = Utc::now();
        let window = Duration::hours(24);
        remember_paid_fetch(&key, at, Err("No space left on device".to_string()));

        let paid = unrecorded_paid_fetch(&key, at + Duration::minutes(15), window).unwrap();
        assert!(paid.failure.contains("No space left"));
        assert!(unrecorded_paid_fetch(&key, at + Duration::hours(25), window).is_none());
    }

    #[test]
    fn a_paid_fetch_that_landed_leaves_nothing_behind() {
        let key = paid_key("paid-lands");
        let at = Utc::now();
        remember_paid_fetch(&key, at, Err("transient".to_string()));
        remember_paid_fetch(&key, at, Ok(()));
        assert!(unrecorded_paid_fetch(&key, at, Duration::hours(24)).is_none());
    }

    /// Landing a period touches the ledger's global connection and the real
    /// raw directory, so the pipeline itself is exercised through each
    /// source's normalizer (which is pure by design) rather than from here.
    /// What is left is worth testing on its own: reading the file, and
    /// summarizing what an import did.
    struct TempFile(PathBuf);

    impl TempFile {
        fn holding(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir()
                .join(format!("cloudbridge-import-{}.csv", uuid::Uuid::new_v4()));
            std::fs::write(&path, bytes).expect("the temp file is writable");
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn a_utf8_export_is_read_with_its_names_intact() {
        let file = TempFile::holding("账期,产品名称\n2026-08,百炼\n".as_bytes());
        assert_eq!(
            read_text(&file.0, &TEST_FORMAT).unwrap(),
            "账期,产品名称\n2026-08,百炼\n"
        );
    }

    /// Both Chinese consoles can produce a GBK export. Guessing would
    /// mangle every product name in the bill, so it is refused with the one
    /// instruction that fixes it.
    #[test]
    fn a_file_that_is_not_utf8_says_what_to_do_about_it() {
        // 产品 in GBK, which is not valid UTF-8.
        let file = TempFile::holding(b"\xB2\xFA\xC6\xB7,1.00\n");

        let error = read_text(&file.0, &TEST_FORMAT).unwrap_err().to_string();
        assert!(error.contains("not UTF-8"), "{}", error);
        assert!(error.contains("UTF-8"), "{}", error);
    }

    #[test]
    fn a_missing_file_is_reported_against_its_path() {
        let error = read_text(Path::new("/nonexistent/bill.csv"), &TEST_FORMAT)
            .unwrap_err()
            .to_string();
        assert!(error.contains("/nonexistent/bill.csv"), "{}", error);
    }

    /// The format these tests read with.
    static TEST_FORMAT: BillFileFormat = BillFileFormat {
        display_name: "Test export",
        origin_hint: "",
        extensions: &["csv"],
        zip_member: Some("cost-"),
        part: "test",
        periods: |_| unreachable!(),
        normalize: |_| unreachable!(),
    };

    /// A zip of `members` as `(name, text)`, written to a temp file.
    fn zipped(members: &[(&str, &str)]) -> TempFile {
        use std::io::Write;

        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            for (name, text) in members {
                writer
                    .start_file(*name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(text.as_bytes()).unwrap();
            }
            writer.finish().unwrap();
        }
        TempFile::holding(&cursor.into_inner())
    }

    #[test]
    fn a_zip_yields_the_member_the_format_reads() {
        let file = zipped(&[
            ("amount-2026-08.csv", "date,amount\n2026-08-01,10787\n"),
            ("cost-2026-08.csv", "date,cost\n2026-08-01,1.50\n"),
        ]);

        assert_eq!(
            read_text(&file.0, &TEST_FORMAT).unwrap(),
            "date,cost\n2026-08-01,1.50\n"
        );
    }

    #[test]
    fn a_zip_without_the_member_names_what_it_does_hold() {
        let file = zipped(&[("amount-2026-08.csv", "date,amount\n")]);

        let error = read_text(&file.0, &TEST_FORMAT).unwrap_err().to_string();
        assert!(error.contains("cost-*"), "{}", error);
        assert!(error.contains("amount-2026-08.csv"), "{}", error);
    }

    #[test]
    fn an_import_summarizes_every_period_it_replaced() {
        let outcome = ImportOutcome {
            format: "Bill detail export",
            periods: vec![
                (
                    "2026-08".to_string(),
                    IngestOutcome {
                        batch_id: "b-1".to_string(),
                        charges: 12,
                        balances: 0,
                        raw_path: PathBuf::new(),
                    },
                ),
                (
                    "2026-09".to_string(),
                    IngestOutcome {
                        batch_id: "b-2".to_string(),
                        charges: 3,
                        balances: 0,
                        raw_path: PathBuf::new(),
                    },
                ),
            ],
        };

        assert_eq!(outcome.charges(), 15);
        assert_eq!(outcome.period_labels(), "2026-08, 2026-09");
    }
}
