//! The sealed lane: age encryption for files whose secrets should
//! sync but never sit plaintext on any remote. Sealing upgrades the
//! gate's promise from "the remote never sees an unreviewed secret"
//! to "the remote never sees a plaintext secret, period."
//!
//! Key model: ONE x25519 identity shared by the user's machines. The
//! private identity lives in the macOS Keychain (or a 0600 file when
//! configured — sandboxed runs); it is NEVER in the store. The public
//! recipient lives in the store at `__wukong__/age.recipient` so any
//! clone can ENCRYPT; only identity holders decrypt. Moving the
//! identity between machines is explicit: `wukong seal-key export` /
//! `import`, through a channel the user trusts.
//!
//! age encryption is deliberately non-deterministic (fresh file key
//! per encryption), so the engine must gate sealed commits on a
//! plaintext content hash — re-encrypting unchanged content would
//! produce an endless stream of ciphertext-only commits.

use age::secrecy::ExposeSecret as _;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

/// Where the PUBLIC recipient lives inside the store repo.
pub const RECIPIENT_REL: &str = "__wukong__/age.recipient";

/// The age binary format's magic — how sealed blobs are recognized.
const AGE_MAGIC: &[u8] = b"age-encryption.org/v1";

#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error(
        "seal identity is missing — create one with `wukong seal <file>` or import with `wukong seal-key import`"
    )]
    NoIdentity,
    #[error("seal error: {0}")]
    Crypto(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Full SHA-256 of plaintext, hex — the sealed lane's determinism
/// guard compares these instead of ciphertext.
#[must_use]
pub fn content_hash(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Is this stored blob sealed?
#[must_use]
pub fn is_sealed(bytes: &[u8]) -> bool {
    bytes.starts_with(AGE_MAGIC)
}

/// Generate a fresh identity; returns (secret identity string,
/// public recipient string). The caller owns getting the secret into
/// an [`IdentityStore`] and the recipient into the store repo.
#[must_use]
pub fn generate() -> (String, String) {
    let identity = age::x25519::Identity::generate();
    let recipient = identity.to_public().to_string();
    (identity.to_string().expose_secret().to_string(), recipient)
}

/// Encrypt plaintext for the recipient (binary age format).
pub fn encrypt(recipient: &str, plaintext: &[u8]) -> Result<Vec<u8>, SealError> {
    let recipient: age::x25519::Recipient = recipient
        .trim()
        .parse()
        .map_err(|e: &str| SealError::Crypto(e.to_string()))?;
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .map_err(|e| SealError::Crypto(e.to_string()))?;
    let mut out = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut out)
        .map_err(|e| SealError::Crypto(e.to_string()))?;
    writer.write_all(plaintext)?;
    writer
        .finish()
        .map_err(|e| SealError::Crypto(e.to_string()))?;
    Ok(out)
}

/// Decrypt a sealed blob with the identity.
pub fn decrypt(identity: &str, ciphertext: &[u8]) -> Result<Vec<u8>, SealError> {
    let identity: age::x25519::Identity = identity
        .trim()
        .parse()
        .map_err(|e: &str| SealError::Crypto(e.to_string()))?;
    let decryptor =
        age::Decryptor::new(ciphertext).map_err(|e| SealError::Crypto(e.to_string()))?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|e| SealError::Crypto(e.to_string()))?;
    let mut out = Vec::new();
    reader.read_to_end(&mut out)?;
    Ok(out)
}

/// Where the private identity lives. Keychain is the macOS-native
/// default (survives `wukong uninstall --purge`, rides system
/// backups); a file is for sandboxed runs and people with their own
/// key discipline.
#[derive(Debug, Clone)]
pub enum IdentityStore {
    Keychain,
    File(PathBuf),
}

const KEYCHAIN_SERVICE: &str = "id.saru.wukong.seal";
const KEYCHAIN_ACCOUNT: &str = "wukong";

impl IdentityStore {
    #[must_use]
    pub fn from_config(identity_file: Option<&Path>) -> Self {
        identity_file.map_or(Self::Keychain, |p| Self::File(p.to_path_buf()))
    }

    /// Load the identity, `Ok(None)` when none exists yet.
    pub fn load(&self) -> Result<Option<String>, SealError> {
        match self {
            Self::Keychain => {
                let out = std::process::Command::new("security")
                    .args([
                        "find-generic-password",
                        "-a",
                        KEYCHAIN_ACCOUNT,
                        "-s",
                        KEYCHAIN_SERVICE,
                        "-w",
                    ])
                    .output()?;
                if out.status.success() {
                    Ok(Some(
                        String::from_utf8_lossy(&out.stdout).trim().to_string(),
                    ))
                } else {
                    Ok(None)
                }
            }
            Self::File(path) => match std::fs::read_to_string(path) {
                Ok(text) => Ok(Some(text.trim().to_string())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            },
        }
    }

    /// Persist the identity (overwriting any previous one — import is
    /// deliberate).
    pub fn save(&self, identity: &str) -> Result<(), SealError> {
        match self {
            Self::Keychain => {
                let out = std::process::Command::new("security")
                    .args([
                        "add-generic-password",
                        "-a",
                        KEYCHAIN_ACCOUNT,
                        "-s",
                        KEYCHAIN_SERVICE,
                        "-U",
                        "-w",
                        identity,
                    ])
                    .output()?;
                if out.status.success() {
                    Ok(())
                } else {
                    Err(SealError::Crypto(format!(
                        "keychain refused the identity: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    )))
                }
            }
            Self::File(path) => {
                use std::os::unix::fs::OpenOptionsExt as _;
                if let Some(dir) = path.parent() {
                    crate::paths::ensure_private_dir(dir)?;
                }
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(path)?;
                file.write_all(identity.as_bytes())?;
                file.write_all(b"\n")?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_magic() {
        let (identity, recipient) = generate();
        let sealed = encrypt(&recipient, b"export TOKEN=verysecret\n").unwrap();
        assert!(is_sealed(&sealed));
        assert!(!is_sealed(b"export TOKEN=verysecret\n"));
        // Ciphertext never contains the plaintext.
        assert!(!sealed.windows(10).any(|w| w == b"verysecret".as_slice()));
        let opened = decrypt(&identity, &sealed).unwrap();
        assert_eq!(opened, b"export TOKEN=verysecret\n");
        // The wrong identity fails loudly, not quietly.
        let (other, _) = generate();
        assert!(decrypt(&other, &sealed).is_err());
    }

    #[test]
    fn encryption_is_nondeterministic_hence_the_hash_guard() {
        let (_, recipient) = generate();
        let a = encrypt(&recipient, b"same content").unwrap();
        let b = encrypt(&recipient, b"same content").unwrap();
        assert_ne!(a, b, "age uses a fresh file key per encryption");
    }

    #[test]
    fn file_identity_store_round_trips_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::TempDir::new().unwrap();
        let store = IdentityStore::File(tmp.path().join("age.key"));
        assert!(store.load().unwrap().is_none());
        let (identity, _) = generate();
        store.save(&identity).unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some(identity.as_str()));
        let mode = std::fs::metadata(tmp.path().join("age.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
