use kin_transport::PeerId;

/// Erros do crate `chat`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("criptografia: {0}")]
    Crypto(#[from] kin_crypto::Error),
    #[error("transporte: {0}")]
    Transport(#[from] kin_transport::Error),
    /// Ainda não há conversa E2EE com o peer (o handshake não terminou ou ele não está conectado).
    #[error("sem conversa com o peer {0}")]
    NoConversation(PeerId),
    /// Nenhum device conhecido dessa identidade tem conversa pronta (`Chat::send_to`).
    #[error("sem conversa com a identidade {0}")]
    UnknownIdentity(String),
    /// Um espelho "enviei isto" (`Payload::Sent`) veio de um device que não é da minha identidade.
    #[error("espelho recusado: o remetente não é um device desta identidade")]
    UnauthorizedMirror,
    /// Link de convite ilegível, adulterado (assinatura inválida) ou de versão desconhecida; ou um
    /// convite para si mesmo.
    #[error("convite inválido")]
    InvalidInvite,
    /// Bytes que não seguem o formato do Kin.
    #[error("mensagem malformada")]
    Malformed,
    /// Lista de relays anunciada por um contato que não passa na validação.
    #[error("lista de relays recusada: {0}")]
    InvalidRelayList(String),
    /// Falha no banco da outbox.
    #[error("armazenamento: {0}")]
    Storage(String),
}

impl Error {
    pub(crate) fn storage(e: impl std::fmt::Display) -> Self {
        Self::Storage(e.to_string())
    }
}
