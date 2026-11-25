//! Over-the-air updates.
//!
//! A background thread checks the latest GitHub release's `latest.json`
//! shortly after start and every few hours. When it lists a newer version for
//! this platform, the build is downloaded next to the running executable and
//! accepted only if its SHA-256 matches and its ed25519 signature verifies
//! against the key built into T7. The swap happens on exit (or when the
//! update icon is clicked): the running executable is renamed aside, which
//! Windows allows, and the new one takes its name.
//!
//! Only builds made by the release pipeline (`T7_RELEASE` set at compile time)
//! update themselves; local development builds never replace themselves.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use t7_proto::update::{Manifest, current_target, from_hex, parse_version, signed_message, to_hex};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const ENABLED: bool = option_env!("T7_RELEASE").is_some();
const MANIFEST_URL: &str = "https://github.com/Mavial/t7rs/releases/latest/download/latest.json";
/// Release signing key (the secret half is a GitHub Actions secret).
const PUBLIC_KEY: &str = "d293f0fe813036683dd8b909c9577906c5b5545cfd0d41f1bf3cdc2a4edc2d24";
const FIRST_CHECK: Duration = Duration::from_secs(5);
const CHECK_EVERY: Duration = Duration::from_hours(6);
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_BUILD: u64 = 200 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not a release build, or no check has finished yet.
    Idle,
    Checking,
    UpToDate,
    Downloading(String),
    /// Verified and waiting next to the executable.
    Ready(String),
    Failed(String),
}

pub struct Updater {
    status: Arc<Mutex<Status>>,
    wake: Option<mpsc::Sender<()>>,
}

impl Updater {
    pub fn start(ctx: egui::Context) -> Self {
        remove_previous_version();
        let status = Arc::new(Mutex::new(Status::Idle));
        if !ENABLED {
            return Self { status, wake: None };
        }
        let (wake, rx) = mpsc::channel();
        let shared = status.clone();
        let spawned = std::thread::Builder::new().name("t7-update".into()).spawn(move || {
            let mut wait = FIRST_CHECK;
            loop {
                match rx.recv_timeout(wait) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
                if !matches!(*shared.lock().unwrap(), Status::Ready(_)) {
                    check(&shared, &ctx);
                }
                wait = CHECK_EVERY;
            }
        });
        Self { status, wake: spawned.ok().map(|_| wake) }
    }

    #[cfg(test)]
    pub fn showing(status: Status) -> Self {
        Self { status: Arc::new(Mutex::new(status)), wake: None }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn enabled(&self) -> bool {
        self.wake.is_some()
    }

    pub fn check_now(&self) {
        if let Some(wake) = &self.wake {
            let _ = wake.send(());
        }
    }

    /// Puts a downloaded update in place of the running executable. Returns
    /// the executable's path if an update was installed.
    pub fn install(&self) -> Option<PathBuf> {
        if !matches!(self.status(), Status::Ready(_)) {
            return None;
        }
        match install_update() {
            Ok(exe) => {
                *self.status.lock().unwrap() = Status::Idle;
                Some(exe)
            }
            Err(e) => {
                tracing::error!("installing the update failed: {e}");
                *self.status.lock().unwrap() = Status::Failed(format!("couldn't install the update: {e}"));
                None
            }
        }
    }
}

fn set(status: &Mutex<Status>, ctx: &egui::Context, value: Status) {
    *status.lock().unwrap() = value;
    ctx.request_repaint();
}

fn check(status: &Mutex<Status>, ctx: &egui::Context) {
    set(status, ctx, Status::Checking);
    let result = match fetch_newer() {
        Ok(None) => Status::UpToDate,
        Ok(Some((version, url, sha256, signature))) => {
            set(status, ctx, Status::Downloading(version.clone()));
            match download(&version, &url, &sha256, &signature) {
                Ok(()) => Status::Ready(version),
                Err(e) => Status::Failed(e),
            }
        }
        Err(e) => Status::Failed(e),
    };
    if let Status::Failed(e) = &result {
        tracing::warn!("update check: {e}");
    }
    set(status, ctx, result);
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(120))).build())
}

/// The newer build for this platform, if the manifest lists one.
fn fetch_newer() -> Result<Option<(String, String, String, String)>, String> {
    let url = std::env::var("T7_UPDATE_URL").unwrap_or_else(|_| MANIFEST_URL.to_owned());
    let mut response = agent().get(&url).call().map_err(|e| format!("couldn't reach the update server: {e}"))?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_MANIFEST)
        .read_to_string()
        .map_err(|e| format!("couldn't read the update manifest: {e}"))?;
    let manifest: Manifest = serde_json::from_str(&body).map_err(|e| format!("bad update manifest: {e}"))?;
    let (Some(latest), Some(current)) = (parse_version(&manifest.version), parse_version(VERSION)) else {
        return Err(format!("bad version {}", manifest.version));
    };
    if latest <= current {
        return Ok(None);
    }
    let target = current_target();
    let asset = manifest
        .assets
        .into_iter()
        .find(|a| a.target == target)
        .ok_or_else(|| format!("version {} has no build for {target}", manifest.version))?;
    Ok(Some((manifest.version, asset.url, asset.sha256, asset.signature)))
}

fn download(version: &str, url: &str, sha256: &str, signature: &str) -> Result<(), String> {
    let mut response = agent().get(url).call().map_err(|e| format!("couldn't download the update: {e}"))?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_BUILD)
        .read_to_vec()
        .map_err(|e| format!("couldn't download the update: {e}"))?;
    verify(version, &current_target(), &bytes, sha256, signature)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let path = update_path(&exe);
    let tmp = path.with_extension("partial");
    std::fs::write(&tmp, &bytes)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("couldn't save the update next to {}: {e}", exe.display()))
}

/// Accepts `bytes` only if they hash to `sha256` and are signed for this version and platform.
fn verify(version: &str, target: &str, bytes: &[u8], sha256: &str, signature: &str) -> Result<(), String> {
    let actual = to_hex(&Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(sha256) {
        return Err("the downloaded update is corrupt (checksum mismatch)".into());
    }
    let key: [u8; 32] = from_hex(PUBLIC_KEY).and_then(|k| k.try_into().ok()).expect("valid built-in key");
    let key = VerifyingKey::from_bytes(&key).expect("valid built-in key");
    let signature: [u8; 64] =
        from_hex(signature).and_then(|s| s.try_into().ok()).ok_or("the update's signature is malformed")?;
    key.verify(signed_message(version, target, &actual).as_bytes(), &Signature::from_bytes(&signature))
        .map_err(|_| "the update isn't signed by the T7 release key; ignoring it".into())
}

fn update_path(exe: &Path) -> PathBuf {
    exe.with_extension("update")
}

fn old_path(exe: &Path) -> PathBuf {
    exe.with_extension("old")
}

fn install_update() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let (update, old) = (update_path(&exe), old_path(&exe));
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old)?;
    if let Err(e) = std::fs::rename(&update, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(e);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))?;
    }
    tracing::info!("installed update into {}", exe.display());
    Ok(exe)
}

/// The executable the last update replaced; it can go once we're running.
fn remove_previous_version() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_path(&exe));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signed(key: &SigningKey, version: &str, target: &str, bytes: &[u8]) -> (String, String) {
        let sha = to_hex(&Sha256::digest(bytes));
        let sig = key.sign(signed_message(version, target, &sha).as_bytes());
        (sha, to_hex(&sig.to_bytes()))
    }

    #[test]
    fn rejects_corrupt_and_foreign_builds() {
        let bytes = b"pretend executable";
        // A key that isn't T7's release key: correctly signed, still refused.
        let stranger = SigningKey::from_bytes(&[7u8; 32]);
        let (sha, sig) = signed(&stranger, "9.9.9", "windows-x86_64", bytes);
        let err = verify("9.9.9", "windows-x86_64", bytes, &sha, &sig).unwrap_err();
        assert!(err.contains("isn't signed"), "{err}");
        // Tampered bytes fail the checksum before the signature is even considered.
        let err = verify("9.9.9", "windows-x86_64", b"tampered", &sha, &sig).unwrap_err();
        assert!(err.contains("corrupt"), "{err}");
        assert!(verify("9.9.9", "windows-x86_64", bytes, &sha, "zz").is_err());
    }

    #[test]
    fn builtin_key_is_valid() {
        let key: [u8; 32] = from_hex(PUBLIC_KEY).unwrap().try_into().unwrap();
        assert!(VerifyingKey::from_bytes(&key).is_ok());
    }
}
