//! What GitHub says the newest release is, and whether it is newer than the
//! build that is running.
//!
//! `releases/latest` is GitHub's own answer: the newest release that is
//! neither a draft nor a prerelease. The rolling `insight-rules` and
//! `web-demo` releases are prereleases for exactly this reason, so they
//! never answer it.

use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use serde::Deserialize;

/// Where releases are published.
pub const REPOSITORY: &str = "JetSquirrel/cloudbridge";

/// The version that is running.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// This platform's release asset, as `.github/workflows/release.yml` names
/// it; `None` where no build is published. Renaming an asset there breaks
/// updating every install that looks for the old name.
pub const ASSET: Option<&str> = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("cloudbridge-macos-arm64.dmg")
} else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
    Some("cloudbridge-windows-x64.exe")
} else {
    None
};

const LATEST_URL: &str = "https://api.github.com/repos/JetSquirrel/cloudbridge/releases/latest";
const DOWNLOAD_PREFIX: &str = "https://github.com/JetSquirrel/cloudbridge/releases/download/";
const RESPONSE_LIMIT: u64 = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);

/// A published release.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    /// `X.Y.Z`, without the tag's `v`.
    pub version: String,
    /// This platform's download, when the release has one this build can
    /// check: a release without the asset, or whose asset GitHub gives no
    /// checksum for, has none, and is downloaded by hand.
    pub asset: Option<Asset>,
}

impl Release {
    /// The release's page, for its notes and a download by hand.
    pub fn page(&self) -> String {
        release_page(&self.version)
    }
}

/// The page of the release tagged `v{version}`.
pub fn release_page(version: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/tag/v{version}")
}

/// One file of a release.
#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub url: String,
    pub size: u64,
    /// Lowercase hex, as GitHub computed it when the file was uploaded.
    pub sha256: String,
}

/// A version's place in the order: its numbers, then whether it is a full
/// release, which comes after its own prereleases.
fn order(text: &str) -> Option<([u64; 3], bool)> {
    let text = text.strip_prefix('v').unwrap_or(text);
    let (core, prerelease) = match text.split_once('-') {
        Some((core, pre)) if !pre.is_empty() => (core, true),
        Some(_) => return None,
        None => (text, false),
    };
    let mut numbers = [0u64; 3];
    let mut parts = core.split('.');
    for slot in &mut numbers {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some((numbers, !prerelease))
}

/// Whether `candidate` is a full release newer than `current`. A prerelease
/// is never offered, and a version either side cannot read is not newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (order(candidate), order(current)) {
        (Some(candidate @ (_, true)), Some(current)) => candidate > current,
        _ => false,
    }
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    /// `sha256:<hex>`; absent on files uploaded before GitHub kept one.
    #[serde(default)]
    digest: Option<String>,
}

/// A release, from GitHub's JSON for it, with the asset called `asset_name`.
pub fn parse_release(json: &[u8], asset_name: Option<&str>) -> Result<Release> {
    let release: GitHubRelease =
        serde_json::from_slice(json).context("GitHub's answer is not a release")?;
    if release.draft || release.prerelease {
        bail!(
            "GitHub's latest release, {}, is not published",
            release.tag_name
        );
    }
    let version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name)
        .to_string();
    if !matches!(order(&version), Some((_, true))) {
        bail!(
            "GitHub's latest release has a tag this app cannot read: {}",
            release.tag_name
        );
    }
    let asset = asset_name
        .and_then(|name| release.assets.iter().find(|asset| asset.name == name))
        .and_then(|asset| {
            let sha256 = asset
                .digest
                .as_deref()
                .and_then(|digest| digest.strip_prefix("sha256:"))
                .filter(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
            let Some(sha256) = sha256 else {
                tracing::warn!("{} in {} has no checksum; not offered", asset.name, version);
                return None;
            };
            // Only this repository's own downloads, whatever the JSON says.
            if !asset.browser_download_url.starts_with(DOWNLOAD_PREFIX) {
                tracing::warn!("{} points outside the repository; not offered", asset.name);
                return None;
            }
            Some(Asset {
                url: asset.browser_download_url.clone(),
                size: asset.size,
                sha256: sha256.to_ascii_lowercase(),
            })
        });
    Ok(Release { version, asset })
}

/// The newest published release. Blocking.
pub fn fetch_latest() -> Result<Release> {
    let agent = crate::cloud::http::agent(TIMEOUT);
    let mut response = agent
        .get(LATEST_URL)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header(
            "User-Agent",
            concat!("CloudBridge/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .map_err(|e| anyhow!("Could not reach GitHub: {e}"))?;
    let status = response.status().as_u16();
    if status != 200 {
        bail!("GitHub answered {status} when asked for the latest release");
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(RESPONSE_LIMIT)
        .read_to_vec()
        .map_err(|e| anyhow!("Could not read GitHub's answer: {e}"))?;
    parse_release(&body, ASSET)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_newer_full_release_is_newer() {
        assert!(is_newer("0.11.0", "0.10.0"));
        assert!(is_newer("0.10.1", "0.10.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        // Numbers, not text: 0.10 is after 0.9.
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
        assert!(!is_newer("0.10.0", "0.10.0"));
        // A prerelease build is behind its own release, and is never offered.
        assert!(is_newer("0.11.0", "0.11.0-beta.1"));
        assert!(!is_newer("0.11.0-beta.2", "0.10.0"));
        // What cannot be read is never newer.
        for unreadable in ["", "1.0", "1.0.0.0", "x.1.0", "1.0.0-", "latest"] {
            assert!(!is_newer(unreadable, "0.1.0"), "{unreadable}");
            assert!(!is_newer("9.9.9", unreadable), "{unreadable}");
        }
    }

    fn release_json(tag: &str, assets: serde_json::Value) -> Vec<u8> {
        serde_json::json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "assets": assets,
        })
        .to_string()
        .into_bytes()
    }

    const DIGEST: &str = "sha256:7899487436EFABE2E413B0C8D6EBFD30AFDAF68979AACC6A6C167343409F5550";

    fn asset(name: &str, digest: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "browser_download_url": format!("{DOWNLOAD_PREFIX}v0.11.0/{name}"),
            "size": 1234,
            "digest": digest,
        })
    }

    #[test]
    fn the_platform_asset_is_picked_with_its_checksum() {
        let json = release_json(
            "v0.11.0",
            serde_json::json!([
                asset("cloudbridge-windows-x64.exe", Some(DIGEST)),
                asset("cloudbridge-macos-arm64.dmg", Some(DIGEST)),
            ]),
        );
        let release = parse_release(&json, Some("cloudbridge-macos-arm64.dmg")).unwrap();
        assert_eq!(release.version, "0.11.0");
        let asset = release.asset.unwrap();
        assert!(asset.url.ends_with("/v0.11.0/cloudbridge-macos-arm64.dmg"));
        assert_eq!(asset.size, 1234);
        assert_eq!(asset.sha256, DIGEST["sha256:".len()..].to_ascii_lowercase());
        assert_eq!(
            release_page("0.11.0"),
            "https://github.com/JetSquirrel/cloudbridge/releases/tag/v0.11.0"
        );
    }

    /// No download is offered that cannot be checked or does not come from
    /// the repository; the release itself is still news.
    #[test]
    fn an_asset_that_cannot_be_checked_is_not_offered() {
        let name = "cloudbridge-windows-x64.exe";
        for assets in [
            serde_json::json!([]),
            serde_json::json!([asset(name, None)]),
            serde_json::json!([asset(name, Some("md5:abc"))]),
            serde_json::json!([asset(name, Some("sha256:abc"))]),
            serde_json::json!([{
                "name": name,
                "browser_download_url": "https://example.com/cloudbridge-windows-x64.exe",
                "size": 1,
                "digest": DIGEST,
            }]),
        ] {
            let release = parse_release(&release_json("v0.11.0", assets), Some(name)).unwrap();
            assert_eq!(release.version, "0.11.0");
            assert_eq!(release.asset, None);
        }
        // A platform with no published build gets no asset either.
        let json = release_json("v0.11.0", serde_json::json!([asset(name, Some(DIGEST))]));
        assert_eq!(parse_release(&json, None).unwrap().asset, None);
    }

    #[test]
    fn a_tag_that_is_not_a_release_is_refused() {
        for tag in ["insight-rules", "v0.11.0-rc.1", "v1.0"] {
            assert!(parse_release(&release_json(tag, serde_json::json!([])), None).is_err());
        }
        let draft = serde_json::json!({ "tag_name": "v1.0.0", "draft": true }).to_string();
        assert!(parse_release(draft.as_bytes(), None).is_err());
    }
}
