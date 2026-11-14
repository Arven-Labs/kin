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

    /// Inverso de [`Self::to_base58`]: lê o texto que o primário imprime (P30 2d-ii-a2). Só aceita o
    /// Peer ID de uma chave Ed25519 que carrega a própria chave (multihash identidade); qualquer outra
    /// coisa é [`Error::InvalidIdentityId`], sem detalhe do que veio.
    pub fn from_base58(text: &str) -> Result<Self, Error> {
        // Um Peer ID Ed25519 em base58 tem ~52 caracteres; o teto barra entrada gigante antes do parse.
        let text = text.trim();
        if text.is_empty() || text.len() > 128 {
            return Err(Error::InvalidIdentityId);
        }
        let peer_id: PeerId = text.parse().map_err(|_| Error::InvalidIdentityId)?;
        // Multihash identidade: código 0x00, tamanho, chave protobuf. Qualquer outro formato não carrega
        // a chave (é só um hash), então não dá para voltar à identidade.
        let bytes = peer_id.to_bytes();
        let [0x00, len, key @ ..] = bytes.as_slice() else {
            return Err(Error::InvalidIdentityId);
        };
        if usize::from(*len) != key.len() {
            return Err(Error::InvalidIdentityId);
        }
        let key = PublicKey::try_decode_protobuf(key).map_err(|_| Error::InvalidIdentityId)?;
        if key.clone().try_into_ed25519().is_err() {
            return Err(Error::InvalidIdentityId);
        }
        // Confere o caminho de volta: o texto lido é exatamente o Peer ID desta chave.
        let id = Self(key);
        if id.as_peer_id() != peer_id {
            return Err(Error::InvalidIdentityId);
        }
        Ok(id)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StandaloneIdentity;

    #[test]
    fn base58_round_trips_the_identity() {
        let id = StandaloneIdentity::generate().id();
        assert_eq!(IdentityId::from_base58(&id.to_base58()).unwrap(), id);
    }

    #[test]
    fn base58_trims_surrounding_whitespace_and_newline() {
        let id = StandaloneIdentity::generate().id();
        let padded = format!("  {}\n", id.to_base58());
        assert_eq!(IdentityId::from_base58(&padded).unwrap(), id);
    }

    #[test]
    fn base58_rejects_garbage_empty_and_oversized_input() {
        for bad in ["", "   ", "não é base58", "0OIl", &"1".repeat(4096)] {
            assert!(
                matches!(IdentityId::from_base58(bad), Err(Error::InvalidIdentityId)),
                "aceitou {bad:?}"
            );
        }
    }

    #[test]
    fn base58_rejects_a_peer_id_that_is_not_an_ed25519_identity_multihash() {
        // Peer ID aleatório: base58 válido de PeerId, mas não carrega uma chave Ed25519.
        let not_a_key = PeerId::random().to_base58();
        assert!(matches!(
            IdentityId::from_base58(&not_a_key),
            Err(Error::InvalidIdentityId)
        ));
    }
}
