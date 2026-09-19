//! The daemon's upload identity: a secp256k1 key used to sign NIP-98
//! upload requests. The oracle accepts uploads only from npubs listed in
//! its `uploader_pubkeys`.
//!
//! The key file uses the oracle's format (PEM `EC PRIVATE KEY` holding the
//! raw 32-byte secret), is created with mode 0600, and is refused when
//! readable by group or others. Owned copies of the secret are zeroized.

use nostr::{
    key::{Keys, SecretKey},
    nips::nip19::ToBech32,
};
use pem_rfc7468::LineEnding;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
};
use zeroize::Zeroizing;

const PEM_LABEL: &str = "EC PRIVATE KEY";
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
    #[error("key file {path} is not a PEM `{PEM_LABEL}` holding a secp256k1 key")]
    Format { path: String },
}

/// Loads the key at `path`, or creates it with mode 0600 when missing.
pub fn load_or_create(path: &Path) -> Result<Keys, KeyError> {
    let display = || path.display().to_string();
    if path.extension().and_then(|extension| extension.to_str()) != Some("pem") {
        return Err(KeyError::Extension { path: display() });
    }
    match fs::metadata(path) {
        Ok(metadata) => read(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let keys = Keys::generate();
            write_new(path, &keys)?;
            Ok(keys)
        }
        Err(source) => Err(KeyError::Read {
            path: display(),
            source,
        }),
    }
}

/// The npub operators add to the oracle's `uploader_pubkeys`.
pub fn npub(keys: &Keys) -> String {
    let Ok(npub) = keys.public_key().to_bech32();
    npub
}

fn read(path: &Path, metadata: &fs::Metadata) -> Result<Keys, KeyError> {
    let display = || path.display().to_string();
    check_private(path, metadata)?;
    if metadata.len() > MAX_KEY_FILE_BYTES {
        return Err(KeyError::Format { path: display() });
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
    let secret = SecretKey::from_slice(&der).map_err(|_| KeyError::Format { path: display() })?;
    Ok(Keys::new(secret))
}

fn write_new(path: &Path, keys: &Keys) -> Result<(), KeyError> {
    let create = |source| KeyError::Create {
        path: path.display().to_string(),
        source,
    };
    let secret = Zeroizing::new(keys.secret_key().to_secret_bytes());
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

    #[test]
    fn keys_are_created_private_and_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.pem");
        let created = load_or_create(&path).unwrap();
        let reloaded = load_or_create(&path).unwrap();
        assert_eq!(created.public_key(), reloaded.public_key());
        assert!(npub(&created).starts_with("npub1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(matches!(
                load_or_create(&path),
                Err(KeyError::Permissions { .. })
            ));
        }
        assert!(matches!(
            load_or_create(&directory.path().join("daemon.key")),
            Err(KeyError::Extension { .. })
        ));
    }
}
