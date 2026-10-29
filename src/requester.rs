//! FOCUS 1.5 requester attribution: who, and which key, incurred a charge.
//!
//! FOCUS 1.5 adds three columns for it. `PrincipalId` is the identity
//! access was granted to — a user, a service account. `CredentialId` is
//! what authenticated the request — an API key, a session — by its
//! published identifier, never the secret itself. `RequesterDetails` is a
//! JSON array of `{"key", "value"}` entries describing them, a
//! `"Principal"` entry for the first and a `"Credential"` entry for the
//! second, each with the `Type` and `Name` FOCUS defines.
//!
//! The model-provider exports are where this shows: OpenAI's usage can be
//! grouped by user and by API key, Anthropic's by API key. Pure and
//! target-independent.

use serde_json::{json, Value};

/// The requester columns of one charge.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Requester {
    pub principal_id: Option<String>,
    pub credential_id: Option<String>,
    pub requester_details: Option<String>,
}

/// FOCUS's description of a requester, from what an export reports.
///
/// An entry exists only beside its identifier, as FOCUS requires: a key
/// name with no key id describes nothing a row can be grouped by, so it
/// is not written. An identifier that looks like a secret — a masked API
/// key such as DeepSeek's `sk-***32f6` — is refused, since FOCUS forbids a
/// value that authenticates a request.
pub fn describe(
    principal_id: Option<&str>,
    principal_type: &str,
    credential_id: Option<&str>,
    credential_name: Option<&str>,
    credential_type: &str,
) -> Requester {
    let clean = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty() && !looks_like_a_secret(value))
            .map(str::to_string)
    };
    let principal_id = clean(principal_id);
    let credential_id = clean(credential_id);

    let mut entries = Vec::new();
    if principal_id.is_some() {
        entries.push(json!({ "key": "Principal", "value": { "Type": principal_type } }));
    }
    if credential_id.is_some() {
        let mut value = json!({ "Type": credential_type });
        if let Some(name) = credential_name
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            value["Name"] = Value::String(name.to_string());
        }
        entries.push(json!({ "key": "Credential", "value": value }));
    }

    Requester {
        principal_id,
        credential_id,
        requester_details: (!entries.is_empty()).then(|| Value::Array(entries).to_string()),
    }
}

/// Whether a value is, or masks, an API key rather than naming one: the
/// `sk-` prefix OpenAI, Anthropic and DeepSeek keys share, or a run of
/// asterisks.
fn looks_like_a_secret(value: &str) -> bool {
    value.starts_with("sk-") || value.contains("***")
}

#[cfg(test)]
mod tests {
    use super::describe;

    #[test]
    fn a_key_and_its_user_are_described_as_focus_spells_them() {
        let requester = describe(
            Some("user-abc"),
            "User",
            Some("key_123"),
            Some("ci-pipeline"),
            "API Key",
        );
        assert_eq!(requester.principal_id.as_deref(), Some("user-abc"));
        assert_eq!(requester.credential_id.as_deref(), Some("key_123"));
        // Read back as JSON: the object's key order is serde_json's.
        let details: serde_json::Value =
            serde_json::from_str(requester.requester_details.as_deref().unwrap()).unwrap();
        assert_eq!(details[0]["key"], "Principal");
        assert_eq!(details[0]["value"]["Type"], "User");
        assert_eq!(details[1]["key"], "Credential");
        assert_eq!(details[1]["value"]["Type"], "API Key");
        assert_eq!(details[1]["value"]["Name"], "ci-pipeline");
    }

    #[test]
    fn a_name_without_an_identifier_describes_nothing() {
        let requester = describe(None, "User", None, Some("dsh"), "API Key");
        assert_eq!(requester, super::Requester::default());
    }

    #[test]
    fn a_secret_is_never_an_identifier() {
        let requester = describe(None, "User", Some("sk-***32f6"), Some("dsh"), "API Key");
        assert_eq!(requester.credential_id, None);
        assert_eq!(requester.requester_details, None);

        let requester = describe(Some("  "), "User", Some("sk-proj-abc"), None, "API Key");
        assert_eq!(requester, super::Requester::default());
    }

    #[test]
    fn a_key_alone_has_only_a_credential_entry() {
        let requester = describe(None, "User", Some("apikey_01"), None, "API Key");
        let details: serde_json::Value =
            serde_json::from_str(requester.requester_details.as_deref().unwrap()).unwrap();
        assert_eq!(details.as_array().unwrap().len(), 1);
        assert_eq!(details[0]["key"], "Credential");
        assert!(details[0]["value"].get("Name").is_none());
    }
}
