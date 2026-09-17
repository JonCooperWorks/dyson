//! Configure-secret enrollment, verification cache, and boot-time preseeding.

use super::{HttpState, Resp, bad_request, unauthorized};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

// Header swarm sends with the per-instance configure secret
// (32-hex plaintext from `Uuid::new_v4().simple()`).  Dyson hashes
// it on first sighting (TOFU) and verifies on every subsequent call.
// The name is compared case-insensitively (hyper `HeaderMap::get`
// lowercases the lookup key), so the shared const's canonical casing
// (`X-Swarm-Configure`) matches swarm's wire header.
use dyson_common::contracts::DYSON_CONFIGURE_HEADER;

/// Filename inside the dyson home dir that holds the argon2id hash
/// of the configure secret.  Lives next to `workspace/`, persists
/// across cube restores (it's in the writable layer).  PHC string
/// format (`$argon2id$v=19$...`) so argon2's verifier can re-derive
/// the salt.
pub(super) const CONFIGURE_HASH_FILENAME: &str = "configure_secret_hash";
const CONFIGURE_PRESEED_FILENAME: &str = "configure.preseed";
const CONFIGURE_VERIFY_CACHE_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ConfigureVerifyCacheKey {
    hash_path: PathBuf,
    secret_digest_prefix: [u8; 16],
    stored_digest_prefix: [u8; 16],
}

static CONFIGURE_AUTH_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
    std::sync::OnceLock::new();
static CONFIGURE_VERIFY_CACHE: std::sync::OnceLock<
    std::sync::Mutex<HashMap<ConfigureVerifyCacheKey, Instant>>,
> = std::sync::OnceLock::new();

pub(super) async fn authorize_configure(
    headers: &hyper::HeaderMap,
    state: &HttpState,
) -> Option<Resp> {
    let secret = match headers
        .get(DYSON_CONFIGURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => s.to_owned(),
        None => return Some(unauthorized(state)),
    };

    // Resolve the hash file's path.  Living next to `workspace/`
    // means a cube template restore picks it up via the writable
    // layer — same spot dyson_home resolves to from
    // `dyson swarm`'s DYSON_HOME env (default /var/lib/dyson).
    let snapshot = state.settings_snapshot();
    let hash_dir = workspace_parent_dir(snapshot.workspace.connection_string.expose());
    let hash_path = hash_dir.join(CONFIGURE_HASH_FILENAME);

    let _guard = CONFIGURE_AUTH_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;

    match tokio::fs::read_to_string(&hash_path).await {
        Ok(stored) => verify_configure_secret(&hash_path, secret, stored, state).await,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let preseed_path = hash_dir.join(CONFIGURE_PRESEED_FILENAME);
            let remove_preseed_after_write = match tokio::fs::read_to_string(&preseed_path).await {
                Ok(preseed) => {
                    let preseed = preseed.trim().to_owned();
                    if preseed.is_empty() {
                        return Some(bad_request("configure preseed is empty"));
                    }
                    if !configure_secret_eq(&secret, &preseed) {
                        return Some(unauthorized(state));
                    }
                    true
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => {
                    return Some(bad_request(&format!(
                        "read {}: {e}",
                        preseed_path.display()
                    )));
                }
            };
            let hash = match hash_configure_secret(secret.clone()).await {
                ConfigureHashOutcome::Hashed(hash) => hash,
                ConfigureHashOutcome::Failed(msg) => {
                    return Some(bad_request(&format!("argon2: {msg}")));
                }
            };
            if let Err(e) = tokio::fs::create_dir_all(&hash_dir).await {
                return Some(bad_request(&format!("mkdir {}: {e}", hash_dir.display())));
            }

            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&hash_path)
                .await
            {
                Ok(mut file) => {
                    if let Err(e) = file.write_all(hash.as_bytes()).await {
                        return Some(bad_request(&format!("write {}: {e}", hash_path.display())));
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = tokio::fs::set_permissions(
                            &hash_path,
                            std::fs::Permissions::from_mode(0o600),
                        )
                        .await;
                    }
                    remember_configure_verify(&hash_path, &secret, &hash);
                    if remove_preseed_after_write {
                        let _ = tokio::fs::remove_file(&preseed_path).await;
                    }
                    None
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    match tokio::fs::read_to_string(&hash_path).await {
                        Ok(stored) => {
                            verify_configure_secret(&hash_path, secret, stored, state).await
                        }
                        Err(e) => Some(bad_request(&format!("read {}: {e}", hash_path.display()))),
                    }
                }
                Err(e) => Some(bad_request(&format!("create {}: {e}", hash_path.display()))),
            }
        }
        Err(e) => Some(bad_request(&format!("read {}: {e}", hash_path.display()))),
    }
}

fn configure_secret_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

enum ConfigureHashOutcome {
    Hashed(String),
    Failed(String),
}

async fn hash_configure_secret(secret: String) -> ConfigureHashOutcome {
    tokio::task::spawn_blocking(move || {
        use argon2::Argon2;
        use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng};

        let salt = SaltString::generate(&mut OsRng);
        match Argon2::default().hash_password(secret.as_bytes(), &salt) {
            Ok(hash) => ConfigureHashOutcome::Hashed(hash.to_string()),
            Err(e) => ConfigureHashOutcome::Failed(e.to_string()),
        }
    })
    .await
    .unwrap_or_else(|e| ConfigureHashOutcome::Failed(format!("worker join failed: {e}")))
}

enum ConfigureVerifyOutcome {
    Verified,
    BadSecret,
    Unreadable(String),
}

async fn verify_configure_secret(
    hash_path: &Path,
    secret: String,
    stored: String,
    state: &HttpState,
) -> Option<Resp> {
    let stored = stored.trim().to_owned();
    if configure_verify_cache_hit(hash_path, &secret, &stored) {
        return None;
    }

    let secret_for_worker = secret.clone();
    let stored_for_worker = stored.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        use argon2::Argon2;
        use argon2::password_hash::{PasswordHash, PasswordVerifier};

        let parsed = match PasswordHash::new(&stored_for_worker) {
            Ok(parsed) => parsed,
            Err(e) => return ConfigureVerifyOutcome::Unreadable(e.to_string()),
        };
        match Argon2::default().verify_password(secret_for_worker.as_bytes(), &parsed) {
            Ok(()) => ConfigureVerifyOutcome::Verified,
            Err(_) => ConfigureVerifyOutcome::BadSecret,
        }
    })
    .await
    .unwrap_or_else(|e| ConfigureVerifyOutcome::Unreadable(format!("worker join failed: {e}")));

    match outcome {
        ConfigureVerifyOutcome::Verified => {
            remember_configure_verify(hash_path, &secret, &stored);
            None
        }
        ConfigureVerifyOutcome::BadSecret => Some(unauthorized(state)),
        ConfigureVerifyOutcome::Unreadable(msg) => {
            Some(bad_request(&format!("stored hash unreadable: {msg}")))
        }
    }
}

fn configure_verify_cache_hit(hash_path: &Path, secret: &str, stored: &str) -> bool {
    let key = configure_verify_cache_key(hash_path, secret, stored);
    let now = Instant::now();
    let cache = CONFIGURE_VERIFY_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    guard.retain(|_, expires_at| *expires_at > now);
    guard.get(&key).is_some_and(|expires_at| *expires_at > now)
}

fn remember_configure_verify(hash_path: &Path, secret: &str, stored: &str) {
    let key = configure_verify_cache_key(hash_path, secret, stored);
    let cache = CONFIGURE_VERIFY_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    guard.insert(key, Instant::now() + CONFIGURE_VERIFY_CACHE_TTL);
}

fn configure_verify_cache_key(
    hash_path: &Path,
    secret: &str,
    stored: &str,
) -> ConfigureVerifyCacheKey {
    let mut secret_digest_prefix = [0_u8; 16];
    let secret_digest = Sha256::digest(secret.as_bytes());
    secret_digest_prefix.copy_from_slice(&secret_digest[..16]);

    let mut stored_digest_prefix = [0_u8; 16];
    let stored_digest = Sha256::digest(stored.as_bytes());
    stored_digest_prefix.copy_from_slice(&stored_digest[..16]);

    ConfigureVerifyCacheKey {
        hash_path: hash_path.to_path_buf(),
        secret_digest_prefix,
        stored_digest_prefix,
    }
}

/// Resolve the directory the configure-secret hash lives in.  We
/// keep it next to the workspace so cube template restores preserve
/// it via the writable layer.  `connection_string` for the in-memory
/// Consume `<dyson_home>/configure.preseed` at boot, hashing its
/// contents into `configure_secret_hash` so the first
/// `/api/admin/configure` POST verifies against the swarm-supplied
/// secret instead of TOFU-minting a hash from whatever caller wins
/// the race. Returns whether a preseed file was found.
///
/// Idempotent: if `configure_secret_hash` already exists, the preseed
/// file is still removed (so a stale plaintext is not left on disk)
/// but the existing hash is preserved.
///
/// Best-effort: the preseed file is removed after a successful hash
/// write. If hashing or writing fails, the preseed file is left in
/// place so the next boot can retry.
pub fn preseed_configure_hash(dyson_home: &Path) -> std::io::Result<bool> {
    let preseed_path = dyson_home.join(CONFIGURE_PRESEED_FILENAME);
    let hash_path = dyson_home.join(CONFIGURE_HASH_FILENAME);

    let secret = match std::fs::read_to_string(&preseed_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let secret = secret.trim_end_matches(['\n', '\r']).to_owned();

    if hash_path.exists() {
        // Hash already minted (warm restart). Drop the preseed plaintext
        // and keep the existing hash so an attacker who plants a fresh
        // preseed file cannot overwrite swarm's verified hash.
        let _ = std::fs::remove_file(&preseed_path);
        return Ok(false);
    }

    use argon2::Argon2;
    use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng};
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .map_err(|e| std::io::Error::other(format!("argon2: {e}")))?
        .to_string();

    std::fs::create_dir_all(dyson_home)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = match opts.open(&hash_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Lost the race against a concurrent writer. Drop the preseed.
            let _ = std::fs::remove_file(&preseed_path);
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    use std::io::Write;
    file.write_all(hash.as_bytes())?;
    drop(file);

    let _ = std::fs::remove_file(&preseed_path);
    Ok(true)
}

/// workspace is its directory path; for the file-backed default
/// it's the directory directly.  For unknown shapes we fall back to
/// `/var/lib/dyson` which matches `dyson swarm`'s default home.
fn workspace_parent_dir(connection_string: &str) -> std::path::PathBuf {
    let p = std::path::PathBuf::from(connection_string);
    if let Some(parent) = p.parent().filter(|p| !p.as_os_str().is_empty()) {
        parent.to_path_buf()
    } else {
        std::path::PathBuf::from("/var/lib/dyson")
    }
}
