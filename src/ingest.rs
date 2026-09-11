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
use std::path::{Path, PathBuf};

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
/// Explorer bills per request).
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

    let mut outcome = RefreshOutcome::default();
    for period in periods {
        if !force && is_fresh(account, &period, now)? {
            tracing::debug!(
                "Skipping {} {}: ingested within the freshness window",
                account.name,
                period.label()
            );
            outcome.skipped_fresh.push(period.label());
            continue;
        }

        let ingested = ingest_period(account, &period)?;
        outcome.ingested.push((period.label(), ingested));
    }

    if !outcome.ingested.is_empty() {
        if let Err(e) = crate::db::mark_account_synced(&account.id, now) {
            tracing::warn!("Could not record the sync time for {}: {}", account.name, e);
        }
        evaluate_alerts();
    }

    Ok(outcome)
}

/// Whether a period was ingested recently enough to leave alone.
fn is_fresh(account: &CloudAccount, period: &BillingPeriod, now: DateTime<Utc>) -> Result<bool> {
    let hours = i64::from(
        crate::config::load_config()
            .map(|settings| settings.refresh_interval_hours)
            .unwrap_or(crate::config::DEFAULT_REFRESH_INTERVAL_HOURS),
    );

    Ok(query::last_ingest(&period_key(account, period))?
        .is_some_and(|ingested_at| now - ingested_at < Duration::hours(hours)))
}

/// Fetch one account's billing period and land it in the ledger.
pub fn ingest_period(account: &CloudAccount, period: &BillingPeriod) -> Result<IngestOutcome> {
    let descriptor = account.descriptor().ok_or_else(|| {
        anyhow!(
            "No billing source registered under '{}'",
            account.source_id.as_str()
        )
    })?;

    let source = descriptor.client(crate::db::account_context(account, descriptor)?)?;
    let parts = source.fetch(period)?;

    let batch = RawBatch {
        provider: descriptor.id.to_string(),
        account_id: account.id.clone(),
        period: *period,
        batch_id: ledger::new_batch_id(),
        fetched_at: Utc::now(),
        parts,
    };

    let raw_path = persist(&batch)?;
    let normalized = source.normalize(&batch)?;
    record(&batch, &normalized, &raw_path, Channel::Api)
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

#[cfg(test)]
mod tests {
    use super::*;

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
