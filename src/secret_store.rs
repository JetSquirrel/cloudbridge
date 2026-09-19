use anyhow::{anyhow, Result};
use keyring::Entry;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};

const SERVICE_NAME: &str = "CloudBridge";

/// Credentials already read from (or written to) the keychain this
/// session. On macOS every keychain read can raise a system password
/// prompt — the item's access control names the binary that stored it,
/// and a rebuild is a new binary — so a run that fetches several
/// billing periods must not go back to the keychain for each one.
static CACHE: LazyLock<Mutex<HashMap<String, (String, String)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cache() -> Result<MutexGuard<'static, HashMap<String, (String, String)>>> {
    CACHE
        .lock()
        .map_err(|e| anyhow!("Failed to lock the credential cache: {}", e))
}

/// The pair as one item. Two entries meant two prompts per read; a single
/// JSON value is one.
fn entry(account_id: &str) -> Result<Entry> {
    Ok(Entry::new(SERVICE_NAME, account_id)?)
}

fn legacy_entries(account_id: &str) -> Result<(Entry, Entry)> {
    Ok((
        Entry::new(&format!("{}:ak", SERVICE_NAME), account_id)?,
        Entry::new(&format!("{}:sk", SERVICE_NAME), account_id)?,
    ))
}

pub fn store_account_secrets(
    account_id: &str,
    access_key_id: &str,
    secret_access_key: &str,
) -> Result<()> {
    entry(account_id)?.set_password(
        &serde_json::json!({
            "ak": access_key_id,
            "sk": secret_access_key,
        })
        .to_string(),
    )?;

    // The split entries this replaces, if an older version left them.
    if let Ok((ak, sk)) = legacy_entries(account_id) {
        let _ = ak.delete_password();
        let _ = sk.delete_password();
    }

    cache()?.insert(
        account_id.to_string(),
        (access_key_id.to_string(), secret_access_key.to_string()),
    );

    Ok(())
}

pub fn get_account_secrets(account_id: &str) -> Result<Option<(String, String)>> {
    if let Some(pair) = cache()?.get(account_id) {
        return Ok(Some(pair.clone()));
    }

    // `or` would read the legacy entries even when the combined one
    // matched — two keychain reads this exists to avoid.
    let pair = match read_combined(account_id)? {
        Some(pair) => Some(pair),
        None => read_legacy(account_id)?,
    };

    if let Some((ref ak, ref sk)) = pair {
        cache()?.insert(account_id.to_string(), (ak.clone(), sk.clone()));
    }

    Ok(pair)
}

fn read_combined(account_id: &str) -> Result<Option<(String, String)>> {
    let password = match entry(account_id)?.get_password() {
        Ok(password) => password,
        Err(_) => return Ok(None),
    };

    let value: serde_json::Value = serde_json::from_str(&password)?;
    match (value["ak"].as_str(), value["sk"].as_str()) {
        (Some(ak), Some(sk)) => Ok(Some((ak.to_string(), sk.to_string()))),
        _ => Ok(None),
    }
}

/// The two-entry format versions before this one wrote. Found secrets move
/// to the single-entry form, so the second legacy read is also the last.
fn read_legacy(account_id: &str) -> Result<Option<(String, String)>> {
    let (ak_entry, sk_entry) = legacy_entries(account_id)?;

    match (ak_entry.get_password(), sk_entry.get_password()) {
        (Ok(ak), Ok(sk)) => {
            if let Err(e) = store_account_secrets(account_id, &ak, &sk) {
                tracing::warn!("Could not re-store the key pair for {}: {}", account_id, e);
            }
            Ok(Some((ak, sk)))
        }
        _ => Ok(None),
    }
}

pub fn delete_account_secrets(account_id: &str) -> Result<()> {
    let _ = entry(account_id)?.delete_password();

    if let Ok((ak, sk)) = legacy_entries(account_id) {
        let _ = ak.delete_password();
        let _ = sk.delete_password();
    }

    cache()?.remove(account_id);

    Ok(())
}
