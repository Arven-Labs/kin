/// Erros do crate `crypto`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("identidade: {0}")]
    Identity(#[from] kin_identity::Error),
    /// A credencial MLS não traz um certificado válido, ou ele não cobre a chave do device.
    #[error("credencial inválida")]
    InvalidCredential,
    /// O certificado é válido, mas de uma identidade diferente da esperada.
    #[error("identidade diferente da esperada")]
    UnexpectedIdentity,
    /// O certificado é válido, mas o Peer ID de rede dele não é o esperado (P15).
    #[error("peer de rede diferente do esperado")]
    UnexpectedPeer,
    #[error("mensagem malformada")]
    Malformed,
    /// Mensagem já processada (entrega at-least-once): pode ser ignorada.
    #[error("mensagem duplicada")]
    Duplicate,
    /// Mensagem de uma época ou geração antiga demais para ainda ser decifrada.
    #[error("mensagem antiga demais")]
    TooOld,
    /// Mensagem de uma época que este device ainda não alcançou (falta aplicar um commit).
    #[error("época desconhecida (commit pendente)")]
    UnknownEpoch,
    /// Mensagem de outra conversa.
    #[error("mensagem de outra conversa")]
    WrongConversation,
    /// O Welcome não foi feito para nenhum KeyPackage deste device.
    #[error("welcome não é para este device")]
    NotForThisDevice,
    /// Só o committer da conversa emite commits (P9); os demais propõem.
    #[error("só o committer da conversa pode emitir commits")]
    NotCommitter,
    /// A conversa é 1:1 (no máximo 2 membros): um commit ou Welcome a levaria além disso. Quem
    /// entrasse leria o que o dono de um device envia pensando falar com um contato (grupos: Fase 5).
    #[error("a conversa não pode passar de 2 membros")]
    TooManyMembers,
    /// Mensagem MLS que o Kin ainda não trata (propostas soltas, etc.).
    #[error("tipo de mensagem não suportado")]
    Unsupported,
    /// O banco de estado já pertence a outra identidade.
    #[error("o estado salvo pertence a outra identidade")]
    IdentityMismatch,
    /// O banco foi criado com outra chave de rede; as conversas antigas não valem mais (P15).
    #[error("o estado salvo pertence a outra chave de rede do device")]
    DeviceMismatch,
    #[error("armazenamento: {0}")]
    Storage(String),
    #[error("falha no MLS: {0}")]
    Mls(String),
}

impl Error {
    pub(crate) fn storage(e: impl std::fmt::Display) -> Self {
        Self::Storage(e.to_string())
    }

    pub(crate) fn mls(e: impl std::fmt::Display) -> Self {
        Self::Mls(e.to_string())
    }
}
