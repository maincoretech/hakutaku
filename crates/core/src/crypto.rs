//! Fixed Hakutaku v1 cryptographic profile.
//!
//! AES-256-GCM is the only content cipher. There is deliberately no runtime
//! algorithm identifier or negotiation path.

use crate::format::{Codec, PageKind, ProjectId, SegmentHeader, SnapshotHeader};
use crate::{Error, Result};
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::signature::{self, UnparsedPublicKey};
use zeroize::{Zeroize, Zeroizing};

const MASTER_CONTEXT: &str = "Hakutaku project master v1";
const SNAPSHOT_SIGNATURE_DOMAIN: &[u8] = b"Hakutaku snapshot signature v1";
const CATALOG_AAD_DOMAIN: &[u8] = b"Hakutaku catalog aad v1";
const PAGE_AAD_DOMAIN: &[u8] = b"Hakutaku page aad v1";
const BLOCK_AAD_DOMAIN: &[u8] = b"Hakutaku block aad v1";
const CATALOG_AAD_LEN: usize = CATALOG_AAD_DOMAIN.len() + 16 + 8 + 8 + 8 + 4;
const PAGE_AAD_LEN: usize = PAGE_AAD_DOMAIN.len() + 16 + 8 + 1 + 1 + 4 + 4 + 4;
const BLOCK_AAD_LEN: usize = BLOCK_AAD_DOMAIN.len() + 16 + 16 + 4 + 1 + 4 + 4;

/// Domain-separated keys derived from one project's runtime root key.
pub struct ProjectKeys {
    master: Zeroizing<[u8; 32]>,
}

/// Prepared AES-256-GCM key. Construct once per active snapshot or segment.
pub struct Aes256Key(LessSafeKey);

impl Aes256Key {
    /// Prepares an AES-256-GCM key from exactly 256 bits of key material.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Authentication`] if the crypto provider rejects the key.
    pub fn new(key: &[u8; 32]) -> Result<Self> {
        Ok(Self(LessSafeKey::new(
            UnboundKey::new(&aead::AES_256_GCM, key)
                .map_err(|_| Error::Authentication("AES-256 key"))?,
        )))
    }

    /// Encrypts `plaintext` in place and appends its authentication tag.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Authentication`] if sealing fails.
    pub fn seal(&self, nonce: Nonce, aad: &[u8], plaintext: &mut Vec<u8>) -> Result<()> {
        self.0
            .seal_in_place_append_tag(nonce, Aad::from(aad), plaintext)
            .map_err(|_| Error::Authentication("AES-256-GCM seal"))
    }

    /// Authenticates and decrypts a ciphertext-plus-tag buffer in place.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Authentication`] with `scope` when verification fails.
    pub fn open(
        &self,
        nonce: Nonce,
        aad: &[u8],
        ciphertext_and_tag: &mut Vec<u8>,
        scope: &'static str,
    ) -> Result<()> {
        let plain_len = self
            .0
            .open_in_place(nonce, Aad::from(aad), ciphertext_and_tag)
            .map_err(|_| Error::Authentication(scope))?
            .len();
        ciphertext_and_tag.truncate(plain_len);
        Ok(())
    }
}

impl ProjectKeys {
    /// Derives the project master key and erases the supplied root-key copy.
    #[must_use]
    pub fn new(mut root_key: [u8; 32], project_id: ProjectId) -> Self {
        let mut material = Zeroizing::new([0_u8; 48]);
        material[..32].copy_from_slice(&root_key);
        material[32..].copy_from_slice(&project_id.0);
        let master = Zeroizing::new(blake3::derive_key(MASTER_CONTEXT, material.as_ref()));
        root_key.zeroize();
        Self { master }
    }

    /// Derives the key used to authenticate and decrypt one release snapshot.
    #[must_use]
    pub fn snapshot_key(&self, salt: &[u8; 16]) -> Zeroizing<[u8; 32]> {
        self.derive(b"snapshot", &[salt])
    }

    /// Derives the key used to authenticate and decrypt one immutable segment.
    #[must_use]
    pub fn segment_key(&self, header: &SegmentHeader) -> Zeroizing<[u8; 32]> {
        self.derive(b"segment", &[&header.segment_uid, &header.salt])
    }

    /// Derives the keyed-hash key for canonical asset paths.
    #[must_use]
    pub fn path_key(&self) -> [u8; 32] {
        *self.derive(b"path", &[])
    }

    fn derive(&self, domain: &[u8], fields: &[&[u8]]) -> Zeroizing<[u8; 32]> {
        let mut hasher = blake3::Hasher::new_keyed(&self.master);
        hasher.update(domain);
        for field in fields {
            hasher.update(&u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes());
            hasher.update(field);
        }
        Zeroizing::new(*hasher.finalize().as_bytes())
    }
}

/// Forms the format's 96-bit nonce from a random prefix and unique ordinal.
pub fn nonce(prefix: [u8; 8], ordinal: u32) -> Nonce {
    let mut bytes = [0_u8; 12];
    bytes[..8].copy_from_slice(&prefix);
    bytes[8..].copy_from_slice(&ordinal.to_le_bytes());
    Nonce::assume_unique_for_key(bytes)
}

#[must_use]
/// Computes the 128-bit identifier stored for an Ed25519 verification key.
pub fn signing_key_id(public_key: &[u8; 32]) -> [u8; 16] {
    let mut result = [0_u8; 16];
    result.copy_from_slice(&blake3::hash(public_key).as_bytes()[..16]);
    result
}

#[must_use]
/// Builds the domain-separated message signed by a package publisher.
pub fn snapshot_signature_message(zeroed_header: &[u8], catalog_ciphertext: &[u8]) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(zeroed_header);
    hasher.update(catalog_ciphertext);
    let digest = hasher.finalize();
    let mut message = Vec::with_capacity(SNAPSHOT_SIGNATURE_DOMAIN.len() + 32);
    message.extend_from_slice(SNAPSHOT_SIGNATURE_DOMAIN);
    message.extend_from_slice(digest.as_bytes());
    message
}

/// Verifies both the expected signing-key identifier and snapshot signature.
///
/// # Errors
///
/// Returns [`Error::Signature`] for a key mismatch or invalid signature.
pub fn verify_snapshot_signature(
    header: &SnapshotHeader,
    catalog_ciphertext: &[u8],
    public_key: &[u8; 32],
) -> Result<()> {
    if signing_key_id(public_key) != header.signing_key_id {
        return Err(Error::Signature);
    }
    let encoded = header.encode(true);
    let message = snapshot_signature_message(&encoded, catalog_ciphertext);
    UnparsedPublicKey::new(&signature::ED25519, public_key)
        .verify(&message, &header.signature)
        .map_err(|_| Error::Signature)
}

#[must_use]
/// Encodes authenticated metadata for one encrypted snapshot page.
pub fn page_aad(
    project_id: ProjectId,
    release_sequence: u64,
    kind: PageKind,
    codec: Codec,
    nonce_ordinal: u32,
    stored_len: u32,
    plain_len: u32,
) -> [u8; PAGE_AAD_LEN] {
    encode_aad([
        PAGE_AAD_DOMAIN,
        &project_id.0,
        &release_sequence.to_le_bytes(),
        &[kind as u8],
        &[codec as u8],
        &nonce_ordinal.to_le_bytes(),
        &stored_len.to_le_bytes(),
        &plain_len.to_le_bytes(),
    ])
}

#[must_use]
/// Encodes authenticated metadata for one encrypted segment block.
pub fn block_aad(
    project_id: ProjectId,
    segment_uid: &[u8; 16],
    block_ordinal: u32,
    codec: Codec,
    stored_len: u32,
    plain_len: u32,
) -> [u8; BLOCK_AAD_LEN] {
    encode_aad([
        BLOCK_AAD_DOMAIN,
        &project_id.0,
        segment_uid,
        &block_ordinal.to_le_bytes(),
        &[codec as u8],
        &stored_len.to_le_bytes(),
        &plain_len.to_le_bytes(),
    ])
}

#[must_use]
/// Encodes authenticated metadata for the encrypted catalog.
pub fn catalog_aad(header: &SnapshotHeader) -> [u8; CATALOG_AAD_LEN] {
    encode_aad([
        CATALOG_AAD_DOMAIN,
        &header.project_id.0,
        &header.release_sequence.to_le_bytes(),
        &header.catalog_stored_len.to_le_bytes(),
        &header.catalog_plain_len.to_le_bytes(),
        &header.page_count.to_le_bytes(),
    ])
}

fn encode_aad<const N: usize, const P: usize>(parts: [&[u8]; P]) -> [u8; N] {
    let mut encoded = [0_u8; N];
    let mut offset = 0;
    for part in parts {
        let end = offset + part.len();
        encoded[offset..end].copy_from_slice(part);
        offset = end;
    }
    debug_assert_eq!(offset, N);
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::SIGNATURE_LEN;

    fn header() -> SnapshotHeader {
        SnapshotHeader {
            project_id: ProjectId([1; 16]),
            release_sequence: 2,
            catalog_stored_len: 16,
            catalog_plain_len: 1,
            page_region_offset: crate::format::SNAPSHOT_HEADER_SIZE as u64 + 16,
            page_count: 0,
            snapshot_salt: [3; 16],
            nonce_prefix: [4; 8],
            signing_key_id: [5; 16],
            source_fingerprint: [6; 32],
            signature: [0; SIGNATURE_LEN],
        }
    }

    #[test]
    fn signature_key_identifier_mismatch_is_rejected_before_verification() {
        assert!(matches!(
            verify_snapshot_signature(&header(), &[], &[7; 32]),
            Err(Error::Signature)
        ));
    }

    #[test]
    fn authenticated_metadata_has_canonical_fixed_width_encoding() {
        let header = header();
        let catalog = catalog_aad(&header);
        let page = page_aad(
            header.project_id,
            2,
            PageKind::BlockMap,
            Codec::Zstd,
            3,
            5,
            7,
        );
        let block = block_aad(header.project_id, &[8; 16], 9, Codec::Raw, 11, 13);

        assert_eq!(catalog.len(), CATALOG_AAD_LEN);
        assert!(catalog.starts_with(CATALOG_AAD_DOMAIN));
        assert_eq!(
            &catalog[catalog.len() - 4..],
            &header.page_count.to_le_bytes()
        );
        assert_eq!(page.len(), PAGE_AAD_LEN);
        assert!(page.starts_with(PAGE_AAD_DOMAIN));
        assert_eq!(&page[page.len() - 4..], &7_u32.to_le_bytes());
        assert_eq!(block.len(), BLOCK_AAD_LEN);
        assert!(block.starts_with(BLOCK_AAD_DOMAIN));
        assert_eq!(&block[block.len() - 4..], &13_u32.to_le_bytes());
    }
}
