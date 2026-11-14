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
    /// Sem detalhe de propósito: não dizer qual das três provas de posse falhou.
    #[error("pedido de pareamento inválido")]
    InvalidPairRequest,
    /// O certificado devolvido não é da identidade esperada ou não cobre as chaves do pedido.
    #[error("o certificado não é o esperado para este pedido de pareamento")]
    PairCertificateMismatch,
    #[error("certificado de autoridade inválido")]
    InvalidAuthorityCertificate,
    #[error("arquivo de chave de autoridade inválido")]
    InvalidAuthorityKey,
    #[error("a chave de autoridade não é a do certificado de autoridade")]
    AuthorityKeyMismatch,
    /// A mestra certificada como a própria autoridade (P30 2b): ela ficaria quente no primário.
    #[error("a chave de autoridade é a própria mestra: a mestra deve ficar fria, fora do primário")]
    AuthorityIsMaster,
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
