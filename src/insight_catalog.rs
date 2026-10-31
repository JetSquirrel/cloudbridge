//! The community catalog of Insights rules: what `insights-rules/` in this
//! repository builds, installed into the folder [`insight_scripts`] reads.
//!
//! CI publishes `index.json` and one `.js` per rule to the rolling
//! `insight-rules` release. Nothing here runs on its own: the index is
//! fetched when someone opens the catalog in Settings, a rule is downloaded
//! when someone installs or updates it, and every download is checked
//! against the SHA-256 the index names before it is written. What a rule
//! can do once installed is bounded by the script sandbox, not by trust in
//! the download: it sees only the bill it is handed and has no network.
//!
//! What came from the catalog is recorded in `insights/catalog.json`, so a
//! rule's status can tell "an update is available" from "you edited it":
//! an update never overwrites a local edit without being asked.
//!
//! [`insight_scripts`]: crate::insight_scripts

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::insight_scripts::{rules_dir, API_VERSION};

/// Where the catalog is published.
pub const CATALOG_BASE: &str =
    "https://github.com/JetSquirrel/cloudbridge/releases/download/insight-rules";
/// The page a person reads rules and contributes on.
pub const CATALOG_HOME: &str =
    "https://github.com/JetSquirrel/cloudbridge/tree/main/insights-rules";

const RECORD_FILE: &str = "catalog.json";
const INDEX_LIMIT: u64 = 1024 * 1024;
const RULE_LIMIT: u64 = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);

/// `index.json`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogIndex {
    pub api_version: u32,
    pub rules: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub id: String,
    pub version: String,
    pub title: String,
    pub explanation: String,
    pub author: String,
    #[serde(default)]
    pub providers: Vec<String>,
    pub api_version: u32,
    pub file: String,
    pub size: u64,
    pub sha256: String,
}

/// What `catalog.json` says was installed from the catalog, by id.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct InstallRecord {
    pub rules: BTreeMap<String, InstalledFrom>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstalledFrom {
    pub version: String,
    pub sha256: String,
}

/// A `.js` file in the rules folder.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalRule {
    pub id: String,
    pub sha256: String,
}

/// Where one rule stands, against the catalog and the folder.
#[derive(Debug, Clone, PartialEq)]
pub enum RuleStatus {
    /// In the catalog, not in the folder.
    Available,
    /// Installed from the catalog, and the catalog has nothing newer.
    UpToDate,
    /// Installed from the catalog as `installed`; the catalog has another.
    UpdateAvailable { installed: String },
    /// Installed from the catalog, then changed in the folder.
    EditedLocally,
    /// In the folder and not from the catalog: someone's own rule, or one
    /// whose id the catalog now also uses.
    Local,
    /// Installed from the catalog, which no longer lists it.
    Withdrawn,
    /// In the catalog, for a newer app.
    NeedsNewerApp,
}

/// One row of the Settings list.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleRow {
    pub id: String,
    pub entry: Option<CatalogEntry>,
    pub installed: bool,
    pub status: RuleStatus,
}

/// The catalog's rules and the folder's, side by side: catalog rules by
/// title, then the folder's own by id.
pub fn rows(
    index: Option<&CatalogIndex>,
    local: &[LocalRule],
    record: &InstallRecord,
) -> Vec<RuleRow> {
    let mut rows = Vec::new();
    let mut catalog: Vec<&CatalogEntry> =
        index.map(|i| i.rules.iter().collect()).unwrap_or_default();
    catalog.sort_by(|a, b| a.title.cmp(&b.title));
    for entry in catalog {
        let file = local.iter().find(|l| l.id == entry.id);
        let from = record.rules.get(&entry.id);
        let status = match (file, from) {
            _ if entry.api_version > API_VERSION && file.is_none() => RuleStatus::NeedsNewerApp,
            (None, _) => RuleStatus::Available,
            (Some(_), None) => RuleStatus::Local,
            (Some(file), Some(from)) if file.sha256 != from.sha256 => RuleStatus::EditedLocally,
            (Some(_), Some(_)) if entry.api_version > API_VERSION => RuleStatus::NeedsNewerApp,
            (Some(_), Some(from)) if from.sha256 != entry.sha256 => RuleStatus::UpdateAvailable {
                installed: from.version.clone(),
            },
            (Some(_), Some(_)) => RuleStatus::UpToDate,
        };
        rows.push(RuleRow {
            id: entry.id.clone(),
            entry: Some(entry.clone()),
            installed: file.is_some(),
            status,
        });
    }
    let mut own: Vec<&LocalRule> = local
        .iter()
        .filter(|l| !rows.iter().any(|r| r.id == l.id))
        .collect();
    own.sort_by(|a, b| a.id.cmp(&b.id));
    for rule in own {
        // Only an index that was read can say a rule is gone from it.
        let withdrawn = index.is_some() && record.rules.contains_key(&rule.id);
        rows.push(RuleRow {
            id: rule.id.clone(),
            entry: None,
            installed: true,
            status: if withdrawn {
                RuleStatus::Withdrawn
            } else {
                RuleStatus::Local
            },
        });
    }
    rows
}

/// Whether `id` is something a rule may be called, and so a safe file
/// name: what `build.mjs` enforces, enforced again on what is downloaded.
pub fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

// ==================== The folder ====================

/// The `.js` files in the rules folder, and what the catalog installed.
pub fn read_local() -> Result<(Vec<LocalRule>, InstallRecord)> {
    read_local_in(&rules_dir()?)
}

fn read_local_in(dir: &Path) -> Result<(Vec<LocalRule>, InstallRecord)> {
    let mut rules = Vec::new();
    if dir.exists() {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".js"))
            else {
                continue;
            };
            if path.is_file() {
                rules.push(LocalRule {
                    id: id.to_string(),
                    sha256: sha256_hex(&std::fs::read(&path)?),
                });
            }
        }
    }
    rules.sort_by(|a, b| a.id.cmp(&b.id));
    let record = match std::fs::read_to_string(dir.join(RECORD_FILE)) {
        Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
        Err(_) => InstallRecord::default(),
    };
    Ok((rules, record))
}

fn write_record(dir: &Path, record: &InstallRecord) -> Result<()> {
    write_atomically(
        &dir.join(RECORD_FILE),
        serde_json::to_string_pretty(record)?.as_bytes(),
    )
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("Writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("Writing {}", path.display()))?;
    Ok(())
}

/// Write `bytes` as `entry`'s rule, after checking they are what the index
/// says, and record where it came from.
fn install_bytes(dir: &Path, entry: &CatalogEntry, bytes: &[u8]) -> Result<()> {
    if !valid_id(&entry.id) || entry.file != format!("{}.js", entry.id) {
        bail!("The catalog names an unusable rule file: {:?}", entry.file);
    }
    let got = sha256_hex(bytes);
    if !got.eq_ignore_ascii_case(&entry.sha256) {
        bail!(
            "{} did not match the catalog's checksum (got {got}); not installed",
            entry.file
        );
    }
    std::fs::create_dir_all(dir).with_context(|| format!("Creating {}", dir.display()))?;
    write_atomically(&dir.join(&entry.file), bytes)?;
    let (_, mut record) = read_local_in(dir)?;
    record.rules.insert(
        entry.id.clone(),
        InstalledFrom {
            version: entry.version.clone(),
            sha256: got,
        },
    );
    write_record(dir, &record)
}

fn remove_in(dir: &Path, id: &str) -> Result<()> {
    if !valid_id(id) {
        bail!("Not a rule id: {id:?}");
    }
    let path = dir.join(format!("{id}.js"));
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("Removing {}", path.display()))?;
    }
    let (_, mut record) = read_local_in(dir)?;
    if record.rules.remove(id).is_some() {
        write_record(dir, &record)?;
    }
    Ok(())
}

/// Delete a rule from the folder, whoever wrote it.
pub fn remove(id: &str) -> Result<()> {
    remove_in(&rules_dir()?, id)
}

// ==================== The network ====================

fn get(url: &str, limit: u64) -> Result<Vec<u8>> {
    let agent = crate::cloud::http::agent(TIMEOUT);
    let mut response = agent
        .get(url)
        .call()
        .map_err(|e| anyhow!("Could not reach the rule catalog: {e}"))?;
    let status = response.status().as_u16();
    if status != 200 {
        bail!("The rule catalog answered {status} for {url}");
    }
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|e| anyhow!("Could not read {url}: {e}"))
}

/// The catalog's index. Blocking.
pub fn fetch_index() -> Result<CatalogIndex> {
    let bytes = get(&format!("{CATALOG_BASE}/index.json"), INDEX_LIMIT)?;
    serde_json::from_slice(&bytes).context("The rule catalog's index is not one this app reads")
}

/// Download `entry`, check it, and install it, replacing what is there.
/// Blocking.
pub fn install(entry: &CatalogEntry) -> Result<()> {
    if entry.api_version > API_VERSION {
        bail!("{} needs a newer CloudBridge", entry.title);
    }
    if !valid_id(&entry.id) {
        bail!("The catalog names an unusable rule id: {:?}", entry.id);
    }
    let bytes = get(&format!("{CATALOG_BASE}/{}", entry.file), RULE_LIMIT)?;
    install_bytes(&rules_dir()?, entry, &bytes)
}

/// The rules folder, created if it is not there yet, for "Open folder".
pub fn ensure_rules_dir() -> Result<PathBuf> {
    let dir = rules_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("Creating {}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: &[u8] = b"export function evaluate() { return []; }";

    fn entry(id: &str, version: &str, bytes: &[u8]) -> CatalogEntry {
        CatalogEntry {
            id: id.to_string(),
            version: version.to_string(),
            title: id.to_string(),
            explanation: String::new(),
            author: "someone".to_string(),
            providers: vec!["AWS".to_string()],
            api_version: API_VERSION,
            file: format!("{id}.js"),
            size: bytes.len() as u64,
            sha256: sha256_hex(bytes),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cloudbridge-catalog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn rule_ids_are_safe_file_names() {
        for good in ["gp2-volumes", "a", "x9"] {
            assert!(valid_id(good), "{good}");
        }
        for bad in [
            "",
            "-x",
            "Up",
            "../x",
            "a/b",
            "a.js",
            "a b",
            &"a".repeat(65),
        ] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    /// Installing writes the file and records it; a download that does not
    /// match its checksum, or names a path, writes nothing.
    #[test]
    fn only_a_matching_download_is_installed() {
        let dir = temp_dir("install");
        let good = entry("gp2-volumes", "1.0.0", RULE);
        install_bytes(&dir, &good, RULE).unwrap();
        let (local, record) = read_local_in(&dir).unwrap();
        assert_eq!(local.len(), 1);
        assert_eq!(record.rules["gp2-volumes"].version, "1.0.0");

        let tampered = install_bytes(&dir, &entry("other", "1.0.0", RULE), b"evil");
        assert!(tampered.unwrap_err().to_string().contains("checksum"));
        let mut escape = entry("x", "1.0.0", RULE);
        escape.file = "../x.js".to_string();
        assert!(install_bytes(&dir, &escape, RULE).is_err());
        assert_eq!(read_local_in(&dir).unwrap().0.len(), 1);

        remove_in(&dir, "gp2-volumes").unwrap();
        let (local, record) = read_local_in(&dir).unwrap();
        assert!(local.is_empty() && record.rules.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_rule_s_status_tells_an_update_from_an_edit() {
        let v1 = entry("a", "1.0.0", b"v1");
        let v2 = entry("a", "1.1.0", b"v2");
        let installed_v1 = InstallRecord {
            rules: [(
                "a".to_string(),
                InstalledFrom {
                    version: "1.0.0".to_string(),
                    sha256: sha256_hex(b"v1"),
                },
            )]
            .into(),
        };
        let file = |bytes: &[u8]| LocalRule {
            id: "a".to_string(),
            sha256: sha256_hex(bytes),
        };
        let index = |e: &CatalogEntry| CatalogIndex {
            api_version: API_VERSION,
            rules: vec![e.clone()],
        };
        let status = |index: Option<&CatalogIndex>, local: &[LocalRule], record: &InstallRecord| {
            rows(index, local, record)
                .into_iter()
                .map(|r| r.status)
                .collect::<Vec<_>>()
        };

        let none = InstallRecord::default();
        assert_eq!(
            status(Some(&index(&v1)), &[], &none),
            [RuleStatus::Available]
        );
        assert_eq!(
            status(Some(&index(&v1)), &[file(b"v1")], &installed_v1),
            [RuleStatus::UpToDate]
        );
        assert_eq!(
            status(Some(&index(&v2)), &[file(b"v1")], &installed_v1),
            [RuleStatus::UpdateAvailable {
                installed: "1.0.0".to_string()
            }]
        );
        assert_eq!(
            status(Some(&index(&v2)), &[file(b"mine")], &installed_v1),
            [RuleStatus::EditedLocally]
        );
        assert_eq!(
            status(Some(&index(&v1)), &[file(b"mine")], &none),
            [RuleStatus::Local]
        );
        let other = entry("b", "1.0.0", b"b");
        assert_eq!(
            status(Some(&index(&other)), &[file(b"v1")], &installed_v1),
            [RuleStatus::Available, RuleStatus::Withdrawn]
        );
        // Without the index, nothing can be called withdrawn.
        assert_eq!(
            status(None, &[file(b"v1")], &installed_v1),
            [RuleStatus::Local]
        );

        let mut future = entry("a", "2.0.0", b"v3");
        future.api_version = API_VERSION + 1;
        assert_eq!(
            status(Some(&index(&future)), &[], &none),
            [RuleStatus::NeedsNewerApp]
        );
        assert_eq!(
            status(Some(&index(&future)), &[file(b"v1")], &installed_v1),
            [RuleStatus::NeedsNewerApp]
        );
    }
}
