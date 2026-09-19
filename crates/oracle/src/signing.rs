//! The oracle's signing key and the attestation rules that protect it.
//!
//! An attestation is the Schnorr value `s = k + e·d` for the event nonce `k`
//! and the oracle key `d`. Anyone who learns `k` for a signed event, or sees
//! two attestations made with one `k`, can solve for `d`. So:
//!
//! - `k` never leaves this module. It is derived on demand from `d`, the
//!   event id, and a random per-event salt, and is never stored or served.
//!   Only the nonce point `R = k·G` and the salt are persisted.
//! - Each event is attested at most once: the database refuses a second
//!   attestation, and [`SigningKey::attest`] only signs outcomes the event
//!   announced.
//! - The key file must be private (mode 0600 on Unix); owned copies of the
//!   secret are erased on drop.

use dlctix::{
    attestation_locking_point, attestation_secret,
    musig2::secp256k1::{PublicKey, Secp256k1, SecretKey, XOnlyPublicKey},
    secp::{MaybePoint, MaybeScalar, Point, Scalar},
};
use nostr::{key::PublicKey as NostrPublicKey, nips::nip19::ToBech32};
use pem_rfc7468::LineEnding;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
};
use uuid::Uuid;
use zeroize::Zeroizing;

const PEM_LABEL: &str = "EC PRIVATE KEY";
const NONCE_TAG: &[u8] = b"noaa-oracle/event-nonce/v1";
/// PEM for a 32-byte key is well under this; anything larger is not a key.
const MAX_KEY_FILE_BYTES: u64 = 1024;

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("key file {path} must end in .pem")]
    Extension { path: String },
    #[error("read key file {path}")]
    Read { path: String, source: io::Error },
    #[error("create key file {path}")]
    Create { path: String, source: io::Error },
    #[error("key file {path} is readable by other users (mode {mode:o}); run chmod 600")]
    Permissions { path: String, mode: u32 },
    #[error("key file {path} is larger than a key")]
    TooLarge { path: String },
    #[error("key file {path} is not a PEM `{PEM_LABEL}`")]
    Format { path: String },
    #[error("key file {path} does not hold a valid secp256k1 secret key")]
    InvalidKey { path: String },
}

#[derive(Debug, thiserror::Error)]
pub enum AttestError {
    #[error("stored nonce point does not match the key; the event was created by another key")]
    NonceMismatch,
    #[error("outcome is not one of the announced outcomes")]
    UnannouncedOutcome,
}

/// Per-event nonce material that is safe to store and publish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventNonce {
    /// Random bytes mixed into the nonce derivation so a recreated event id
    /// never reuses a nonce.
    pub salt: [u8; 32],
    /// `R = k·G`, published so participants can check locking points.
    pub point: Point,
}

/// The oracle's secp256k1 secret. Not `Clone`, `Debug`, or `Serialize`;
/// erased when dropped.
pub struct SigningKey {
    secret: SecretKey,
    public: PublicKey,
}

impl Drop for SigningKey {
    fn drop(&mut self) {
        self.secret.non_secure_erase();
    }
}

impl SigningKey {
    /// Loads the key at `path`, or creates it with mode 0600 when missing.
    pub fn load_or_create(path: &Path) -> Result<Self, KeyError> {
        let display = || path.display().to_string();
        if path.extension().and_then(|extension| extension.to_str()) != Some("pem") {
            return Err(KeyError::Extension { path: display() });
        }
        match fs::metadata(path) {
            Ok(_) => Self::read(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let key = Self::from_secret(SecretKey::new(&mut rand::rng()));
                key.write_new(path)?;
                Ok(key)
            }
            Err(source) => Err(KeyError::Read {
                path: display(),
                source,
            }),
        }
    }

    fn from_secret(secret: SecretKey) -> Self {
        let public = secret.public_key(&Secp256k1::signing_only());
        Self { secret, public }
    }

    fn read(path: &Path) -> Result<Self, KeyError> {
        let display = || path.display().to_string();
        let metadata = fs::metadata(path).map_err(|source| KeyError::Read {
            path: display(),
            source,
        })?;
        check_private(path, &metadata)?;
        if metadata.len() > MAX_KEY_FILE_BYTES {
            return Err(KeyError::TooLarge { path: display() });
        }
        let pem = Zeroizing::new(fs::read(path).map_err(|source| KeyError::Read {
            path: display(),
            source,
        })?);
        let (label, der) =
            pem_rfc7468::decode_vec(&pem).map_err(|_| KeyError::Format { path: display() })?;
        let der = Zeroizing::new(der);
        if label != PEM_LABEL {
            return Err(KeyError::Format { path: display() });
        }
        let bytes: Zeroizing<[u8; 32]> = Zeroizing::new(
            der.as_slice()
                .try_into()
                .map_err(|_| KeyError::InvalidKey { path: display() })?,
        );
        let secret = SecretKey::from_byte_array(*bytes)
            .map_err(|_| KeyError::InvalidKey { path: display() })?;
        Ok(Self::from_secret(secret))
    }

    fn write_new(&self, path: &Path) -> Result<(), KeyError> {
        let create = |source| KeyError::Create {
            path: path.display().to_string(),
            source,
        };
        let secret = Zeroizing::new(self.secret.secret_bytes());
        let pem = Zeroizing::new(
            pem_rfc7468::encode_string(PEM_LABEL, LineEnding::LF, secret.as_slice())
                .map_err(|error| create(io::Error::other(error)))?,
        );
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(path).map_err(create)?;
        file.write_all(pem.as_bytes()).map_err(create)?;
        file.sync_all().map_err(create)
    }

    pub fn public_key(&self) -> PublicKey {
        self.public
    }

    pub fn x_only_public_key(&self) -> XOnlyPublicKey {
        self.public.x_only_public_key().0
    }

    /// The oracle's nostr identity: the same key as an npub.
    pub fn npub(&self) -> String {
        let key = NostrPublicKey::from_byte_array(self.x_only_public_key().serialize());
        let Ok(npub) = key.to_bech32();
        npub
    }

    /// Picks nonce material for a new event.
    pub fn new_event_nonce(&self, event_id: Uuid) -> EventNonce {
        let salt: [u8; 32] = rand::random();
        EventNonce {
            salt,
            point: self.event_nonce_secret(event_id, &salt).base_point_mul(),
        }
    }

    /// `k = H(tag || d || event_id || salt) mod n`. Deterministic, so the
    /// secret is recomputed at signing time instead of being stored.
    fn event_nonce_secret(&self, event_id: Uuid, salt: &[u8; 32]) -> Scalar {
        let secret = Zeroizing::new(self.secret.secret_bytes());
        let mut counter: u8 = 0;
        loop {
            let digest: Zeroizing<[u8; 32]> = Zeroizing::new(
                Sha256::new()
                    .chain_update(NONCE_TAG)
                    .chain_update(secret.as_slice())
                    .chain_update(event_id.as_bytes())
                    .chain_update(salt)
                    .chain_update([counter])
                    .finalize()
                    .into(),
            );
            // A zero scalar has probability ~2^-256; retry rather than fail.
            if let Ok(nonce) = MaybeScalar::reduce_from(&digest).not_zero() {
                return nonce;
            }
            counter = counter.wrapping_add(1);
        }
    }

    /// Locking point for `message` under this key and `nonce_point`.
    pub fn locking_point(&self, nonce_point: Point, message: &[u8]) -> MaybePoint {
        attestation_locking_point(self.public, nonce_point, message)
    }

    /// Attests `message` for an event. Refuses when the nonce does not
    /// belong to this key or the message's locking point was not announced,
    /// so a signature is only ever produced for an announced outcome.
    pub fn attest(
        &self,
        event_id: Uuid,
        nonce: &EventNonce,
        announced: &[MaybePoint],
        message: &[u8],
    ) -> Result<MaybeScalar, AttestError> {
        let secret_nonce = self.event_nonce_secret(event_id, &nonce.salt);
        if secret_nonce.base_point_mul() != nonce.point {
            return Err(AttestError::NonceMismatch);
        }
        let locking_point = self.locking_point(nonce.point, message);
        if !matches!(locking_point, MaybePoint::Valid(_)) || !announced.contains(&locking_point) {
            return Err(AttestError::UnannouncedOutcome);
        }
        let attestation = attestation_secret(self.secret, secret_nonce, message);
        debug_assert_eq!(attestation.base_point_mul(), locking_point);
        Ok(attestation)
    }
}

#[cfg(unix)]
fn check_private(path: &Path, metadata: &fs::Metadata) -> Result<(), KeyError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(KeyError::Permissions {
            path: path.display().to_string(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_path: &Path, _metadata: &fs::Metadata) -> Result<(), KeyError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dlctix::attestation_locking_point;

    fn key_in(directory: &tempfile::TempDir) -> SigningKey {
        SigningKey::load_or_create(&directory.path().join("oracle.pem")).unwrap()
    }

    #[test]
    fn keys_round_trip_through_private_pem_files() {
        let directory = tempfile::tempdir().unwrap();
        let created = key_in(&directory);
        let reloaded = key_in(&directory);
        assert_eq!(created.public_key(), reloaded.public_key());
        assert!(matches!(
            SigningKey::load_or_create(&directory.path().join("oracle.key")),
            Err(KeyError::Extension { .. })
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = directory.path().join("oracle.pem");
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn readable_key_files_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oracle.pem");
        drop(key_in(&directory));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            SigningKey::load_or_create(&path),
            Err(KeyError::Permissions { mode: 0o644, .. })
        ));
    }

    #[test]
    fn malformed_key_files_are_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oracle.pem");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let pem = pem_rfc7468::encode_string("PUBLIC KEY", LineEnding::LF, &[7; 32]).unwrap();
        options
            .open(&path)
            .unwrap()
            .write_all(pem.as_bytes())
            .unwrap();
        assert!(matches!(
            SigningKey::load_or_create(&path),
            Err(KeyError::Format { .. })
        ));
    }

    #[test]
    fn nonces_are_derived_per_event_and_salt() {
        let directory = tempfile::tempdir().unwrap();
        let key = key_in(&directory);
        let event = Uuid::now_v7();
        let first = key.new_event_nonce(event);
        let second = key.new_event_nonce(event);
        assert_ne!(
            first.point, second.point,
            "a recreated event gets a new nonce"
        );
        assert_eq!(
            key.event_nonce_secret(event, &first.salt).base_point_mul(),
            first.point
        );
        assert_ne!(
            key.event_nonce_secret(Uuid::now_v7(), &first.salt)
                .base_point_mul(),
            first.point
        );
    }

    #[test]
    fn only_announced_outcomes_are_attested() {
        let directory = tempfile::tempdir().unwrap();
        let key = key_in(&directory);
        let event = Uuid::now_v7();
        let nonce = key.new_event_nonce(event);
        let announced: Vec<MaybePoint> = [b"a".as_slice(), b"b".as_slice()]
            .iter()
            .map(|message| key.locking_point(nonce.point, message))
            .collect();

        let attestation = key.attest(event, &nonce, &announced, b"b").unwrap();
        assert_eq!(
            attestation.base_point_mul(),
            attestation_locking_point(key.public_key(), nonce.point, b"b")
        );
        assert!(matches!(
            key.attest(event, &nonce, &announced, b"c"),
            Err(AttestError::UnannouncedOutcome)
        ));
        let foreign = EventNonce {
            salt: nonce.salt,
            point: Scalar::random(&mut rand::rng()).base_point_mul(),
        };
        assert!(matches!(
            key.attest(event, &foreign, &announced, b"a"),
            Err(AttestError::NonceMismatch)
        ));
    }

    #[test]
    fn npub_is_derived_from_the_public_key() {
        let directory = tempfile::tempdir().unwrap();
        let key = key_in(&directory);
        let npub = key.npub();
        assert!(npub.starts_with("npub1"));
        let parsed = NostrPublicKey::parse(&npub).unwrap();
        assert_eq!(parsed.to_bytes(), key.x_only_public_key().serialize());
    }
}
