use crate::{Error, Result};
use hakutaku_core::ProjectId;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use zeroize::Zeroizing;

const MAGIC: &[u8; 8] = b"HAKID001";
const HEADER_SIZE: usize = 96;
const CHECKSUM_SIZE: usize = 32;

/// Runtime-only key material embedded into a packaged executable.
pub struct RuntimeKeyMaterial {
    /// First obfuscated share of the project root key.
    pub key_share_a: [u8; 32],
    /// Second share; XOR with `key_share_a` reconstructs the root key.
    pub key_share_b: [u8; 32],
    /// Ed25519 verification key for authenticating snapshots.
    pub public_key: [u8; 32],
}

/// Publisher-only identity. Never ship this file with a game.
pub struct Identity {
    project_id: ProjectId,
    root_key: Zeroizing<[u8; 32]>,
    public_key: [u8; 32],
    signing_pkcs8: Zeroizing<Vec<u8>>,
}

impl Identity {
    /// Generates a new project identity from the operating system CSPRNG.
    ///
    /// # Errors
    ///
    /// Returns an error if secure randomness or Ed25519 key generation fails.
    pub fn generate() -> Result<Self> {
        let rng = SystemRandom::new();
        let mut project_id = [0_u8; 16];
        let mut root_key = [0_u8; 32];
        rng.fill(&mut project_id)
            .map_err(|_| Error::Crypto("project ID generation"))?;
        rng.fill(&mut root_key)
            .map_err(|_| Error::Crypto("root key generation"))?;
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| Error::Crypto("Ed25519 key generation"))?;
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
            .map_err(|_| Error::Crypto("generated Ed25519 key validation"))?;
        let public_key = pair
            .public_key()
            .as_ref()
            .try_into()
            .map_err(|_| Error::Crypto("Ed25519 public key length"))?;
        Ok(Self {
            project_id: ProjectId(project_id),
            root_key: Zeroizing::new(root_key),
            public_key,
            signing_pkcs8: Zeroizing::new(pkcs8.as_ref().to_vec()),
        })
    }

    /// Loads and validates a publisher-only identity file.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O failures, corruption, or inconsistent key material.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let bytes = Zeroizing::new(std::fs::read(path)?);
        if bytes.len() < HEADER_SIZE + CHECKSUM_SIZE || bytes.get(..8) != Some(MAGIC) {
            return Err(Error::Identity("magic or length"));
        }
        if u16::from_le_bytes(bytes[8..10].try_into().expect("fixed slice")) != 1
            || u16::from_le_bytes(bytes[10..12].try_into().expect("fixed slice")) as usize
                != HEADER_SIZE
        {
            return Err(Error::Identity("version"));
        }
        let pkcs8_len = u32::from_le_bytes(bytes[92..96].try_into().expect("fixed slice")) as usize;
        let payload_len = identity_payload_len(pkcs8_len)?;
        if payload_len.checked_add(CHECKSUM_SIZE) != Some(bytes.len()) {
            return Err(Error::Identity("length"));
        }
        if blake3::hash(&bytes[..payload_len]).as_bytes() != &bytes[payload_len..] {
            return Err(Error::Identity("checksum"));
        }
        let signing_pkcs8 = bytes[HEADER_SIZE..payload_len].to_vec();
        let pair = Ed25519KeyPair::from_pkcs8(&signing_pkcs8)
            .map_err(|_| Error::Identity("Ed25519 private key"))?;
        let public_key: [u8; 32] = bytes[60..92].try_into().expect("fixed slice");
        if pair.public_key().as_ref() != public_key {
            return Err(Error::Identity("public/private key mismatch"));
        }
        Ok(Self {
            project_id: ProjectId(bytes[12..28].try_into().expect("fixed slice")),
            root_key: Zeroizing::new(bytes[28..60].try_into().expect("fixed slice")),
            public_key,
            signing_pkcs8: Zeroizing::new(signing_pkcs8),
        })
    }

    /// Atomically creates a publisher-only identity file without overwriting.
    ///
    /// # Errors
    ///
    /// Returns an error if the path exists or the durable write cannot complete.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if path.exists() {
            return Err(Error::InvalidInput(format!(
                "identity already exists: {}",
                path.display()
            )));
        }
        let pkcs8_len = u32::try_from(self.signing_pkcs8.len())
            .map_err(|_| Error::Identity("private key length"))?;
        let mut bytes = Zeroizing::new(Vec::with_capacity(
            HEADER_SIZE + self.signing_pkcs8.len() + CHECKSUM_SIZE,
        ));
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
        bytes.extend_from_slice(&self.project_id.0);
        bytes.extend_from_slice(self.root_key.as_ref());
        bytes.extend_from_slice(&self.public_key);
        bytes.extend_from_slice(&pkcs8_len.to_le_bytes());
        bytes.extend_from_slice(&self.signing_pkcs8);
        let checksum = blake3::hash(&bytes);
        bytes.extend_from_slice(checksum.as_bytes());

        let mut temporary_nonce = [0_u8; 8];
        SystemRandom::new()
            .fill(&mut temporary_nonce)
            .map_err(|_| Error::Crypto("identity temporary path generation"))?;
        let temporary = path.with_extension(format!(
            "part-{}-{:016x}",
            std::process::id(),
            u64::from_le_bytes(temporary_nonce)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        let publish = (|| -> std::io::Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            Ok(())
        })();
        cleanup_temporary_on_error(publish, &temporary)?;
        publish_identity(&temporary, path)?;
        sync_parent(path)?;
        Ok(())
    }

    #[must_use]
    /// Returns the stable project identifier.
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    #[must_use]
    /// Returns the Ed25519 verification key safe to embed in a runtime.
    pub const fn public_key(&self) -> [u8; 32] {
        self.public_key
    }

    #[must_use]
    /// Returns a copy of the project root key for controlled publisher use.
    pub fn root_key(&self) -> [u8; 32] {
        *self.root_key
    }

    /// Splits runtime decryption material into two XOR shares.
    ///
    /// # Errors
    ///
    /// Returns an error if the operating system CSPRNG fails.
    pub fn runtime_key_material(&self) -> Result<RuntimeKeyMaterial> {
        let mut key_share_a = [0_u8; 32];
        SystemRandom::new()
            .fill(&mut key_share_a)
            .map_err(|_| Error::Crypto("runtime key share generation"))?;
        let mut key_share_b = [0_u8; 32];
        for (output, (root, share)) in key_share_b
            .iter_mut()
            .zip(self.root_key.iter().zip(key_share_a))
        {
            *output = root ^ share;
        }
        Ok(RuntimeKeyMaterial {
            key_share_a,
            key_share_b,
            public_key: self.public_key,
        })
    }

    pub(crate) fn sign(&self, message: &[u8]) -> Result<[u8; 64]> {
        let pair = Ed25519KeyPair::from_pkcs8(&self.signing_pkcs8)
            .map_err(|_| Error::Identity("Ed25519 private key"))?;
        Ok(pair
            .sign(message)
            .as_ref()
            .try_into()
            .expect("Ed25519 signature size"))
    }
}

fn identity_payload_len(pkcs8_len: usize) -> Result<usize> {
    HEADER_SIZE
        .checked_add(pkcs8_len)
        .ok_or(Error::Identity("length overflow"))
}

fn publish_identity(temporary: &Path, path: &Path) -> Result<()> {
    // A hard link publishes the fully synchronized inode without the
    // check-then-rename overwrite race. Both paths are deliberately siblings.
    let result = std::fs::hard_link(temporary, path);
    let _ = std::fs::remove_file(temporary);
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(
            Error::InvalidInput(format!("identity already exists: {}", path.display())),
        ),
        Err(error) => Err(Error::Io(error)),
    }
}

fn cleanup_temporary_on_error(result: std::io::Result<()>, temporary: &Path) -> Result<()> {
    result.map_err(|error| {
        let _ = std::fs::remove_file(temporary);
        Error::Io(error)
    })
}

fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("hakutaku-identity-{}-{name}", std::process::id()))
    }

    fn rewrite_checksum(bytes: &mut [u8]) {
        let payload_len = bytes.len() - CHECKSUM_SIZE;
        let checksum = blake3::hash(&bytes[..payload_len]);
        bytes[payload_len..].copy_from_slice(checksum.as_bytes());
    }

    #[test]
    fn identity_loader_rejects_each_corruption_class() {
        let root = root("corruption");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("publisher.haki");
        let identity = Identity::generate().unwrap();
        identity.save(&path).unwrap();
        assert_eq!(
            Identity::load(&path).unwrap().project_id(),
            identity.project_id()
        );
        assert!(identity.save(&path).is_err());
        let valid = std::fs::read(&path).unwrap();

        for (name, mutate, fix_checksum) in [
            ("short", 0_usize, false),
            ("magic", 0, false),
            ("version", 8, false),
            ("length", 92, false),
            ("checksum", 28, false),
            ("private", HEADER_SIZE, true),
            ("public", 60, true),
        ] {
            let damaged_path = root.join(name);
            let mut damaged = if name == "short" {
                vec![0; HEADER_SIZE]
            } else {
                valid.clone()
            };
            damaged[mutate] ^= 1;
            if fix_checksum {
                rewrite_checksum(&mut damaged);
            }
            std::fs::write(&damaged_path, damaged).unwrap();
            assert!(Identity::load(damaged_path).is_err(), "{name}");
        }
        assert!(identity_payload_len(usize::MAX).is_err());
        sync_parent(Path::new("identity-without-parent")).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_maps_collisions_and_other_link_failures() {
        let root = root("publish");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let temporary = root.join("temporary");
        let target = root.join("target");
        std::fs::write(&temporary, b"identity").unwrap();
        std::fs::write(&target, b"winner").unwrap();
        assert!(matches!(
            publish_identity(&temporary, &target),
            Err(Error::InvalidInput(_))
        ));
        let temporary = root.join("temporary-2");
        std::fs::write(&temporary, b"identity").unwrap();
        assert!(matches!(
            publish_identity(&temporary, &root.join("missing/target")),
            Err(Error::Io(_))
        ));
        let temporary = root.join("temporary-3");
        std::fs::write(&temporary, b"identity").unwrap();
        assert!(
            cleanup_temporary_on_error(Err(std::io::ErrorKind::WriteZero.into()), &temporary)
                .is_err()
        );
        assert!(!temporary.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
