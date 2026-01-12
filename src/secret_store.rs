use anyhow::Result;
use keyring::Entry;

const SERVICE_NAME: &str = "CloudBridge";

pub fn store_account_secrets(account_id: &str, access_key_id: &str, secret_access_key: &str) -> Result<()> {
    let ak = Entry::new(&format!("{}:ak", SERVICE_NAME), account_id)?;
    ak.set_password(access_key_id)?;

    let sk = Entry::new(&format!("{}:sk", SERVICE_NAME), account_id)?;
    sk.set_password(secret_access_key)?;

    Ok(())
}

pub fn get_account_secrets(account_id: &str) -> Result<Option<(String, String)>> {
    let ak_entry = Entry::new(&format!("{}:ak", SERVICE_NAME), account_id)?;
    let sk_entry = Entry::new(&format!("{}:sk", SERVICE_NAME), account_id)?;

    match (ak_entry.get_password(), sk_entry.get_password()) {
        (Ok(a), Ok(s)) => Ok(Some((a, s))),
        _ => Ok(None),
    }
}

pub fn delete_account_secrets(account_id: &str) -> Result<()> {
    let ak = Entry::new(&format!("{}:ak", SERVICE_NAME), account_id)?;
    let _ = ak.delete_password();

    let sk = Entry::new(&format!("{}:sk", SERVICE_NAME), account_id)?;
    let _ = sk.delete_password();

    Ok(())
}
