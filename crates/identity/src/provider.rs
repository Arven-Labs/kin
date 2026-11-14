use libp2p_identity::{PeerId, PublicKey};

use crate::{DeviceCertificate, Error, NostrPublicKey};

/// ID estável de uma identidade: a chave pública mestra.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityId(PublicKey);

impl IdentityId {
    pub fn new(key: PublicKey) -> Self {
        Self(key)
    }

    pub fn public_key(&self) -> &PublicKey {
        &self.0
    }

    /// Representação textual estável (a mesma do Peer ID derivado da chave mestra).
    pub fn to_base58(&self) -> String {
        self.0.to_peer_id().to_base58()
    }

    pub fn as_peer_id(&self) -> PeerId {
        self.0.to_peer_id()
    }
}

/// O que o Kin exige de uma identidade, seja qual for a origem (D7).
pub trait IdentityProvider {
    /// ID estável da identidade.
    fn id(&self) -> IdentityId;

    // Sem `sign` livre (auditoria da fatia 1b, S1, ADR 0013): a chave da identidade só assina por
    // métodos com domínio, como este, para ninguém obter dela uma assinatura sobre bytes escolhidos.

    /// Autoriza um device ("este device pertence a esta identidade"), certificando a chave de
    /// assinatura MLS, a chave de rede e a chave Nostr juntas.
    /// `created_at` é o instante em segundos Unix, passado de fora para manter o código testável.
    fn authorize_device(
        &self,
        signing_key: &PublicKey,
        network_key: &PublicKey,
        nostr_key: &NostrPublicKey,
        created_at: u64,
    ) -> Result<DeviceCertificate, Error>;
}
