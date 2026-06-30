//! Persistent secret storage with an OS-keyring-first, encrypted-file-fallback strategy.
//!
//! The OS keyring is preferred (macOS Keychain, Windows Credential Manager, Linux Secret
//! Service). But it isn't always reachable: a headless box, or a Linux session where no
//! `org.freedesktop.secrets` provider is activatable, makes every keyring call fail — and the
//! kernel-keyutils backend that *does* always work is wiped on logout/reboot (the "keyring keeps
//! resetting, I had to re-enter my API keys" bug). So when the keyring is unavailable we fall
//! back to an encrypted file under the config dir: AEAD (ChaCha20-Poly1305) with a random key in
//! a sibling `0600` keyfile. That persists across reboots regardless of any daemon.
//!
//! Every public entry point here routes through [`get`]/[`set`]/[`delete`], which try the keyring
//! and transparently fall back to the file, so callers (provider keys, search keys, MCP tokens,
//! OAuth tokens) get durable storage without caring which backend answered.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::ConfigError;

const KEYRING_SERVICE: &str = "forge";

/// Max time to wait for the OS keyring backend to answer a probe before declaring it unusable for
/// the session. Generous enough for a slow-but-live Secret Service, short enough that a *dead* one
/// (the WSL/headless case) doesn't stall startup.
const KEYRING_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(800);

/// Whether the OS keyring backend is reachable — probed ONCE, with a timeout, and cached for the
/// process. This exists because on some boxes (WSL / headless Linux with an activatable-but-dead
/// `org.freedesktop.secrets`) a keyring call **blocks forever** instead of returning an error,
/// which hung `forge chat` before the TUI ever drew its first frame. We run the probe on a detached
/// thread and wait at most [`KEYRING_PROBE_TIMEOUT`]; if it doesn't answer we treat the keyring as
/// unavailable for the whole session and use the encrypted file store exclusively. A box with a
/// live keyring answers in milliseconds, so this is invisible there.
fn keyring_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Any return (Ok OR Err) means the backend ANSWERED within the window — real calls will
            // then also return promptly (and fall back to the file on their own Err). Only a true
            // hang never sends, tripping the recv timeout below. The detached thread is left to
            // unblock on its own rather than wedging the main path.
            let _ =
                keyring::Entry::new(KEYRING_SERVICE, "__forge_probe__").map(|e| e.get_password());
            let _ = tx.send(());
        });
        match rx.recv_timeout(KEYRING_PROBE_TIMEOUT) {
            Ok(()) => true,
            Err(_) => {
                tracing::warn!(
                    "OS keyring did not respond within {}ms — using the encrypted file store for \
                     this session (secrets are still durable)",
                    KEYRING_PROBE_TIMEOUT.as_millis()
                );
                false
            }
        }
    })
}

/// Store `value` under `key`: OS keyring first, encrypted file on keyring failure/unavailability.
pub fn set(key: &str, value: &str) -> Result<(), ConfigError> {
    if keyring_available()
        && keyring::Entry::new(KEYRING_SERVICE, key)
            .and_then(|e| e.set_password(value))
            .is_ok()
    {
        return Ok(());
    }
    file_set(key, value)
}

/// Read the secret for `key`: env-independent. Keyring first, then the encrypted file.
pub fn get(key: &str) -> Option<String> {
    if keyring_available() {
        if let Ok(v) = keyring::Entry::new(KEYRING_SERVICE, key).and_then(|e| e.get_password()) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    file_get(key)
}

/// Remove `key` from wherever it lives. `Ok(true)` if something was removed (from either store),
/// `Ok(false)` if nothing was stored — so removal stays idempotent.
pub fn delete(key: &str) -> Result<bool, ConfigError> {
    let mut removed = false;
    if keyring_available() {
        match keyring::Entry::new(KEYRING_SERVICE, key).and_then(|e| e.delete_credential()) {
            Ok(()) => removed = true,
            Err(keyring::Error::NoEntry) => {}
            Err(_) => {} // keyring unreachable — fall through to the file store
        }
    }
    removed |= file_delete(key)?;
    Ok(removed)
}

// --- encrypted file fallback ------------------------------------------------------------------

fn secrets_path() -> Option<PathBuf> {
    crate::config_dir().map(|d| d.join("secrets.enc"))
}

fn keyfile_path() -> Option<PathBuf> {
    crate::config_dir().map(|d| d.join("secret.key"))
}

/// Load (or create) the 32-byte file-store key. Stored `0600` next to the encrypted blob.
fn load_or_create_key() -> Result<Key, ConfigError> {
    let path = keyfile_path().ok_or_else(|| ConfigError::Keyring("no config dir".into()))?;
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == 32 {
            return Ok(Key::try_from(bytes.as_slice()).unwrap());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ConfigError::Keyring(e.to_string()))?;
    }
    let raw: [u8; 32] = rand::random();
    write_private(&path, &raw)?;
    Ok(Key::try_from(raw.as_slice()).unwrap())
}

/// Write a file readable/writable only by the owner (`0600` on Unix).
fn write_private(path: &PathBuf, bytes: &[u8]) -> Result<(), ConfigError> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| ConfigError::Keyring(e.to_string()))?;
    f.write_all(bytes)
        .map_err(|e| ConfigError::Keyring(e.to_string()))?;
    Ok(())
}

fn cipher() -> Result<ChaCha20Poly1305, ConfigError> {
    Ok(ChaCha20Poly1305::new(&load_or_create_key()?))
}

/// The on-disk map is `name -> base64(nonce ‖ ciphertext)`.
fn read_map() -> BTreeMap<String, String> {
    secrets_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_map(map: &BTreeMap<String, String>) -> Result<(), ConfigError> {
    let path = secrets_path().ok_or_else(|| ConfigError::Keyring("no config dir".into()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ConfigError::Keyring(e.to_string()))?;
    }
    let body = serde_json::to_vec_pretty(map).map_err(|e| ConfigError::Keyring(e.to_string()))?;
    write_private(&path, &body)
}

fn file_set(key: &str, value: &str) -> Result<(), ConfigError> {
    let cipher = cipher()?;
    let nonce_bytes: [u8; 12] = rand::random();
    let nonce = Nonce::try_from(nonce_bytes.as_slice()).unwrap();
    let ct = cipher
        .encrypt(&nonce, value.as_bytes())
        .map_err(|e| ConfigError::Keyring(e.to_string()))?;
    let mut blob = nonce_bytes.to_vec();
    blob.extend_from_slice(&ct);
    let encoded = base64::engine::general_purpose::STANDARD.encode(blob);
    let mut map = read_map();
    map.insert(key.to_string(), encoded);
    write_map(&map)
}

fn file_get(key: &str) -> Option<String> {
    let encoded = read_map().get(key)?.clone();
    let blob = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    if blob.len() < 12 {
        return None;
    }
    let (nonce_bytes, ct) = blob.split_at(12);
    let pt = cipher()
        .ok()?
        .decrypt(&Nonce::try_from(nonce_bytes).unwrap(), ct)
        .ok()?;
    String::from_utf8(pt).ok()
}

fn file_delete(key: &str) -> Result<bool, ConfigError> {
    let mut map = read_map();
    if map.remove(key).is_some() {
        write_map(&map)?;
        return Ok(true);
    }
    Ok(false)
}
