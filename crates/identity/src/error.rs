/// Erros do crate `identity`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("falha ao assinar: {0}")]
    Signing(#[from] libp2p_identity::SigningError),
    #[error("chave inválida: {0}")]
    Decoding(#[from] libp2p_identity::DecodingError),
    #[error("a chave não é Ed25519")]
    NotEd25519,
    #[error("chave Nostr inválida (não é um ponto da curva secp256k1)")]
    InvalidNostrKey,
    #[error("arquivo de chave do device inválido ou de formato antigo")]
    InvalidDeviceKey,
    /// Sem detalhe de propósito: o link carrega a chave mestra e não pode ser ecoado.
    #[error("link de pareamento inválido (esperado kin://pair/ + chave mestra em base64url)")]
    InvalidPairLink,
    #[error("certificado de device inválido")]
    InvalidCertificate,
    #[error("erro de E/S: {0}")]
    Io(#[from] std::io::Error),
}
