//! The resource scanner behind the Insights page: corkscrew, from our fork
//! (github.com/JetSquirrel/corkscrew, branch `cloudbridge-dist`; MIT, from
//! jlgore/corkscrew), installed and run by CloudBridge.
//!
//! It is the first enrichment plugin: optional, installed only when the
//! user asks, and its only output the ledger reads is the two tables an
//! inventory import writes (`dim_resource`, `inventory_scan`). The
//! dependency is the fork's `cloudbridge-rN` releases, never upstream's and
//! never "latest" — upstream ships no built AWS plugin and has not merged
//! the fixes the scan needs.
//!
//! The user never handles it. On the first scan CloudBridge downloads the
//! pinned release (CLI plus AWS provider plugin), checks it against a
//! SHA-256 compiled in here, and unpacks it under the app's data
//! directory. A scan runs it with:
//!
//! - the working directory at the install, where corkscrew finds its
//!   plugin (`build/bin/plugins/official/aws`);
//! - `HOME` / `USERPROFILE` at a sandbox of CloudBridge's, so a corkscrew
//!   the user installed themselves — its config, its plugins — plays no
//!   part, and a config file of CloudBridge's writing in that sandbox
//!   (`scan` refuses to run without one);
//! - the account's keyring credentials in the environment, and the shared
//!   AWS config files pointed nowhere, so no other profile can be picked up.
//!
//! It only reads: a scan lists and describes resources and writes its
//! results to a database file CloudBridge then imports.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};

use super::SourceContext;
use crate::config::get_app_data_dir;

/// The release CloudBridge installs.
pub const RELEASE: &str = "cloudbridge-r2";
const DOWNLOAD_BASE: &str = "https://github.com/JetSquirrel/corkscrew/releases/download";

/// (archive, SHA-256) for this platform's build, or `None` where there is
/// no build: the scanner is then unavailable rather than guessed at.
fn archive() -> Option<(&'static str, &'static str)> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some((
            "corkscrew-cloudbridge_darwin_arm64.tar.gz",
            "b4467e32c2b554b237a8d33fff44acd826bcf6c370b1e54758256437f7f14601",
        ))
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some((
            "corkscrew-cloudbridge_windows_amd64.zip",
            "e934753f23954cc9402276e41808dc73d8175aeb21a864abdf38ce15de0c8614",
        ))
    } else {
        None
    }
}

/// One corkscrew provider plugin, and what CloudBridge has to tell it.
///
/// A source scans its resources when its descriptor names one of these
/// (`SourceDescriptor::inventory`). Everything provider-specific about a
/// scan is here; [`scan`] itself only runs the scanner, and the import
/// tells a scan's resources apart by [`Self::plugin`].
pub struct ScanProvider {
    /// The plugin's name: `--provider`, its key in the config file, and
    /// the `provider` corkscrew records the scan under.
    pub plugin: &'static str,
    /// The ledger's source id for the plugin's resources.
    pub source: &'static str,
    /// The services a scan asks for.
    pub services: &'static [&'static str],
    /// Whether a scan covers a list of regions. A provider without regions
    /// is scanned whole, and gets none.
    pub regional: bool,
    /// Hand the account's credentials to the plugin, and keep anything of
    /// the user's own configuration for the same provider out of its way.
    pub configure: fn(&mut Command, &SourceContext, &[String], &Path),
    /// Settings handed to the plugin when it starts: the `config:` map of
    /// its entry in the config file.
    pub settings: fn(&SourceContext) -> Vec<(&'static str, String)>,
    /// A failure worth saying in the user's terms, from the scanner's log.
    pub explain_failure: fn(&str) -> Option<String>,
}

/// Every plugin CloudBridge knows how to run and import.
pub static PROVIDERS: &[&ScanProvider] = &[&AWS, &CLOUDFLARE];

/// The provider a corkscrew plugin name belongs to.
pub fn provider_for_plugin(plugin: &str) -> Option<&'static ScanProvider> {
    PROVIDERS
        .iter()
        .copied()
        .find(|provider| provider.plugin.eq_ignore_ascii_case(plugin))
}

/// AWS, over Resource Explorer and Cloud Control.
pub static AWS: ScanProvider = ScanProvider {
    plugin: "aws",
    source: "AWS",
    services: AWS_SERVICES,
    regional: true,
    configure: configure_aws,
    settings: |_| Vec::new(),
    explain_failure: aws_scan_failure,
};

/// Cloudflare, over the account API: the fork's `cloudflare` plugin.
///
/// Not in the pinned release yet, so no source names it; the Cloudflare
/// descriptor switches to it with the release that ships the plugin.
pub static CLOUDFLARE: ScanProvider = ScanProvider {
    plugin: "cloudflare",
    source: "Cloudflare",
    services: CLOUDFLARE_SERVICES,
    // One account, worldwide: corkscrew takes the plugin's own `global`.
    regional: false,
    configure: configure_cloudflare,
    settings: cloudflare_settings,
    explain_failure: cloudflare_scan_failure,
};

/// The plugin's service groups a Cloudflare scan asks for: the ones that
/// bill — Workers and Durable Objects, R2, KV, Queues, D1 — and the
/// account and zones they hang from. DNS records are left out: there can
/// be thousands, and none of them carries a cost.
///
/// Each group needs its own read permission on the account's token, on top
/// of the Billing · Read the bill uses: Account Settings, Zone, Workers
/// Scripts, Workers R2 Storage, Workers KV Storage, Queues and D1, all Read.
pub const CLOUDFLARE_SERVICES: &[&str] = &["accounts", "zones", "workers", "storage", "data"];

/// The services an AWS scan asks for: the ones that carry cost and that the
/// Insights findings read. Resource Explorer's names.
pub const AWS_SERVICES: &[&str] = &[
    "ec2",
    "s3",
    "rds",
    "lambda",
    "dynamodb",
    "kms",
    "secretsmanager",
    "ecr",
    "elasticloadbalancing",
    "elasticfilesystem",
    "eks",
    "ecs",
    "elasticache",
    "cloudfront",
    "sns",
    "sqs",
    "logs",
    "amplify",
    "appsync",
    "cognito-idp",
    "cloudformation",
];

/// A scan that runs longer than this is stopped: something is wrong.
const SCAN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Whether this build of CloudBridge has a scanner to install at all.
pub fn is_supported() -> bool {
    archive().is_some()
}

fn install_dir() -> Result<PathBuf> {
    Ok(get_app_data_dir()?
        .join("tools")
        .join("corkscrew")
        .join(RELEASE))
}

fn executable(dir: &Path) -> PathBuf {
    dir.join(if cfg!(windows) {
        "corkscrew.exe"
    } else {
        "corkscrew"
    })
}

/// Whether the pinned release is installed and ready to run.
pub fn is_installed() -> bool {
    install_dir()
        .map(|dir| dir.join(".installed").exists() && executable(&dir).exists())
        .unwrap_or(false)
}

/// Download, verify and unpack the pinned release, unless it is there
/// already. Blocking.
pub fn ensure_installed() -> Result<PathBuf> {
    let dir = install_dir()?;
    if is_installed() {
        return Ok(dir);
    }
    install_into(&dir)?;
    tracing::info!("Scanner {} installed at {}", RELEASE, dir.display());
    Ok(dir)
}

/// Download, verify and unpack the pinned release into `dir`, replacing
/// whatever is there.
fn install_into(dir: &Path) -> Result<()> {
    let (name, sha256) =
        archive().ok_or_else(|| anyhow!("Insights scanning is not available on this platform"))?;

    let bytes = download(&format!("{DOWNLOAD_BASE}/{RELEASE}/{name}"))?;
    let digest = hex::encode(Sha256::digest(&bytes));
    if digest != sha256 {
        return Err(anyhow!(
            "The downloaded scanner did not match its checksum, so it was not installed"
        ));
    }

    // Unpack beside the final directory and move it into place, so a
    // half-finished install never looks installed.
    let parent = dir
        .parent()
        .ok_or_else(|| anyhow!("No parent for {}", dir.display()))?;
    std::fs::create_dir_all(parent)?;
    let staging = parent.join(format!("{RELEASE}.partial"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let archive_path = staging.join(name);
    std::fs::write(&archive_path, &bytes)?;
    unpack(&archive_path, &staging)?;
    std::fs::remove_file(&archive_path)?;
    if !executable(&staging).exists() {
        return Err(anyhow!("The scanner archive did not contain the scanner"));
    }
    std::fs::write(staging.join(".installed"), sha256)?;

    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&staging, dir)?;
    Ok(())
}

fn download(url: &str) -> Result<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(300)))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .call()
        .map_err(|e| anyhow!("Could not download the scanner: {e}"))?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut response.body_mut().as_reader(), &mut bytes)
        .map_err(|e| anyhow!("Could not download the scanner: {e}"))?;
    Ok(bytes)
}

/// Unpack with the system `tar`, which reads both the macOS `.tar.gz` and
/// the Windows `.zip` (bsdtar ships with macOS and with Windows 10 and
/// later).
fn unpack(archive: &Path, into: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .status()
        .map_err(|e| anyhow!("Could not unpack the scanner: {e}"))?;
    if !status.success() {
        return Err(anyhow!(
            "Could not unpack the scanner (tar exited with {status})"
        ));
    }
    Ok(())
}

/// Scan one account's `regions` into a fresh database at `out`, with the
/// provider's plugin. Blocking; `ensure_installed` first.
pub fn scan(
    install: &Path,
    provider: &ScanProvider,
    credentials: &SourceContext,
    regions: &[String],
    out: &Path,
) -> Result<()> {
    if provider.regional && regions.is_empty() {
        return Err(anyhow!("No regions to scan"));
    }
    let sandbox = get_app_data_dir()?.join("tools").join("corkscrew-home");
    std::fs::create_dir_all(&sandbox)?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    for stale in [out.to_path_buf(), out.with_extension("duckdb.wal")] {
        let _ = std::fs::remove_file(stale);
    }
    let config = sandbox.join("corkscrew.yaml");
    let settings = (provider.settings)(credentials);
    std::fs::write(&config, config_yaml(provider, regions, &settings, out))?;

    let mut command = Command::new(executable(install));
    command
        .current_dir(install)
        .args(["scan", "--provider", provider.plugin, "--output", "json"]);
    if provider.regional {
        command.arg("--region").arg(regions.join(","));
    }
    command
        .arg("--services")
        .arg(provider.services.join(","))
        .arg("--database")
        .arg(out)
        .env("HOME", &sandbox)
        .env("USERPROFILE", &sandbox)
        .env("CORKSCREW_CONFIG_FILE", &config)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    (provider.configure)(&mut command, credentials, regions, &sandbox);

    let mut child = command
        .spawn()
        .map_err(|e| anyhow!("Could not start the scanner: {e}"))?;
    let mut stderr = child.stderr.take().expect("stderr is piped");
    // Read stderr on its own thread: a scan logs a lot, and a full pipe
    // would stall it.
    let log = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
        text
    });

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > SCAN_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(
                "The scan took longer than 30 minutes and was stopped"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    let log = log.join().unwrap_or_default();

    if let Some(message) = (provider.explain_failure)(&log) {
        return Err(anyhow!(message));
    }
    if !status.success() {
        tracing::warn!("Scanner exited with {status}; last output: {}", tail(&log));
        return Err(anyhow!("The scan did not finish ({status})"));
    }
    Ok(())
}

/// The AWS plugin reads the standard SDK variables. The shared config
/// files are pointed into the sandbox, where there are none, so no profile
/// of the user's can stand in for the account's own key.
fn configure_aws(
    command: &mut Command,
    credentials: &SourceContext,
    regions: &[String],
    sandbox: &Path,
) {
    command
        .env("AWS_ACCESS_KEY_ID", &credentials.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &credentials.secret_access_key)
        .env("AWS_REGION", &regions[0])
        .env(
            "AWS_SHARED_CREDENTIALS_FILE",
            sandbox.join("no-credentials"),
        )
        .env("AWS_CONFIG_FILE", sandbox.join("no-config"))
        .env_remove("AWS_PROFILE")
        .env_remove("AWS_SESSION_TOKEN");
}

/// The Cloudflare plugin reads an API token from the environment. The
/// legacy key and email it would also accept are removed, so a Global API
/// Key in the user's shell cannot stand in for the account's own token —
/// and the sandboxed `HOME` keeps its stored OAuth profiles out of reach.
fn configure_cloudflare(
    command: &mut Command,
    credentials: &SourceContext,
    _regions: &[String],
    _sandbox: &Path,
) {
    command
        .env("CLOUDFLARE_API_TOKEN", &credentials.secret_access_key)
        .env_remove("CLOUDFLARE_API_KEY")
        .env_remove("CLOUDFLARE_EMAIL");
}

/// The token is the only method, and the scan stays inside the account
/// the token was saved for — a token can reach several.
fn cloudflare_settings(credentials: &SourceContext) -> Vec<(&'static str, String)> {
    vec![
        ("auth.method", "api_token".to_string()),
        ("account_ids", credentials.access_key_id.trim().to_string()),
    ]
}

/// A failure of a Cloudflare scan worth saying in the user's terms: the
/// plugin refusing to start is the token, and says why.
fn cloudflare_scan_failure(log: &str) -> Option<String> {
    let start = log
        .lines()
        .find(|line| line.contains("initialize provider \"cloudflare\""))?;
    let reason = start
        .split_once("initialize provider \"cloudflare\":")
        .map(|(_, reason)| reason.trim())
        .filter(|reason| !reason.is_empty())
        .unwrap_or("no reason given");
    Some(format!(
        "Cloudflare did not let the scan start ({reason}). Check that the account's \
         API token can read Workers, R2, KV, Queues, D1 and zones."
    ))
}

/// YAML's double-quoted form of `value`.
fn yaml_quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The configuration a scan runs with: the provider over `regions` and
/// its services, with its `settings`, writing to `out`. The flags say the
/// same; corkscrew still wants the file.
fn config_yaml(
    provider: &ScanProvider,
    regions: &[String],
    settings: &[(&str, String)],
    out: &Path,
) -> String {
    let quoted = |items: &mut dyn Iterator<Item = &str>| -> String {
        items
            .map(|item| format!("      - {}\n", yaml_quoted(item)))
            .collect()
    };
    let regions = if provider.regional {
        format!(
            "    regions:\n{}",
            quoted(&mut regions.iter().map(String::as_str))
        )
    } else {
        String::new()
    };
    let config = if settings.is_empty() {
        String::new()
    } else {
        let entries: String = settings
            .iter()
            .map(|(key, value)| format!("      {}: {}\n", yaml_quoted(key), yaml_quoted(value)))
            .collect();
        format!("    config:\n{entries}")
    };
    format!(
        "version: \"2.0\"\nproviders:\n  {}:\n    enabled: true\n{}    services:\n{}{}database:\n  path: {}\n",
        provider.plugin,
        regions,
        quoted(&mut provider.services.iter().copied()),
        config,
        yaml_quoted(&out.display().to_string()),
    )
}

/// A failure of an AWS scan worth saying in the user's terms.
fn aws_scan_failure(log: &str) -> Option<String> {
    let denied = [
        "AccessDenied",
        "UnauthorizedOperation",
        "is not authorized to perform",
    ];
    let invalid = [
        "InvalidClientTokenId",
        "SignatureDoesNotMatch",
        "UnrecognizedClientException",
    ];
    if invalid.iter().any(|marker| log.contains(marker)) {
        return Some(
            "AWS did not accept this account's access key. Check it on the Accounts page."
                .to_string(),
        );
    }
    // A scan logs per-resource failures and carries on; only a scan that
    // stored nothing because every call was refused is the account's
    // permissions, which the import that follows reports as an empty scan.
    if denied.iter().any(|marker| log.contains(marker)) && !log.contains("Batch scan") {
        return Some(
            "This account's access key cannot read resources. Grant it read-only access \
             (the AWS ReadOnlyAccess policy) to scan it."
                .to_string(),
        );
    }
    None
}

fn tail(log: &str) -> &str {
    let start = log.len().saturating_sub(800);
    let start = (start..log.len())
        .find(|&i| log.is_char_boundary(i))
        .unwrap_or(log.len());
    &log[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Downloads the pinned release: run with `--ignored` when changing it.
    #[test]
    #[ignore]
    fn the_pinned_release_installs() {
        let dir = std::env::temp_dir()
            .join(format!("cloudbridge-scanner-{}", uuid::Uuid::new_v4()))
            .join(RELEASE);
        install_into(&dir).unwrap();
        assert!(executable(&dir).exists());
        let plugin = dir.join("build/bin/plugins/official/aws");
        assert!(plugin.join("plugin.json").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(executable(&dir))
                .unwrap()
                .permissions()
                .mode();
            assert!(mode & 0o111 != 0, "the scanner must stay executable");
            let mode = std::fs::metadata(plugin.join("aws-provider"))
                .unwrap()
                .permissions()
                .mode();
            assert!(mode & 0o111 != 0, "the plugin must stay executable");
        }
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn the_config_names_the_regions_services_and_database() {
        let yaml = config_yaml(
            &AWS,
            &["ap-east-1".to_string(), "us-east-1".to_string()],
            &[],
            Path::new("/data/inventory/scan-1.duckdb"),
        );
        assert!(yaml.starts_with("version: \"2.0\"\nproviders:\n  aws:\n    enabled: true\n"));
        assert!(yaml.contains("    regions:\n      - \"ap-east-1\"\n      - \"us-east-1\"\n"));
        assert!(yaml.contains("      - \"kms\"\n"));
        assert!(yaml.ends_with("database:\n  path: \"/data/inventory/scan-1.duckdb\"\n"));
        // A Windows path's backslashes are escaped inside the quotes.
        let windows = config_yaml(
            &AWS,
            &["us-east-1".to_string()],
            &[],
            Path::new(r"C:\data\scan.duckdb"),
        );
        assert!(
            windows.contains(r#"path: "C:\\data\\scan.duckdb""#),
            "{windows}"
        );
    }

    /// A provider scanned whole names no regions, in the file as on the
    /// command line.
    #[test]
    fn a_provider_without_regions_is_configured_without_them() {
        static WHOLE: ScanProvider = ScanProvider {
            plugin: "example",
            source: "Example",
            services: &["things"],
            regional: false,
            configure: |_, _, _, _| {},
            settings: |_| Vec::new(),
            explain_failure: |_| None,
        };
        let yaml = config_yaml(&WHOLE, &[], &[], Path::new("/data/scan.duckdb"));
        assert!(yaml.starts_with(
            "version: \"2.0\"\nproviders:\n  example:\n    enabled: true\n    services:\n"
        ));
        assert!(!yaml.contains("regions"));
    }

    fn cloudflare_account() -> SourceContext {
        SourceContext {
            access_key_id: " 023e105f4ecef8ad9ca31a8372d0c353 ".to_string(),
            secret_access_key: "token".to_string(),
            region: None,
            export_uri: None,
        }
    }

    /// The scan is held to the token's own method and the account it was
    /// saved for, and asks for no regions: corkscrew then takes the
    /// plugin's own `global`.
    #[test]
    fn a_cloudflare_scan_is_one_account_with_its_token() {
        let settings = (CLOUDFLARE.settings)(&cloudflare_account());
        let yaml = config_yaml(&CLOUDFLARE, &[], &settings, Path::new("/data/scan.duckdb"));
        assert!(
            yaml.contains("  cloudflare:\n    enabled: true\n    services:\n"),
            "{yaml}"
        );
        assert!(!yaml.contains("regions"), "{yaml}");
        assert!(yaml.contains(
            "    config:\n      \"auth.method\": \"api_token\"\n      \"account_ids\": \"023e105f4ecef8ad9ca31a8372d0c353\"\n"
        ), "{yaml}");
        assert!(
            !yaml.contains("\"dns\""),
            "DNS records carry no cost: {yaml}"
        );
    }

    #[test]
    fn a_scan_database_names_its_provider_by_plugin() {
        assert_eq!(provider_for_plugin("AWS").map(|p| p.source), Some("AWS"));
        assert_eq!(
            provider_for_plugin("cloudflare").map(|p| p.source),
            Some("Cloudflare")
        );
        assert!(provider_for_plugin("gcp").is_none());
    }

    #[test]
    fn a_cloudflare_scan_that_cannot_start_says_why() {
        let log = "Error: initialize provider \"cloudflare\": cloudflare token validation failed: Invalid API Token";
        let message = cloudflare_scan_failure(log).unwrap();
        assert!(message.contains("Invalid API Token"), "{message}");
        assert!(message.contains("API token can read"), "{message}");
        assert_eq!(
            cloudflare_scan_failure("Batch scan scan_1: 12 resources"),
            None
        );
    }

    #[test]
    fn a_rejected_key_is_reported_in_the_users_terms() {
        let log = "operation error STS: GetCallerIdentity, api error InvalidClientTokenId: \
                   The security token included in the request is invalid";
        assert!(aws_scan_failure(log).unwrap().contains("access key"));
    }

    #[test]
    fn missing_read_permissions_are_reported_when_nothing_was_scanned() {
        let refused = "api error AccessDeniedException: User is not authorized to perform \
                       resource-explorer-2:Search";
        assert!(aws_scan_failure(refused)
            .unwrap()
            .contains("ReadOnlyAccess"));
        // Per-resource refusals in a scan that went through are not fatal.
        let partial = format!("{refused}\nBatch scan scan_1: 485 resources across 17 services");
        assert_eq!(aws_scan_failure(&partial), None);
    }

    #[test]
    fn the_scan_covers_the_services_insights_reads() {
        for service in ["ec2", "s3", "kms", "secretsmanager"] {
            assert!(AWS_SERVICES.contains(&service), "{service}");
        }
    }
}
