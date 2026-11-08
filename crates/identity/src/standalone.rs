use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use libp2p_identity::{Keypair, PublicKey};

use crate::{DeviceCertificate, Error, IdentityId, IdentityProvider, NostrPublicKey, store};

const PAIR_PREFIX: &str = "kin://pair/";

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

    /// A chave mestra como link de pareamento (`kin://pair/` + base64url sem padding), para levar a
    /// identidade a outro device (3.5, ADR 0006). **É o segredo inteiro**: quem vir o link é esta
    /// identidade. Não há cifra de transporte; quem chama avisa o usuário e não registra o link em log.
    pub fn to_pair_link(&self) -> Result<String, Error> {
        Ok(format!(
            "{PAIR_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(self.to_bytes()?)
        ))
    }

    /// Lê um link de pareamento. Aceita espaço e quebra de linha nas pontas (vem de arquivo ou stdin).
    /// Qualquer defeito vira [`Error::InvalidPairLink`], sem ecoar o conteúdo.
    pub fn from_pair_link(link: &str) -> Result<Self, Error> {
        let body = link
            .trim()
            .strip_prefix(PAIR_PREFIX)
            .ok_or(Error::InvalidPairLink)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| Error::InvalidPairLink)?;
        Self::from_bytes(&bytes).map_err(|_| Error::InvalidPairLink)
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
