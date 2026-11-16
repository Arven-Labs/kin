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
    /// Um `Update` trocou a chave de assinatura ou a chave Nostr do membro: o leaf novo tem de ser o mesmo
    /// device, com as mesmas 4 chaves (P30 3d-ii-a, ADR 0028).
    #[error("o Update trocou as chaves do device")]
    DeviceKeysChanged,
    /// O certificado do membro tem tombstone gravado (ou o store não pôde ser lido: falha fechada), P30 3d-i.
    #[error("o device deste certificado foi revogado")]
    Revoked,
    /// O banco já tem certificado (de um pareamento ou de uma mestra): não é um device pendente (P30 2c-ii).
    #[error("este banco já tem certificado: não está pendente de pareamento")]
    AlreadyPaired,
    /// Não há pedido de pareamento guardado neste banco para instalar um certificado.
    #[error("não há pedido de pareamento pendente neste banco")]
    NotPending,
    /// O pedido de pareamento passou do prazo (P30 2d-ii): descarte-o e peça de novo.
    #[error("o pedido de pareamento expirou: peça de novo")]
    PairRequestExpired,
    /// O relógio do sistema está antes de 1970: sem hora confiável não há prazo (P30 2d-ii).
    #[error("o relógio do sistema está antes de 1970")]
    Clock,
    /// O banco não tem um device pareado (certificado instalado) para reabrir.
    #[error("este banco não tem um device pareado")]
    NotPaired,
    /// O banco tem um pedido de pareamento pendente: abri-lo como mestra ou primário criaria outra chave
    /// MLS por cima da que o pedido já declarou.
    #[error("este banco está à espera de um certificado de pareamento")]
    PairingInProgress,
    /// Um device pareado tem de ter certificado v4 (de uma autoridade); este foi assinado pela própria
    /// mestra (v3), o que deixaria a mestra num device secundário (P30 3e-i, ADR 0011 item 4).
    #[error(
        "o certificado deste banco foi assinado pela mestra (v3), não por uma autoridade: pareie de novo, em um --data-dir novo"
    )]
    NotAuthorityIssued,
    /// O certificado é v3 (da mestra), mas esta identidade já foi aceita em regime v4: um device v3 dela
    /// só pode ser de quem tem a mestra, e o regime novo não volta atrás (P30 3e-ii-a, ADR 0031).
    #[error(
        "esta identidade já usa certificados de autoridade (v4): um certificado da mestra (v3) é recusado"
    )]
    Downgrade,
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
