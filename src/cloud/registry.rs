//! Billing source registry.
//!
//! A source is a row in [`SOURCES`], not an enum variant. Adding one means
//! adding a [`SourceDescriptor`] and a parser — nothing else in the codebase
//! learns its name. That matters because the roadmap adds model-provider
//! APIs, token plans and local agent usage on top of the public clouds, and
//! the previous `CloudProvider` enum had to be matched in 48 places.
//!
//! Callers ask the descriptor what a source can do rather than who it is:
//! whether it builds an API client, whether its bill export can be
//! imported, whether it reports a balance — not whether it is called
//! DeepSeek.

use anyhow::{anyhow, Result};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::billfile::{self, BillFileFormat};
use super::{aliyun::AliyunCloudService, aws::AwsCloudService, deepseek::DeepSeekService};
use super::{BillingSource, SourceContext};

/// What a source reports, and therefore what there is to refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reporting {
    /// Cost accrued over a period. Refreshing means re-fetching the current
    /// billing period and, for a while after it ends, the one before it.
    Periodic,
    /// A point-in-time balance. There is no period cost and no history to
    /// backfill, so a refresh reads the balance as it stands.
    Snapshot,
}

/// Identifier of a billing source.
///
/// Persisted verbatim in the `cloud_accounts` table, so these strings are
/// part of the on-disk format and must not be renamed without a migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(String);

impl SourceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The descriptor for this id, or `None` if no source is registered
    /// under it — an account written by a newer build, or by a build that
    /// still had the Azure and GCP enum variants.
    pub fn descriptor(&self) -> Option<&'static SourceDescriptor> {
        get(&self.0)
    }
}

impl From<&str> for SourceId {
    fn from(id: &str) -> Self {
        Self(id.to_string())
    }
}

impl From<String> for SourceId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// Where a source's credentials conventionally sit on this machine.
///
/// Two places, both in the provider's own naming: the variables its tooling
/// reads, and the profile files its CLI writes. They are consulted in that
/// order, so a shell that exports a key overrides a stale file without
/// hiding the rest of it.
pub struct LocalCredentials {
    pub env: EnvCredentials,
    /// Profile files to read, in order. Empty for a source whose tooling
    /// keeps nothing on disk.
    pub files: &'static [ProfileFile],
}

/// The environment variables a source's credentials can be read from.
///
/// Each slot lists the variables the provider's own tooling reads, most
/// canonical first.
pub struct EnvCredentials {
    pub access_key: &'static [&'static str],
    /// Empty for a source that authenticates with a single key.
    pub secret_key: &'static [&'static str],
    pub region: &'static [&'static str],
}

/// An ini-style credentials file with one section per profile, as written
/// by `aws configure`.
pub struct ProfileFile {
    /// Location relative to the home directory, in the provider's layout.
    pub path: &'static str,
    /// Variable that relocates the file, if the tooling has one.
    pub path_var: Option<&'static str>,
    /// Variable naming the profile to read; `default` when it is unset.
    pub profile_var: Option<&'static str>,
    /// How this file titles a profile's section: `[default]` in
    /// `~/.aws/credentials`, but `[profile work]` in `~/.aws/config`. The
    /// bare name is tried as well, because the default profile keeps it
    /// even in a file that prefixes the others.
    pub section_prefix: &'static str,
    pub access_key: &'static str,
    pub secret_key: &'static str,
    pub region: &'static str,
}

/// Credentials found on this machine, ready to fill a form with.
pub struct FoundCredentials {
    pub access_key: String,
    pub secret_key: Option<String>,
    pub region: Option<String>,
    /// Where the key came from — a variable, or a file and profile — so the
    /// UI can say what it filled the form from.
    pub origin: String,
}

/// What one place holds, any part of which may be missing.
#[derive(Default)]
struct Slots {
    access_key: Option<String>,
    secret_key: Option<String>,
    region: Option<String>,
}

/// A value that is actually there, rather than blank or whitespace.
fn nonempty(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The `key = value` pairs under one section of an ini-style file, or
/// `None` when the file has no such section.
///
/// Deliberately minimal — section headers, assignments, and `#` or `;`
/// comments, which is all `aws configure` writes. A nested subsection's
/// keys are read as ordinary ones and simply never asked for.
fn section_values(contents: &str, section: &str) -> Option<Vec<(String, String)>> {
    let mut values = Vec::new();
    let mut found = false;
    let mut inside = false;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            inside = header.trim() == section;
            found |= inside;
            continue;
        }

        if inside {
            if let Some((key, value)) = line.split_once('=') {
                values.push((key.trim().to_string(), value.trim().to_string()));
            }
        }
    }

    found.then_some(values)
}

impl LocalCredentials {
    /// Read what this machine has, or `None` when it holds no key.
    ///
    /// A missing secret or region is not a failure — the user can fill in
    /// the rest — but without a key there is nothing to offer.
    pub fn read(&self) -> Option<FoundCredentials> {
        let home = BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());

        self.read_with(
            |name| std::env::var(name).ok(),
            home.as_deref(),
            |path| std::fs::read_to_string(path).ok(),
        )
    }

    /// The places [`Self::read`] looks, in order, so a fill that finds
    /// nothing can say where it looked.
    pub fn places(&self) -> Vec<String> {
        let env = |name: &str| std::env::var(name).ok();

        let mut places: Vec<String> = self
            .env
            .access_key
            .iter()
            .map(|name| name.to_string())
            .collect();
        places.extend(self.files.iter().map(|file| file.display_path(env)));
        places
    }

    /// [`Self::read`] against an arbitrary environment and filesystem, so
    /// the precedence rules can be tested without a configured machine.
    fn read_with(
        &self,
        env: impl Fn(&str) -> Option<String>,
        home: Option<&Path>,
        read_file: impl Fn(&Path) -> Option<String>,
    ) -> Option<FoundCredentials> {
        let first_set = |names: &[&str]| names.iter().find_map(|name| env(name).and_then(nonempty));

        let (env_var, env_key) = match self
            .env
            .access_key
            .iter()
            .find_map(|name| env(name).and_then(nonempty).map(|value| (*name, value)))
        {
            Some((name, value)) => (name, Some(value)),
            None => ("the environment", None),
        };

        let mut places = vec![(
            env_var.to_string(),
            Slots {
                access_key: env_key,
                secret_key: first_set(self.env.secret_key),
                region: first_set(self.env.region),
            },
        )];

        for file in self.files {
            let Some(path) = file.resolve(&env, home) else {
                continue;
            };
            let Some(contents) = read_file(&path) else {
                continue;
            };

            let profile = file.profile(&env);
            let titled = format!("{}{}", file.section_prefix, profile);
            let Some(values) =
                section_values(&contents, &titled).or_else(|| section_values(&contents, &profile))
            else {
                continue;
            };

            let value = |key: &str| {
                values
                    .iter()
                    .find(|(name, _)| name == key)
                    .and_then(|(_, value)| nonempty(value.clone()))
            };

            places.push((
                format!("{} [{}]", file.display_path(&env), profile),
                Slots {
                    access_key: value(file.access_key),
                    secret_key: value(file.secret_key),
                    region: value(file.region),
                },
            ));
        }

        // The secret comes from wherever the key did, never from a later
        // place: half of one credential paired with half of another would
        // sign requests as nobody at all.
        let (origin, slots) = places
            .iter()
            .find(|(_, slots)| slots.access_key.is_some())?;

        Some(FoundCredentials {
            access_key: slots.access_key.clone()?,
            secret_key: slots.secret_key.clone(),
            // A region is not half of a credential, so it may come from
            // anywhere — usually `~/.aws/config`, which holds no keys.
            region: places.iter().find_map(|(_, slots)| slots.region.clone()),
            origin: origin.clone(),
        })
    }
}

impl ProfileFile {
    /// Where this file actually is, or `None` when nothing points at it and
    /// there is no home directory to resolve it against.
    fn resolve(
        &self,
        env: impl Fn(&str) -> Option<String>,
        home: Option<&Path>,
    ) -> Option<PathBuf> {
        match self.path_var.and_then(&env).and_then(nonempty) {
            Some(path) => Some(PathBuf::from(path)),
            None => Some(home?.join(self.path)),
        }
    }

    /// The profile to read: the one the environment names, or `default`.
    fn profile(&self, env: impl Fn(&str) -> Option<String>) -> String {
        self.profile_var
            .and_then(env)
            .and_then(nonempty)
            .unwrap_or_else(|| "default".to_string())
    }

    /// The file as a person would write it, for a message that has to name
    /// where a credential came from.
    fn display_path(&self, env: impl Fn(&str) -> Option<String>) -> String {
        self.path_var
            .and_then(env)
            .and_then(nonempty)
            .unwrap_or_else(|| format!("~/{}", self.path))
    }
}

/// Everything the application needs to know about a billing source.
pub struct SourceDescriptor {
    /// Stable identifier; see [`SourceId`].
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
    /// Where this source's credentials can be read from on the machine
    /// running the app, or `None` for a source with no such convention.
    pub local_credentials: Option<LocalCredentials>,
    /// Builds the API client, or `None` for a source CloudBridge can only
    /// read a bill export from so far.
    ///
    /// Optional rather than a client that fails on every call, so that a
    /// source without an API channel says so in one place and the UI can
    /// stop asking for credentials nothing would use.
    pub build: Option<fn(SourceContext) -> Box<dyn BillingSource>>,
    /// How this source's own bill export is read, or `None` for a source
    /// that publishes none.
    ///
    /// The second channel into the ledger: see [`billfile`]. A source can
    /// have both, and for Alibaba Cloud the export is the finer of the two.
    pub bill_file: Option<&'static BillFileFormat>,
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

    /// Credentials for this source found on this machine, if any.
    ///
    /// Read on demand rather than at startup: an app launched from Finder
    /// inherits no shell environment, and a credentials file can be
    /// written while the app is running.
    pub fn credentials_from_system(&self) -> Option<FoundCredentials> {
        self.local_credentials
            .as_ref()
            .and_then(LocalCredentials::read)
    }

    /// The places [`Self::credentials_from_system`] looks, in order, so a
    /// fill that finds nothing can say where it looked.
    pub fn credential_places(&self) -> Vec<String> {
        self.local_credentials
            .as_ref()
            .map(LocalCredentials::places)
            .unwrap_or_default()
    }

    /// Whether this source can be fetched from over the network.
    pub fn fetches_from_api(&self) -> bool {
        self.build.is_some()
    }

    /// Whether this source reports a balance rather than a period cost.
    pub fn is_snapshot(&self) -> bool {
        matches!(self.reporting, Reporting::Snapshot)
    }

    /// Build the API client, or say why this source has none.
    pub fn client(&self, context: SourceContext) -> Result<Box<dyn BillingSource>> {
        let build = self.build.ok_or_else(|| {
            anyhow!(
                "{} has no billing API in this build{}",
                self.display_name,
                match self.bill_file {
                    Some(format) => format!("; import its {} instead", format.display_name),
                    None => String::new(),
                }
            )
        })?;
        Ok(build(context))
    }

    /// Whether a bill export can be imported for this source.
    pub fn imports_bill_file(&self) -> bool {
        self.bill_file.is_some()
    }

    /// Whether an account of this source is usable without credentials.
    ///
    /// True exactly when a bill export is a way in: the file channel needs
    /// no key, so requiring one would be asking for a secret to sit in the
    /// keyring unused. A source whose only channel is the API has nothing
    /// to offer an account with no credentials.
    pub fn credentials_optional(&self) -> bool {
        self.imports_bill_file()
    }
}

/// The two files `aws configure` writes: the keys in one, the region in
/// the other.
static AWS_PROFILE_FILES: &[ProfileFile] = &[
    ProfileFile {
        path: ".aws/credentials",
        path_var: Some("AWS_SHARED_CREDENTIALS_FILE"),
        profile_var: Some("AWS_PROFILE"),
        section_prefix: "",
        access_key: "aws_access_key_id",
        secret_key: "aws_secret_access_key",
        region: "region",
    },
    ProfileFile {
        path: ".aws/config",
        path_var: Some("AWS_CONFIG_FILE"),
        profile_var: Some("AWS_PROFILE"),
        section_prefix: "profile ",
        access_key: "aws_access_key_id",
        secret_key: "aws_secret_access_key",
        region: "region",
    },
];

static SOURCES: &[SourceDescriptor] = &[
    SourceDescriptor {
        id: "AWS",
        display_name: "Amazon Web Services",
        short_name: "AWS",
        access_key_label: "Access Key ID",
        secret_key_label: Some("Secret Access Key"),
        default_region: Some("us-east-1"),
        reporting: Reporting::Periodic,
        local_credentials: Some(LocalCredentials {
            env: EnvCredentials {
                access_key: &["AWS_ACCESS_KEY_ID"],
                secret_key: &["AWS_SECRET_ACCESS_KEY"],
                // AWS_REGION wins, as it does in the SDKs.
                region: &["AWS_REGION", "AWS_DEFAULT_REGION"],
            },
            files: AWS_PROFILE_FILES,
        }),
        build: Some(|ctx| {
            Box::new(AwsCloudService::new(
                ctx.access_key_id,
                ctx.secret_access_key,
                ctx.region,
            ))
        }),
        // The Cost and Usage Report export is P1's; Cost Explorer is the
        // only channel today.
        bill_file: None,
    },
    SourceDescriptor {
        id: "Aliyun",
        display_name: "Alibaba Cloud",
        short_name: "Aliyun",
        access_key_label: "AccessKey ID",
        secret_key_label: Some("AccessKey Secret"),
        default_region: Some("cn-hangzhou"),
        reporting: Reporting::Periodic,
        local_credentials: Some(LocalCredentials {
            env: EnvCredentials {
                access_key: &["ALIBABA_CLOUD_ACCESS_KEY_ID", "ALICLOUD_ACCESS_KEY"],
                secret_key: &["ALIBABA_CLOUD_ACCESS_KEY_SECRET", "ALICLOUD_SECRET_KEY"],
                region: &["ALIBABA_CLOUD_REGION_ID", "ALICLOUD_REGION"],
            },
            // The Aliyun CLI keeps its profiles in JSON, which this reader
            // does not parse; the environment is all it offers for now.
            files: &[],
        }),
        build: Some(|ctx| {
            Box::new(AliyunCloudService::new(
                ctx.access_key_id,
                ctx.secret_access_key,
                ctx.region,
            ))
        }),
        // The finer of Alibaba Cloud's two channels, and the only one that
        // reports Model Studio (百炼) per model.
        bill_file: Some(&billfile::aliyun::FORMAT),
    },
    SourceDescriptor {
        id: "DeepSeek",
        display_name: "DeepSeek",
        short_name: "DeepSeek",
        access_key_label: "API Key",
        secret_key_label: None,
        default_region: None,
        reporting: Reporting::Snapshot,
        local_credentials: Some(LocalCredentials {
            env: EnvCredentials {
                access_key: &["DEEPSEEK_API_KEY"],
                secret_key: &[],
                region: &[],
            },
            // A key handed out by a web console, kept nowhere on disk.
            files: &[],
        }),
        build: Some(|ctx| {
            Box::new(DeepSeekService::new(
                ctx.access_key_id,
                ctx.secret_access_key,
                ctx.region,
            ))
        }),
        // The only window into what DeepSeek spend was for: its API
        // reports a balance and nothing else.
        bill_file: Some(&billfile::deepseek::FORMAT),
    },
    // The three below are bill-file only so far. Each names the credential
    // its billing API will want, so the form has a label ready, but none is
    // asked for while there is nothing to sign.
    SourceDescriptor {
        id: "Volcengine",
        display_name: "Volcengine (火山引擎)",
        short_name: "Volcengine",
        access_key_label: "Access Key ID",
        secret_key_label: Some("Secret Access Key"),
        default_region: None,
        reporting: Reporting::Periodic,
        local_credentials: None,
        build: None,
        // Added for Ark (火山方舟); the export covers the whole account.
        bill_file: Some(&billfile::volcengine::FORMAT),
    },
    SourceDescriptor {
        id: "OpenAI",
        display_name: "OpenAI",
        short_name: "OpenAI",
        // Costs are an organization-level endpoint: an ordinary project key
        // cannot read them, which is worth saying in the label rather than
        // in a failed request.
        access_key_label: "Admin API Key",
        secret_key_label: None,
        default_region: None,
        reporting: Reporting::Periodic,
        local_credentials: None,
        build: None,
        bill_file: Some(&billfile::openai::FORMAT),
    },
    SourceDescriptor {
        id: "Anthropic",
        display_name: "Anthropic (Claude)",
        short_name: "Claude",
        access_key_label: "Admin API Key",
        secret_key_label: None,
        default_region: None,
        reporting: Reporting::Periodic,
        local_credentials: None,
        build: None,
        bill_file: Some(&billfile::anthropic::FORMAT),
    },
];

/// Every registered source, in the order they are offered in the UI.
pub fn all() -> &'static [SourceDescriptor] {
    SOURCES
}

/// The descriptor registered under `id`, if any.
pub fn get(id: &str) -> Option<&'static SourceDescriptor> {
    SOURCES.iter().find(|source| source.id == id)
}

/// The source offered first when adding an account.
pub fn default_source() -> &'static SourceDescriptor {
    &SOURCES[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids are persisted in `cloud_accounts`, so a duplicate would make one of
    /// the two sources unreachable and silently reroute existing accounts.
    #[test]
    fn ids_are_unique() {
        let mut seen = Vec::new();
        for source in all() {
            assert!(
                !seen.contains(&source.id),
                "duplicate source id {}",
                source.id
            );
            seen.push(source.id);
        }
    }

    /// The round trip an account takes: descriptor -> stored id -> descriptor.
    #[test]
    fn every_descriptor_resolves_from_its_own_id() {
        for source in all() {
            let resolved = source
                .source_id()
                .descriptor()
                .unwrap_or_else(|| panic!("{} does not resolve", source.id));
            assert_eq!(resolved.id, source.id);
        }
    }

    /// A source that can be neither fetched from nor imported into is a row
    /// the UI would offer and nothing could ever fill.
    #[test]
    fn every_source_has_a_way_in() {
        for source in all() {
            assert!(
                source.fetches_from_api() || source.imports_bill_file(),
                "{} has neither a client nor a bill file format",
                source.id
            );
        }
    }

    /// A part name is what a normalizer looks its payload up by. Two formats
    /// sharing one would let either read the other's file.
    #[test]
    fn bill_file_part_names_are_unique() {
        let mut seen = Vec::new();
        for source in all() {
            let Some(format) = source.bill_file else {
                continue;
            };
            assert!(
                !seen.contains(&format.part),
                "{} reuses the part name {}",
                source.id,
                format.part
            );
            seen.push(format.part);
        }
    }

    /// Credentials are optional exactly where a file is a way in, because
    /// that is the only case in which an account with no key is still
    /// useful. Stated as a test because the credential form and
    /// `save_account` both branch on it.
    #[test]
    fn credentials_are_optional_exactly_where_a_file_is_a_way_in() {
        for source in all() {
            assert_eq!(
                source.credentials_optional(),
                source.imports_bill_file(),
                "{}",
                source.id
            );
        }
    }

    /// Offering to fill a form from the machine, for a source that would
    /// never sign anything with what it found, is a promise the build
    /// cannot keep.
    #[test]
    fn a_source_with_no_api_does_not_offer_to_read_credentials() {
        for source in all() {
            if !source.fetches_from_api() {
                assert!(
                    source.local_credentials.is_none(),
                    "{} would fill in a credential nothing uses",
                    source.id
                );
            }
        }
    }

    /// The error names the file channel when there is one, since that is
    /// what the user should reach for instead.
    #[test]
    fn a_source_with_no_client_says_how_its_bill_does_arrive() {
        let openai = get("OpenAI").expect("OpenAI is registered");
        // `Box<dyn BillingSource>` is not Debug, so the Ok arm is spelled
        // out rather than unwrapped.
        let error = match openai.client(SourceContext {
            access_key_id: String::new(),
            secret_access_key: String::new(),
            region: None,
        }) {
            Ok(_) => panic!("OpenAI has no API client in this build"),
            Err(e) => e.to_string(),
        };

        assert!(error.contains("no billing API"), "{}", error);
        assert!(error.contains("import"), "{}", error);
    }

    /// A source with no region must not offer one, or the credential form
    /// would ask for a value that is silently discarded.
    #[test]
    fn region_placeholder_matches_default_region() {
        for source in all() {
            match source.default_region {
                Some(region) => {
                    assert!(
                        source.region_placeholder().contains(region),
                        "{}",
                        source.id
                    )
                }
                None => assert_eq!(source.region_placeholder(), "(Not required)"),
            }
        }
    }

    /// A lookup over a fixed set of variables.
    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    /// A filesystem holding a fixed set of files.
    fn files<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&Path) -> Option<String> + 'a {
        move |path| {
            pairs
                .iter()
                .find(|(name, _)| Path::new(name) == path)
                .map(|(_, contents)| contents.to_string())
        }
    }

    /// The home directory the fixtures below are written under.
    fn home() -> Option<&'static Path> {
        Some(Path::new("/home/example"))
    }

    fn local(source: &str) -> &'static LocalCredentials {
        get(source)
            .unwrap_or_else(|| panic!("{} is registered", source))
            .local_credentials
            .as_ref()
            .unwrap_or_else(|| panic!("{} reads credentials from the machine", source))
    }

    fn aws() -> &'static LocalCredentials {
        local("AWS")
    }

    #[test]
    fn a_configured_shell_fills_every_field() {
        let found = aws()
            .read_with(
                env(&[
                    ("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE"),
                    ("AWS_SECRET_ACCESS_KEY", "secret"),
                    ("AWS_REGION", "eu-west-1"),
                ]),
                home(),
                files(&[]),
            )
            .expect("credentials are there");

        assert_eq!(found.access_key, "AKIAEXAMPLE");
        assert_eq!(found.secret_key.as_deref(), Some("secret"));
        assert_eq!(found.region.as_deref(), Some("eu-west-1"));
        assert_eq!(found.origin, "AWS_ACCESS_KEY_ID");
    }

    #[test]
    fn the_canonical_variable_wins_over_the_older_one() {
        let found = aws()
            .read_with(
                env(&[
                    ("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE"),
                    ("AWS_DEFAULT_REGION", "us-east-1"),
                    ("AWS_REGION", "eu-west-1"),
                ]),
                home(),
                files(&[]),
            )
            .unwrap();

        assert_eq!(found.region.as_deref(), Some("eu-west-1"));
    }

    #[test]
    fn a_machine_with_no_key_offers_nothing() {
        // A secret alone is not something to fill a form with.
        assert!(aws()
            .read_with(
                env(&[("AWS_SECRET_ACCESS_KEY", "secret")]),
                home(),
                files(&[])
            )
            .is_none());
        // Nor is a variable that is set but empty.
        assert!(aws()
            .read_with(env(&[("AWS_ACCESS_KEY_ID", "   ")]), home(), files(&[]))
            .is_none());
        // Nor is a credentials file without the profile being asked for.
        assert!(aws()
            .read_with(
                env(&[]),
                home(),
                files(&[(
                    "/home/example/.aws/credentials",
                    "[work]\naws_access_key_id = AKIAWORK\n"
                )])
            )
            .is_none());
    }

    #[test]
    fn a_single_key_source_needs_no_secret() {
        let found = local("DeepSeek")
            .read_with(
                env(&[("DEEPSEEK_API_KEY", "sk-example")]),
                home(),
                files(&[]),
            )
            .unwrap();

        assert_eq!(found.access_key, "sk-example");
        assert_eq!(found.secret_key, None);
        assert_eq!(found.region, None);
    }

    #[test]
    fn surrounding_whitespace_is_not_part_of_a_key() {
        let found = aws()
            .read_with(
                env(&[("AWS_ACCESS_KEY_ID", " AKIAEXAMPLE\n")]),
                home(),
                files(&[]),
            )
            .unwrap();

        assert_eq!(found.access_key, "AKIAEXAMPLE");
    }

    /// The case the button exists for: an app launched from Finder, which
    /// inherits no shell variables at all.
    #[test]
    fn the_credentials_file_is_read_when_the_environment_is_empty() {
        let found = aws()
            .read_with(
                env(&[]),
                home(),
                files(&[(
                    "/home/example/.aws/credentials",
                    "# written by aws configure\n                     [default]\n                     aws_access_key_id = AKIAFROMFILE\n                     aws_secret_access_key = filesecret\n                     region = ap-southeast-1\n",
                )]),
            )
            .expect("the file has a default profile");

        assert_eq!(found.access_key, "AKIAFROMFILE");
        assert_eq!(found.secret_key.as_deref(), Some("filesecret"));
        assert_eq!(found.region.as_deref(), Some("ap-southeast-1"));
        assert_eq!(found.origin, "~/.aws/credentials [default]");
    }

    /// A key exported by hand is the more deliberate of the two, and its
    /// secret must not be completed from the file — that would sign
    /// requests with half of one credential and half of another.
    #[test]
    fn the_shell_wins_over_the_file_and_takes_its_secret_with_it() {
        let found = aws()
            .read_with(
                env(&[("AWS_ACCESS_KEY_ID", "AKIAFROMSHELL")]),
                home(),
                files(&[(
                    "/home/example/.aws/credentials",
                    "[default]\n                     aws_access_key_id = AKIAFROMFILE\n                     aws_secret_access_key = filesecret\n",
                )]),
            )
            .unwrap();

        assert_eq!(found.access_key, "AKIAFROMSHELL");
        assert_eq!(found.secret_key, None);
    }

    #[test]
    fn the_named_profile_is_the_one_read() {
        let found = aws()
            .read_with(
                env(&[("AWS_PROFILE", "work")]),
                home(),
                files(&[(
                    "/home/example/.aws/credentials",
                    "[default]\n                     aws_access_key_id = AKIADEFAULT\n                     \n                     [work]\n                     aws_access_key_id = AKIAWORK\n                     aws_secret_access_key = worksecret\n",
                )]),
            )
            .unwrap();

        assert_eq!(found.access_key, "AKIAWORK");
        assert_eq!(found.origin, "~/.aws/credentials [work]");
    }

    /// `~/.aws/config` holds the region, and titles its sections
    /// `[profile work]` — except the default one, which stays `[default]`.
    #[test]
    fn the_region_can_come_from_the_config_file() {
        let credentials = "[default]\naws_access_key_id = AKIADEFAULT\n";

        let found = aws()
            .read_with(
                env(&[]),
                home(),
                files(&[
                    ("/home/example/.aws/credentials", credentials),
                    (
                        "/home/example/.aws/config",
                        "[default]\nregion = us-west-2\n",
                    ),
                ]),
            )
            .unwrap();
        assert_eq!(found.region.as_deref(), Some("us-west-2"));

        let found = aws()
            .read_with(
                env(&[("AWS_PROFILE", "work")]),
                home(),
                files(&[
                    (
                        "/home/example/.aws/credentials",
                        "[work]\naws_access_key_id = AKIAWORK\n",
                    ),
                    (
                        "/home/example/.aws/config",
                        "[profile work]\nregion = eu-central-1\n",
                    ),
                ]),
            )
            .unwrap();
        assert_eq!(found.region.as_deref(), Some("eu-central-1"));
    }

    #[test]
    fn a_relocated_credentials_file_is_followed() {
        let found = aws()
            .read_with(
                env(&[("AWS_SHARED_CREDENTIALS_FILE", "/etc/aws-keys")]),
                home(),
                files(&[
                    (
                        "/home/example/.aws/credentials",
                        "[default]\naws_access_key_id = AKIAIGNORED\n",
                    ),
                    (
                        "/etc/aws-keys",
                        "[default]\naws_access_key_id = AKIAMOVED\n",
                    ),
                ]),
            )
            .unwrap();

        assert_eq!(found.access_key, "AKIAMOVED");
        assert_eq!(found.origin, "/etc/aws-keys [default]");
    }

    /// Without a home directory there is no `~/.aws` to look in, and a
    /// shell-less machine simply has nothing to offer.
    #[test]
    fn no_home_directory_is_not_a_failure() {
        assert!(aws().read_with(env(&[]), None, files(&[])).is_none());
    }
}
