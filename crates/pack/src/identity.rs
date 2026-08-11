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

/// Publisher-only identity. Never ship this file with a game.
pub struct Identity {
    project_id: ProjectId,
    root_key: Zeroizing<[u8; 32]>,
    public_key: [u8; 32],
    signing_pkcs8: Zeroizing<Vec<u8>>,
}

impl Identity {
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
        let payload_len = HEADER_SIZE
            .checked_add(pkcs8_len)
            .ok_or(Error::Identity("length overflow"))?;
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

        let temporary = path.with_extension(format!("part-{}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        if let Err(error) = (|| -> std::io::Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, path)
        })() {
            let _ = std::fs::remove_file(&temporary);
            return Err(Error::Io(error));
        }
        Ok(())
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    #[must_use]
    pub const fn public_key(&self) -> [u8; 32] {
        self.public_key
    }

    #[must_use]
    pub fn root_key(&self) -> [u8; 32] {
        *self.root_key
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
