use std::path::Path;

use libp2p_identity::{Keypair, PublicKey};

use crate::{DeviceCertificate, Error, IdentityId, IdentityProvider, NostrPublicKey, store};

/// Provedor padrão (D7): keypair Ed25519 gerado pelo Kin, sem conta e sem blockchain.
pub struct StandaloneIdentity {
    master: Keypair,
}

impl StandaloneIdentity {
    pub fn generate() -> Self {
        Self {
            master: Keypair::generate_ed25519(),
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let master = Keypair::from_protobuf_encoding(bytes)?;
        master
            .clone()
            .try_into_ed25519()
            .map_err(|_| Error::NotEd25519)?;
        Ok(Self { master })
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        Ok(self.master.to_protobuf_encoding()?)
    }

    /// Grava a chave mestra em `path` (permissão 0600 em Unix). Não cifra em repouso (P13).
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        store::write_secret(path, &self.to_bytes()?)
    }

    pub fn load(path: &Path) -> Result<Self, Error> {
        Self::from_bytes(&store::read_secret(path)?)
    }
}

impl IdentityProvider for StandaloneIdentity {
    fn id(&self) -> IdentityId {
        IdentityId::new(self.master.public())
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self.master.sign(message)?)
    }

    fn authorize_device(
        &self,
        signing_key: &PublicKey,
        network_key: &PublicKey,
        nostr_key: &NostrPublicKey,
        created_at: u64,
    ) -> Result<DeviceCertificate, Error> {
        let payload =
            DeviceCertificate::signing_payload(signing_key, network_key, nostr_key, created_at);
        Ok(DeviceCertificate {
            identity: self.id(),
            signing_key: signing_key.clone(),
            network_key: network_key.clone(),
            nostr_key: *nostr_key,
            created_at,
            signature: self.master.sign(&payload)?,
        })
    }
}
