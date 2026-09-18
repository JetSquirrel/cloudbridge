//! Billing sources, as far as a browser can know them.
//!
//! The desktop's registry carries a client builder, the places a provider's
//! credentials conventionally sit on the machine, and the parser for the
//! export its console produces. None of that crosses into a browser: there is
//! no keyring to read, no file to open and no request to sign.
//!
//! What is left is the metadata every page already renders — what a source is
//! called, which half of a credential it wants, whether it reports a cost or
//! a balance, and whether its bill export is a way in. Answering exactly that
//! is what keeps the source picker and the account form looking the same here
//! as they do on the desktop.

pub mod raw;
pub mod registry;

use anyhow::{anyhow, Result};

use crate::model::Reporting;

pub use crate::model::{
    access_key_hint, BillingPeriod, BudgetInfo, BudgetStatus, CloudAccount, SourceId,
};

/// The credentials a client would be built from.
///
/// Bundled into one struct for the same reason the desktop bundles it: so the
/// account form can hand one thing to a source and get a client back. Nothing
/// here authenticates with it.
pub struct SourceContext {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: Option<String>,
    /// Where the provider's own billing export lands, if the account is
    /// backed by one.
    pub export_uri: Option<String>,
}

/// A source of billing data.
///
/// The desktop's trait fetches a period and normalizes the response. Neither
/// is possible from a browser that fetches nothing, so this keeps the one
/// method the interface actually calls.
pub trait BillingSource: Send + Sync {
    /// Validate credentials against the provider.
    fn validate_credentials(&self) -> Result<bool>;
}

/// A client that explains itself rather than reaching a provider.
///
/// The account form's Validate button goes down the same path it does on the
/// desktop — build a client, ask it to check the credential — and this is
/// what answers. A button that silently did nothing would be worse than one
/// that says why it cannot.
struct UnreachableApi {
    provider: &'static str,
}

impl BillingSource for UnreachableApi {
    fn validate_credentials(&self) -> Result<bool> {
        Err(anyhow!(
            "The web demo cannot reach {} — it runs entirely on demo data",
            self.provider
        ))
    }
}

/// A provider's own bill export, as far as describing it needs.
///
/// The desktop's version also knows how to parse the file. Here it is only
/// ever named — in the import dialog and in the sentence that tells you a
/// source needs no credentials — so the parser is left out.
pub struct BillFileFormat {
    /// What the provider calls this export.
    pub display_name: &'static str,
    /// Where the export is produced, for the dialog's hint.
    pub origin_hint: &'static str,
    /// Filename extensions the import would accept.
    pub extensions: &'static [&'static str],
}

impl BillFileFormat {
    pub fn extension_hint(&self) -> String {
        self.extensions
            .iter()
            .map(|extension| format!(".{}", extension))
            .collect::<Vec<_>>()
            .join(" or ")
    }
}

/// Everything a page needs to know about a billing source.
pub struct SourceDescriptor {
    pub id: &'static str,
    pub display_name: &'static str,
    pub short_name: &'static str,
    /// Label for the public half of the credential.
    pub access_key_label: &'static str,
    /// Label for the secret half, or `None` when the source authenticates
    /// with a single key.
    pub secret_key_label: Option<&'static str>,
    /// Region applied when the user leaves the field blank, or `None` when
    /// the source has no notion of a region.
    pub default_region: Option<&'static str>,
    /// What the source reports; see [`Reporting`].
    pub reporting: Reporting,
    /// Whether the source has a billing API on the desktop.
    ///
    /// Recorded rather than acted on: the demo fetches nothing either way,
    /// but the account form asks for credentials in exactly the places the
    /// desktop would, which is the point of showing it at all.
    pub fetches_from_api: bool,
    /// How this source's own bill export would be read, or `None` for a
    /// source that publishes none. Same caveat as `fetches_from_api`.
    pub bill_file: Option<&'static BillFileFormat>,
    /// Where this source's credentials conventionally sit on the machine
    /// running the app, or `None` for a source with no such convention.
    ///
    /// Always `None` here. A browser cannot read a shell's environment or a
    /// provider's profile file, so the account form never offers to fill
    /// itself in from the system — which is why the field is still read: the
    /// button it gates is correctly absent rather than present and broken.
    pub local_credentials: Option<SystemCredentials>,
}

/// A place a source's credentials conventionally sit.
///
/// Never constructed, and deliberately carries nothing: the browser can reach
/// none of the places the desktop looks in, so there is nothing to describe.
pub struct SystemCredentials {
    /// Path or variable name, as the desktop would report it.
    pub origin: &'static str,
}

impl SourceDescriptor {
    pub fn source_id(&self) -> SourceId {
        SourceId::from(self.id)
    }

    /// Whether the credential form should require a secret key.
    pub fn needs_secret_key(&self) -> bool {
        self.secret_key_label.is_some()
    }

    pub fn secret_key_placeholder(&self) -> &'static str {
        self.secret_key_label
            .unwrap_or("(Not required, leave empty)")
    }

    pub fn region_placeholder(&self) -> String {
        match self.default_region {
            Some(region) => format!("Region (optional, default {})", region),
            None => "(Not required)".to_string(),
        }
    }

    /// Region to use for an account that stored none.
    pub fn region_or_default(&self, region: Option<String>) -> Option<String> {
        region.or_else(|| self.default_region.map(str::to_string))
    }

    /// Credentials found on this machine.
    ///
    /// Always `None`: a browser cannot see `~/.aws/credentials` or a shell's
    /// environment. The form's "fill from system" button says it found
    /// nothing rather than silently doing nothing, which is the honest
    /// answer to a question that has no answer here.
    pub fn credentials_from_system(&self) -> Option<FoundCredentials> {
        None
    }

    /// The places [`Self::credentials_from_system`] would have looked.
    pub fn credential_places(&self) -> Vec<String> {
        Vec::new()
    }

    /// Whether this source can be fetched from over the network.
    pub fn fetches_from_api(&self) -> bool {
        self.fetches_from_api
    }

    /// Build the client, or say why this source has none.
    ///
    /// The desktop signs a request with the credential it is handed. There is
    /// no request to sign here — but the Validate button on the account form
    /// comes through this door, so it gets a client that answers rather than
    /// one that does not exist.
    pub fn client(&self, _context: SourceContext) -> Result<Box<dyn BillingSource>> {
        if !self.fetches_from_api {
            return Err(anyhow!(
                "{} has no billing API in this build{}",
                self.display_name,
                match self.bill_file {
                    Some(format) => format!("; import its {} instead", format.display_name),
                    None => String::new(),
                }
            ));
        }

        Ok(Box::new(UnreachableApi {
            provider: self.display_name,
        }))
    }

    /// Whether this source reports a balance rather than a period cost.
    pub fn is_snapshot(&self) -> bool {
        matches!(self.reporting, Reporting::Snapshot)
    }

    /// Whether a bill export can be imported for this source.
    pub fn imports_bill_file(&self) -> bool {
        self.bill_file.is_some()
    }

    /// Whether an account of this source is usable without credentials.
    pub fn credentials_optional(&self) -> bool {
        self.imports_bill_file()
    }
}

/// Credentials read off the machine, as the account form expects them back.
///
/// Never constructed here; the shape exists so the form compiles against the
/// same type on both targets.
pub struct FoundCredentials {
    pub access_key: String,
    pub secret_key: Option<String>,
    pub region: Option<String>,
    /// Where the key came from, so the UI can say what it filled the form
    /// from.
    pub origin: String,
}

impl CloudAccount {
    /// The descriptor for this account's source, or `None` if the stored id
    /// is not registered in this build.
    pub fn descriptor(&self) -> Option<&'static SourceDescriptor> {
        self.source_id.descriptor()
    }
}

impl SourceId {
    /// The descriptor for this id, or `None` if no source is registered
    /// under it.
    pub fn descriptor(&self) -> Option<&'static SourceDescriptor> {
        get(self.as_str())
    }
}

const ALIYUN_FORMAT: BillFileFormat = BillFileFormat {
    display_name: "bill export",
    origin_hint: "Expenses and Costs → Bill Details → Export",
    extensions: &["csv"],
};

const DEEPSEEK_FORMAT: BillFileFormat = BillFileFormat {
    display_name: "usage export",
    origin_hint: "Usage → Download",
    extensions: &["zip", "csv"],
};

const VOLCENGINE_FORMAT: BillFileFormat = BillFileFormat {
    display_name: "bill export",
    origin_hint: "Billing → Bill Details → Export",
    extensions: &["csv"],
};

const OPENAI_FORMAT: BillFileFormat = BillFileFormat {
    display_name: "usage export",
    origin_hint: "Usage → Export",
    extensions: &["csv"],
};

const ANTHROPIC_FORMAT: BillFileFormat = BillFileFormat {
    display_name: "usage export",
    origin_hint: "Usage or Cost → Export",
    extensions: &["csv"],
};

/// Every source, in the order the picker shows them.
///
/// The same six the desktop registers, with the same names and the same
/// answers to every question above.
static SOURCES: &[SourceDescriptor] = &[
    SourceDescriptor {
        id: "AWS",
        display_name: "Amazon Web Services",
        short_name: "AWS",
        access_key_label: "Access Key ID",
        secret_key_label: Some("Secret Access Key"),
        default_region: Some("us-east-1"),
        reporting: Reporting::Periodic,
        fetches_from_api: true,
        bill_file: None,
        local_credentials: None,
    },
    SourceDescriptor {
        id: "Aliyun",
        display_name: "Alibaba Cloud",
        short_name: "Aliyun",
        access_key_label: "AccessKey ID",
        secret_key_label: Some("AccessKey Secret"),
        default_region: Some("cn-hangzhou"),
        reporting: Reporting::Periodic,
        fetches_from_api: true,
        bill_file: Some(&ALIYUN_FORMAT),
        local_credentials: None,
    },
    SourceDescriptor {
        id: "DeepSeek",
        display_name: "DeepSeek",
        short_name: "DeepSeek",
        access_key_label: "API Key",
        secret_key_label: None,
        default_region: None,
        reporting: Reporting::Snapshot,
        fetches_from_api: true,
        bill_file: Some(&DEEPSEEK_FORMAT),
        local_credentials: None,
    },
    SourceDescriptor {
        id: "Volcengine",
        display_name: "Volcengine (火山引擎)",
        short_name: "Volcengine",
        access_key_label: "Access Key ID",
        secret_key_label: Some("Secret Access Key"),
        default_region: None,
        reporting: Reporting::Periodic,
        fetches_from_api: false,
        bill_file: Some(&VOLCENGINE_FORMAT),
        local_credentials: None,
    },
    SourceDescriptor {
        id: "OpenAI",
        display_name: "OpenAI",
        short_name: "OpenAI",
        access_key_label: "Admin API Key",
        secret_key_label: None,
        default_region: None,
        reporting: Reporting::Periodic,
        fetches_from_api: false,
        bill_file: Some(&OPENAI_FORMAT),
        local_credentials: None,
    },
    SourceDescriptor {
        id: "Anthropic",
        display_name: "Anthropic (Claude)",
        short_name: "Claude",
        access_key_label: "Admin API Key",
        secret_key_label: None,
        default_region: None,
        reporting: Reporting::Periodic,
        fetches_from_api: false,
        bill_file: Some(&ANTHROPIC_FORMAT),
        local_credentials: None,
    },
];

pub fn all() -> &'static [SourceDescriptor] {
    SOURCES
}

/// The descriptor registered under `id`, if any.
pub fn get(id: &str) -> Option<&'static SourceDescriptor> {
    SOURCES.iter().find(|source| source.id == id)
}

/// The source a fresh account form opens on.
pub fn default_source() -> &'static SourceDescriptor {
    &SOURCES[0]
}
