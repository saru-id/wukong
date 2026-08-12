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

/// Where the passphrase-encrypted identity escrow lives — in the
/// SHARED lane, so every clone can offer `seal-key restore`. The
/// passphrase never leaves the user's head; losing every machine
/// costs one passphrase, not the sealed files.
pub const ESCROW_REL: &str = "__wukong__/age.key.enc";

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

/// Passphrase-encrypt bytes (scrypt under the hood) — the escrow
/// path for the identity itself.
pub fn encrypt_with_passphrase(passphrase: &str, plaintext: &[u8]) -> Result<Vec<u8>, SealError> {
    let encryptor = age::Encryptor::with_user_passphrase(age::secrecy::SecretString::from(
        passphrase.to_string(),
    ));
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

pub fn decrypt_with_passphrase(passphrase: &str, ciphertext: &[u8]) -> Result<Vec<u8>, SealError> {
    let decryptor =
        age::Decryptor::new(ciphertext).map_err(|e| SealError::Crypto(e.to_string()))?;
    let identity =
        age::scrypt::Identity::new(age::secrecy::SecretString::from(passphrase.to_string()));
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|e| SealError::Crypto(e.to_string()))?;
    let mut out = Vec::new();
    reader.read_to_end(&mut out)?;
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

/// `errSecItemNotFound`, as `security(1)` exits with it. The ONE code
/// that means "there is no such item"; every other failure means the
/// keychain would not answer, which is a different thing entirely.
const ERR_SEC_ITEM_NOT_FOUND: i32 = 44;

/// The keychain is asked from the daemon's event loop. An unlock
/// prompt nobody is present to click, or a securityd that stopped
/// answering, must cost the seal probe — not the governor.
const KEYCHAIN_TIMEOUT_SECS: u64 = 10;

impl IdentityStore {
    #[must_use]
    pub fn from_config(identity_file: Option<&Path>) -> Self {
        identity_file.map_or(Self::Keychain, |p| Self::File(p.to_path_buf()))
    }

    /// Load the identity. `Ok(None)` means the store answered and there
    /// is genuinely nothing there — never "the store would not talk to
    /// us", which is an error and must stay one: callers treat `None`
    /// as permission to generate a fresh key pair, and generating over
    /// a key that was merely unreachable orphans every sealed blob in
    /// the store, permanently and silently.
    pub fn load(&self) -> Result<Option<String>, SealError> {
        match self {
            Self::Keychain => {
                let out = crate::proc::bounded(
                    "security",
                    &[
                        "find-generic-password",
                        "-a",
                        KEYCHAIN_ACCOUNT,
                        "-s",
                        KEYCHAIN_SERVICE,
                        "-w",
                    ],
                    KEYCHAIN_TIMEOUT_SECS,
                )
                .ok_or_else(|| {
                    SealError::Crypto(
                        "the keychain did not answer in time — is securityd wedged?".to_string(),
                    )
                })?;
                if out.status.success() {
                    return Ok(Some(
                        String::from_utf8_lossy(&out.stdout).trim().to_string(),
                    ));
                }
                if out.status.code() == Some(ERR_SEC_ITEM_NOT_FOUND) {
                    return Ok(None);
                }
                // A locked keychain lands here (152), as does a denied
                // prompt and anything else securityd objects to — and
                // it often says nothing at all, so the exit code is the
                // only thing left to report.
                let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let why = if why.is_empty() {
                    format!(
                        "exit {} (a locked keychain answers 152)",
                        out.status.code().unwrap_or(-1)
                    )
                } else {
                    why
                };
                Err(SealError::Crypto(format!(
                    "the keychain refused to hand over the seal identity: {why}"
                )))
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
    ///
    /// A write to a LOCKED keychain blocks forever — measured: ten
    /// minutes and still waiting on an unlock prompt with nobody
    /// present to answer it. The read path fails fast (exit 152); this
    /// one just stops. The wall clock is what keeps that from being a
    /// wedged governor, and it also bounds the second problem: the
    /// identity travels in `security`'s argv, readable in `ps` for as
    /// long as the call runs. `security(1)` offers no stdin path for
    /// it — `-w` with no value eats the next positional, and its
    /// interactive prompt wants a tty no daemon has — so the honest
    /// statement is that the key is exposed for at most the timeout,
    /// not that it isn't exposed.
    pub fn save(&self, identity: &str) -> Result<(), SealError> {
        match self {
            Self::Keychain => {
                let out = crate::proc::bounded(
                    "security",
                    &[
                        "add-generic-password",
                        "-a",
                        KEYCHAIN_ACCOUNT,
                        "-s",
                        KEYCHAIN_SERVICE,
                        "-U",
                        "-w",
                        identity,
                    ],
                    KEYCHAIN_TIMEOUT_SECS,
                )
                .ok_or_else(|| {
                    SealError::Crypto(
                        "the keychain did not answer in time — is securityd wedged?".to_string(),
                    )
                })?;
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
    fn escrow_round_trips_and_wrong_passphrase_fails() {
        let (identity, _) = generate();
        let blob = encrypt_with_passphrase("horse battery", identity.as_bytes()).unwrap();
        assert!(is_sealed(&blob), "escrow blob is ordinary age ciphertext");
        let back = decrypt_with_passphrase("horse battery", &blob).unwrap();
        assert_eq!(back, identity.as_bytes());
        assert!(decrypt_with_passphrase("wrong", &blob).is_err());
        // And the x25519 path refuses a scrypt blob rather than panic.
        assert!(decrypt(&identity, &blob).is_err());
    }

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

    /// "There is no key" and "I could not read the key" must never be
    /// the same answer: the first is permission to make a new one, and
    /// making a new one over the second orphans every sealed blob in
    /// the store. The keychain says this with exit 44 versus anything
    /// else; a file store says it with `NotFound` versus anything else.
    #[test]
    fn an_unreadable_identity_store_is_an_error_not_an_empty_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(
            IdentityStore::File(tmp.path().join("never-written"))
                .load()
                .unwrap()
                .is_none(),
            "an absent identity is genuinely absent"
        );
        let unreadable = tmp.path().join("age.key");
        std::fs::create_dir_all(&unreadable).unwrap();
        assert!(
            IdentityStore::File(unreadable).load().is_err(),
            "an unreadable identity must not read as absent"
        );
    }
}
