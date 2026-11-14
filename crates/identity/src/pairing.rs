//! Pedido de pareamento (P30 fatia 2c, ADR 0016): o device novo mostra as três chaves públicas e prova
//! que tem as três privadas; o primário confere e responde com o certificado v4.

use libp2p_identity::PublicKey;

use crate::{DeviceKey, Error, IdentityId, NostrPublicKey};

/// Tamanho do nonce do pedido, em bytes.
pub const PAIR_NONCE_LEN: usize = 16;

/// Pedido autoassinado de um device novo. Cada chave assina (com domínio próprio) o conjunto das três
/// chaves e o nonce: prova de posse, sem a qual alguém poderia pôr a chave de outro no pedido.
// Rascunho (stubs da fatia 2c): o corpo vem depois dos testes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairRequest {
    /// Chave de assinatura MLS do device.
    pub signing_key: PublicKey,
    /// Chave de rede do device.
    pub network_key: PublicKey,
    /// Chave Nostr do device.
    pub nostr_key: NostrPublicKey,
    pub nonce: [u8; PAIR_NONCE_LEN],
    /// Assinatura da chave MLS sobre o conjunto.
    pub signing_proof: Vec<u8>,
    /// Assinatura da chave de rede sobre o conjunto.
    pub network_proof: Vec<u8>,
    /// Assinatura Schnorr da chave Nostr sobre o conjunto.
    pub nostr_proof: Vec<u8>,
}

impl PairRequest {
    /// Monta o pedido de `device` (rede e Nostr) com a chave MLS `signing_key`, cuja assinatura vem de
    /// `sign_mls` (a chave MLS é do openmls e não passa por aqui). `sign_mls` recebe os bytes com domínio.
    pub fn new(
        _device: &DeviceKey,
        _signing_key: &PublicKey,
        _nonce: [u8; PAIR_NONCE_LEN],
        _sign_mls: impl FnOnce(&[u8]) -> Result<Vec<u8>, Error>,
    ) -> Result<Self, Error> {
        todo!("2c: assinar o conjunto com as três chaves")
    }

    /// Confere as três provas de posse.
    pub fn verify(&self) -> Result<(), Error> {
        todo!("2c: conferir as três assinaturas")
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        todo!("2c: serializar")
    }

    /// Lê um pedido serializado. Não confere as provas: use [`Self::verify`].
    pub fn from_bytes(_bytes: &[u8]) -> Result<Self, Error> {
        todo!("2c: ler, estrito")
    }

    /// Código de comparação que as duas telas mostram: 12 dígitos em três grupos (`1234-5678-9012`),
    /// derivados do pedido e da identidade para a qual o primário vai certificar.
    pub fn sas_code(&self, _identity: &IdentityId) -> String {
        todo!("2c: SHA-256 truncado")
    }
}
