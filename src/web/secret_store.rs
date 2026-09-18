//! Credentials, in a browser that has no keyring.
//!
//! The desktop keeps one keyring item per account and reads it once a
//! session. There is nothing to read here: the demo's accounts carry no
//! credentials and nothing in the demo signs a request, so every one of these
//! succeeds while storing nothing. They exist so that the account form, which
//! saves and clears credentials around a `db::save_account`, compiles and
//! behaves as if the keychain had taken them.

use anyhow::Result;

/// Accept a key pair, storing nothing.
pub fn store_account_secrets(
    _account_id: &str,
    _access_key_id: &str,
    _secret_access_key: &str,
) -> Result<()> {
    Ok(())
}

/// Read an account's key pair.
///
/// `Ok(None)` rather than an error, which is what the desktop returns for an
/// account whose secrets were never stored — the account list says "not set"
/// instead of failing to render.
pub fn get_account_secrets(_account_id: &str) -> Result<Option<(String, String)>> {
    Ok(None)
}

/// Forget an account's key pair. There is none.
pub fn delete_account_secrets(_account_id: &str) -> Result<()> {
    Ok(())
}
