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
    /// Senha errada e arquivo adulterado são **o mesmo erro** de propósito: distinguir os dois seria um oráculo.
    #[error("senha errada ou arquivo de chave corrompido")]
    WrongPassphraseOrCorrupt,
    /// Cabeçalho ilegível: magic, versão, parâmetros do KDF ou tamanho. Sem detalhe do conteúdo.
    #[error("arquivo de chave cifrado inválido")]
    InvalidSealedFile,
    #[error("o arquivo de chave está cifrado: informe a senha")]
    PassphraseRequired,
    #[error("a senha não pode ser vazia")]
    EmptyPassphrase,
    #[error("o arquivo de chave já está cifrado")]
    AlreadySealed,
    /// Com senha informada, um arquivo em claro é anômalo (troca de arquivo por quem tem escrita no
    /// diretório): recusa em vez de abrir a identidade de outro.
    #[error("o arquivo de chave está em claro, mas uma senha foi informada: arquivo trocado?")]
    PlainFileWhereSealedExpected,
    #[error("arquivo de chave grande demais")]
    KeyFileTooLarge,
    #[error("erro de E/S: {0}")]
    Io(#[from] std::io::Error),
}
