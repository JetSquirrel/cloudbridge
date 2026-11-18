//! Downloading a release, checking it, and putting it in place of the
//! build that is running.
//!
//! Installing is two steps, apart in time:
//!
//! 1. **Staging** ([`stage`]) downloads the platform's asset into the data
//!    directory's `updates/<version>/` and checks it: its SHA-256 against
//!    the digest GitHub recorded when CI uploaded it, and on macOS the
//!    unpacked app's code signature against this team's Developer ID, its
//!    notarization through Gatekeeper, and its version. The `ready` marker
//!    is written last, so a staging that stopped partway is never mistaken
//!    for one that finished.
//! 2. **Applying** ([`apply`]) swaps the staged copy in for the running
//!    one: renames, so it is quick enough for a quit, with the old copy put
//!    back if the new one cannot be moved into place. It runs when someone
//!    restarts to update, or when the app quits with an update staged.
//!
//! The running copy is never written over in place. On Windows a running
//! executable can be renamed but not deleted, so its old copy stays beside
//! it as `<name>.old` until the next launch clears it away ([`tidy`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use sha2::{Digest as _, Sha256};

use super::release::{is_newer, Asset, Release, ASSET, CURRENT};

/// Marks a staged update as checked and complete.
const READY: &str = "ready";
/// An asset larger than this is not one of ours.
const DOWNLOAD_LIMIT: u64 = 512 * 1024 * 1024;
const DOWNLOAD_ATTEMPTS: u32 = 3;

/// The update's file or bundle inside its staging directory.
const STAGED_NAME: &str = if cfg!(target_os = "macos") {
    "CloudBridge.app"
} else {
    "cloudbridge.exe"
};

/// A checked update waiting to be applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Staged {
    pub version: String,
    /// The staged executable, or `.app` bundle on macOS.
    pub path: PathBuf,
}

/// What an update replaces: the `.app` bundle on macOS, the executable on
/// Windows. `None` for a build that must not replace itself — a debug
/// build, an unbundled macOS binary, a platform with no published build.
pub fn install_target() -> Option<PathBuf> {
    if cfg!(debug_assertions) || ASSET.is_none() {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    if cfg!(target_os = "macos") {
        bundle_of(&exe)
    } else {
        Some(exe)
    }
}

/// `…/CloudBridge.app` for `…/CloudBridge.app/Contents/MacOS/CloudBridge`.
fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let bundled = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app";
    bundled.then(|| app.to_path_buf())
}

fn updates_dir() -> Result<PathBuf> {
    Ok(crate::config::get_app_data_dir()?.join("updates"))
}

/// Where the running copy waits while the new one moves in: beside it, so
/// the move is a rename.
fn backup_of(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    target.with_file_name(name)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Clear away what earlier updates left — the replaced copy, staged
/// updates that are no newer than this build or never finished — and
/// return the newest finished one still to apply. Blocking; best-effort.
pub fn tidy(target: Option<&Path>) -> Option<Staged> {
    if let Some(target) = target {
        let backup = backup_of(target);
        if backup.exists() {
            if let Err(e) = remove_path(&backup) {
                tracing::warn!("Could not remove {}: {e}", backup.display());
            }
        }
    }
    let dir = updates_dir().ok()?;
    let staged = tidy_in(&dir, CURRENT)?;
    // Nothing can be applied without a copy to replace.
    if target.is_none() {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }
    Some(staged)
}

fn tidy_in(dir: &Path, current: &str) -> Option<Staged> {
    let mut newest: Option<Staged> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let version = entry.file_name().to_string_lossy().into_owned();
        let finished = std::fs::read_to_string(path.join(READY))
            .is_ok_and(|marked| marked.trim() == version)
            && path.join(STAGED_NAME).exists();
        let keep = finished && is_newer(&version, current);
        if keep
            && newest
                .as_ref()
                .is_none_or(|n| is_newer(&version, &n.version))
        {
            if let Some(older) = newest.replace(Staged {
                version,
                path: path.join(STAGED_NAME),
            }) {
                let _ = remove_path(older.path.parent().unwrap_or(&older.path));
            }
        } else if let Err(e) = remove_path(&path) {
            tracing::warn!("Could not remove {}: {e}", path.display());
        }
    }
    newest
}

/// Download `release` for this platform, check it, and stage it for
/// [`apply`] over `target`. Blocking.
pub fn stage(release: &Release, target: &Path) -> Result<Staged> {
    let asset = release.asset.as_ref().ok_or_else(|| {
        anyhow!(
            "CloudBridge {} has no download for this platform",
            release.version
        )
    })?;
    ensure_replaceable(target)?;

    let dir = updates_dir()?.join(&release.version);
    let _ = remove_path(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("Creating {}", dir.display()))?;

    let bytes = download(asset)?;
    check_download(&bytes, asset)?;

    let staged = dir.join(STAGED_NAME);
    unpack(&bytes, &dir, &staged, &release.version)?;
    std::fs::write(dir.join(READY), &release.version)?;
    tracing::info!(
        "CloudBridge {} staged at {}",
        release.version,
        staged.display()
    );
    Ok(Staged {
        version: release.version.clone(),
        path: staged,
    })
}

/// The download is the file the release lists, byte for byte.
fn check_download(bytes: &[u8], asset: &Asset) -> Result<()> {
    if bytes.len() as u64 != asset.size {
        bail!(
            "The download was {} bytes where the release lists {}; not installed",
            bytes.len(),
            asset.size
        );
    }
    let got = sha256_hex(bytes);
    if got != asset.sha256 {
        bail!("The download did not match the release's checksum; not installed");
    }
    Ok(())
}

/// Fail now, rather than at quit when nobody can be told, if the running
/// copy is somewhere it cannot be replaced from.
fn ensure_replaceable(target: &Path) -> Result<()> {
    let text = target.to_string_lossy();
    if cfg!(target_os = "macos")
        && (text.contains("/AppTranslocation/") || text.starts_with("/Volumes/"))
    {
        bail!(
            "CloudBridge is running from the disk image or a quarantined copy. Move it to the \
             Applications folder, open it from there, and update again."
        );
    }
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("No folder holds {}", target.display()))?;
    let probe = parent.join(format!(".cloudbridge-update-{}", std::process::id()));
    std::fs::write(&probe, b"")
        .and_then(|()| std::fs::remove_file(&probe))
        .map_err(|e| {
            anyhow!(
                "CloudBridge cannot replace itself in {} ({e}). Download the new version from \
                 the release page instead.",
                parent.display()
            )
        })
}

fn download(asset: &Asset) -> Result<Vec<u8>> {
    let agent = crate::cloud::http::strict_agent(Duration::from_secs(600));
    let mut attempt = 1;
    loop {
        match download_once(&agent, &asset.url) {
            Ok(bytes) => return Ok(bytes),
            Err(e) if attempt < DOWNLOAD_ATTEMPTS && retryable(&e) => {
                tracing::warn!("Update download attempt {attempt} failed, retrying: {e}");
                std::thread::sleep(Duration::from_secs(2 * u64::from(attempt)));
                attempt += 1;
            }
            Err(e) => return Err(anyhow!("Could not download the update: {e}")),
        }
    }
}

fn download_once(agent: &ureq::Agent, url: &str) -> std::result::Result<Vec<u8>, ureq::Error> {
    agent
        .get(url)
        .header(
            "User-Agent",
            concat!("CloudBridge/", env!("CARGO_PKG_VERSION")),
        )
        .call()?
        .body_mut()
        .with_config()
        .limit(DOWNLOAD_LIMIT)
        .read_to_vec()
}

/// A refusal will not change on a retry; a dropped connection may.
fn retryable(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::StatusCode(status) => *status == 429 || *status >= 500,
        _ => true,
    }
}

/// Put the downloaded `bytes` at `staged`, ready to move into place.
#[cfg(not(target_os = "macos"))]
fn unpack(bytes: &[u8], _dir: &Path, staged: &Path, _version: &str) -> Result<()> {
    if !bytes.starts_with(b"MZ") {
        bail!("The download is not a Windows program; not installed");
    }
    std::fs::write(staged, bytes).with_context(|| format!("Writing {}", staged.display()))
}

#[cfg(target_os = "macos")]
fn unpack(bytes: &[u8], dir: &Path, staged: &Path, version: &str) -> Result<()> {
    macos::unpack(bytes, dir, staged, version)
}

/// Swap `staged` in for `target`, keeping the old copy until the new one is
/// in place. Blocking: renames, unless the two are on different volumes.
pub fn apply(staged: &Staged, target: &Path) -> Result<()> {
    swap(&staged.path, target)?;
    if let Some(dir) = staged.path.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
    tracing::info!(
        "CloudBridge {} installed at {}",
        staged.version,
        target.display()
    );
    Ok(())
}

fn swap(new: &Path, target: &Path) -> Result<()> {
    if !new.exists() {
        bail!("The downloaded update is gone from {}", new.display());
    }
    let backup = backup_of(target);
    if backup.exists() {
        remove_path(&backup).with_context(|| format!("Removing {}", backup.display()))?;
    }
    std::fs::rename(target, &backup)
        .with_context(|| format!("Moving {} aside", target.display()))?;
    if let Err(e) = move_into_place(new, target) {
        let _ = remove_path(target);
        if let Err(back) = std::fs::rename(&backup, target) {
            tracing::error!("Could not put {} back: {back}", target.display());
        }
        return Err(e.context(format!("Installing the update at {}", target.display())));
    }
    // A running Windows executable cannot be deleted; the next launch
    // clears it away instead.
    let _ = remove_path(&backup);
    Ok(())
}

/// A rename, or a copy where `from` is on another volume.
fn move_into_place(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if from.is_dir() {
        copy_dir(from, to)?;
    } else {
        std::fs::copy(from, to).with_context(|| format!("Copying to {}", to.display()))?;
    }
    let _ = remove_path(from);
    Ok(())
}

/// `ditto` on macOS, which keeps a bundle's signature, links and modes.
#[cfg(target_os = "macos")]
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    macos::run(std::process::Command::new("ditto").arg(from).arg(to)).map(drop)
}

#[cfg(not(target_os = "macos"))]
fn copy_dir(from: &Path, _to: &Path) -> Result<()> {
    bail!("Cannot copy the folder {}", from.display())
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Unpacking the disk image and checking the app inside it.
#[cfg(target_os = "macos")]
mod macos {
    use std::path::Path;
    use std::process::Command;

    use anyhow::{anyhow, bail, Result};

    /// The team the release is signed by (`scripts/package-macos.sh`). An
    /// app signed by anyone else is not installed, whatever its checksum.
    const TEAM_ID: &str = "V5KP6ZYMDT";

    pub(super) fn unpack(bytes: &[u8], dir: &Path, staged: &Path, version: &str) -> Result<()> {
        let dmg = dir.join("update.dmg");
        std::fs::write(&dmg, bytes)?;
        let mount = dir.join("mount");
        std::fs::create_dir_all(&mount)?;
        run(Command::new("hdiutil")
            .args([
                "attach",
                "-nobrowse",
                "-readonly",
                "-noautoopen",
                "-mountpoint",
            ])
            .arg(&mount)
            .arg(&dmg))?;
        let copied = run(Command::new("ditto")
            .arg(mount.join("CloudBridge.app"))
            .arg(staged));
        if let Err(e) = run(Command::new("hdiutil")
            .args(["detach", "-quiet", "-force"])
            .arg(&mount))
        {
            tracing::warn!("Could not detach the update's disk image: {e}");
        }
        let _ = std::fs::remove_file(&dmg);
        let _ = std::fs::remove_dir(&mount);
        copied?;
        verify(staged, version)
    }

    /// The app is this team's, notarized, and the version it says it is.
    fn verify(app: &Path, version: &str) -> Result<()> {
        run(Command::new("codesign")
            .args(["--verify", "--deep", "--strict", "-R"])
            .arg(format!(
                "=anchor apple generic and certificate leaf[subject.OU] = \"{TEAM_ID}\""
            ))
            .arg(app))
        .map_err(|e| anyhow!("The update is not signed by CloudBridge's developer: {e}"))?;
        run(Command::new("spctl")
            .args(["--assess", "--type", "execute"])
            .arg(app))
        .map_err(|e| anyhow!("macOS would not open the update: {e}"))?;
        let found = run(Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", "Print :CFBundleShortVersionString"])
            .arg(app.join("Contents/Info.plist")))?;
        if found.trim() != version {
            bail!(
                "The update says it is version {} rather than {version}",
                found.trim()
            );
        }
        Ok(())
    }

    /// Run `command`, returning what it printed, or failing with what it
    /// said on stderr.
    pub(super) fn run(command: &mut Command) -> Result<String> {
        let program = command.get_program().to_string_lossy().into_owned();
        let output = command
            .output()
            .map_err(|e| anyhow!("Could not run {program}: {e}"))?;
        if !output.status.success() {
            bail!(
                "{program} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cloudbridge-update-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_bundle_is_found_from_its_executable() {
        assert_eq!(
            bundle_of(Path::new(
                "/Applications/CloudBridge.app/Contents/MacOS/CloudBridge"
            )),
            Some(PathBuf::from("/Applications/CloudBridge.app"))
        );
        // `cargo run` is not a bundle, and must not be replaced as one.
        assert_eq!(
            bundle_of(Path::new("/src/target/release/cloudbridge")),
            None
        );
    }

    #[test]
    fn a_download_must_match_the_release_exactly() {
        let bytes = b"MZ new build";
        let asset = Asset {
            url: String::new(),
            size: bytes.len() as u64,
            sha256: sha256_hex(bytes),
        };
        check_download(bytes, &asset).unwrap();
        assert!(check_download(b"MZ new buil", &asset).is_err());
        assert!(check_download(b"MZ new bulid", &asset)
            .unwrap_err()
            .to_string()
            .contains("checksum"));
    }

    /// The new copy takes the old one's place, and the old one is gone or
    /// waiting beside it for the next launch to clear.
    #[test]
    fn applying_swaps_the_new_copy_in() {
        let dir = temp_dir("apply");
        let target = dir.join("cloudbridge.exe");
        std::fs::write(&target, b"old").unwrap();
        let staged_dir = dir.join("updates").join("0.11.0");
        std::fs::create_dir_all(&staged_dir).unwrap();
        let staged = Staged {
            version: "0.11.0".into(),
            path: staged_dir.join("cloudbridge.exe"),
        };
        std::fs::write(&staged.path, b"new").unwrap();

        apply(&staged, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!staged_dir.exists(), "the staging is cleared away");
        assert!(!backup_of(&target).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An update that is not there leaves the running copy where it was.
    #[test]
    fn a_failed_apply_keeps_the_running_copy() {
        let dir = temp_dir("keep");
        let target = dir.join("cloudbridge.exe");
        std::fs::write(&target, b"old").unwrap();
        let missing = Staged {
            version: "0.11.0".into(),
            path: dir.join("gone.exe"),
        };
        assert!(apply(&missing, &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn stage_at(updates: &Path, version: &str, marked: Option<&str>) {
        let dir = updates.join(version);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(STAGED_NAME), b"build").unwrap();
        if let Some(marked) = marked {
            std::fs::write(dir.join(READY), marked).unwrap();
        }
    }

    /// Only the newest finished update newer than this build survives.
    #[test]
    fn tidying_keeps_only_the_newest_finished_update() {
        let updates = temp_dir("tidy");
        stage_at(&updates, "0.10.0", Some("0.10.0")); // what is running
        stage_at(&updates, "0.11.0", Some("0.11.0")); // superseded
        stage_at(&updates, "0.12.0", Some("0.12.0")); // the one
        stage_at(&updates, "0.13.0", None); // stopped partway
        stage_at(&updates, "0.14.0", Some("0.13.0")); // marked for another

        let kept = tidy_in(&updates, "0.10.0").unwrap();
        assert_eq!(kept.version, "0.12.0");
        assert_eq!(kept.path, updates.join("0.12.0").join(STAGED_NAME));
        let mut left: Vec<String> = std::fs::read_dir(&updates)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["0.12.0"]);

        // Once that version is running, there is nothing left to apply.
        assert_eq!(tidy_in(&updates, "0.12.0"), None);
        assert_eq!(std::fs::read_dir(&updates).unwrap().count(), 0);
        std::fs::remove_dir_all(&updates).ok();
    }

    #[test]
    fn a_dropped_download_is_retried_and_a_refusal_is_not() {
        let dropped = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
        assert!(retryable(&ureq::Error::Io(dropped)));
        assert!(retryable(&ureq::Error::StatusCode(503)));
        assert!(!retryable(&ureq::Error::StatusCode(404)));
    }
}
