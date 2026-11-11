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
    /// Lista de devices anunciada por um device que tem um item de outra identidade (3.5).
    #[error("lista de devices recusada: {0}")]
    InvalidDeviceList(String),
    /// Um espelho "enviei isto" (`Payload::Sent`) veio de um device que não é da minha identidade.
    #[error("espelho recusado: o remetente não é um device desta identidade")]
    UnauthorizedMirror,
    /// Um pedido ou uma página de histórico (`HistoryRequest`/`HistoryChunk`) veio de um device que não é
    /// da minha identidade, ou uma página chegou sem eu ter pedido.
    #[error("histórico recusado: não é de um device desta identidade, ou não foi pedido")]
    UnauthorizedHistory,
    /// A sincronização de histórico com um irmão passou do teto de linhas por sessão ou não avança (o
    /// irmão devolve o mesmo cursor sem terminar): parada para não encher o disco nem girar sem fim.
    #[error("sincronização de histórico interrompida: {0}")]
    HistoryLimit(&'static str),
    /// Link de convite ilegível, adulterado (assinatura inválida) ou de versão desconhecida; ou um
    /// convite para si mesmo.
    #[error("convite inválido")]
    InvalidInvite,
    /// `Chat::untrust` da minha própria identidade: os meus devices são família, não contatos.
    #[error("não dá para desconfiar da própria identidade")]
    OwnIdentity,
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
