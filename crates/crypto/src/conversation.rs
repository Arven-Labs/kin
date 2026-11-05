use kin_identity::{IdentityId, PeerId};
use openmls::prelude::tls_codec::Deserialize as _;
use openmls::prelude::{
    GroupId, KeyPackageIn, MlsGroup, MlsGroupCreateConfig, MlsMessageBodyIn, MlsMessageIn,
    ProcessedMessageContent, Proposal, ProtocolVersion, SenderRatchetConfiguration, StagedWelcome,
};

use openmls_traits::OpenMlsProvider as _;

use crate::device::CIPHERSUITE;
use crate::{CryptoDevice, Error, credential};

/// Quantas gerações atrás (e à frente) o MLS tolera numa mesma época: cobre a entrega fora de ordem.
const OUT_OF_ORDER_TOLERANCE: u32 = 10;
const MAX_FORWARD_DISTANCE: u32 = 2000;
/// Épocas passadas mantidas para decifrar mensagens que chegam depois de um commit.
const MAX_PAST_EPOCHS: usize = 3;

fn config() -> MlsGroupCreateConfig {
    MlsGroupCreateConfig::builder()
        .ciphersuite(CIPHERSUITE)
        .use_ratchet_tree_extension(true)
        .sender_ratchet_configuration(SenderRatchetConfiguration::new(
            OUT_OF_ORDER_TOLERANCE,
            MAX_FORWARD_DISTANCE,
        ))
        .max_past_epochs(MAX_PAST_EPOCHS)
        .build()
}

/// O que o chamador já sabe sobre o outro lado; cada campo informado é exigido (o que fica `None`
/// não é conferido). O Peer ID vem da conexão de rede e amarra a credencial MLS a ela (P15).
#[derive(Debug, Clone, Copy, Default)]
pub struct Expected<'a> {
    pub identity: Option<&'a IdentityId>,
    pub peer: Option<&'a PeerId>,
}

/// Resultado de [`Conversation::invite`].
#[derive(Debug, Clone)]
pub struct Invite {
    /// Commit que leva os membros atuais à nova época (em 1:1, o convidante está sozinho: ninguém
    /// precisa dele, mas em grupos maiores os outros membros precisam).
    pub commit: Vec<u8>,
    /// Welcome para o convidado entrar na conversa.
    pub welcome: Vec<u8>,
}

/// O que uma mensagem recebida produziu.
// `EpochChanged` é raro e a diferença de tamanho é só a `IdentityId`; encaixotar não compensa.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decrypted {
    /// Mensagem de aplicação decifrada.
    Message {
        sender: IdentityId,
        plaintext: Vec<u8>,
    },
    /// Era um commit; a conversa avançou de época.
    EpochChanged,
    /// Era uma proposta de outro membro; fica guardada até o committer emitir o commit (P9).
    ProposalQueued,
}

/// Conversa E2EE (grupo MLS). Só um dos lados deve emitir commits por vez (P9).
pub struct Conversation {
    device: CryptoDevice,
    group: MlsGroup,
}

impl Conversation {
    /// Cria uma conversa nova, só com este device.
    pub fn create(device: &CryptoDevice) -> Result<Self, Error> {
        let inner = &device.0;
        let group = MlsGroup::new(
            &inner.provider,
            &inner.signer,
            &config(),
            inner.credential.clone(),
        )
        .map_err(Error::mls)?;
        Ok(Self {
            device: device.clone(),
            group,
        })
    }

    /// Recarrega do banco uma conversa já existente; `None` se não houver com esse id.
    pub fn load(device: &CryptoDevice, id: &[u8]) -> Result<Option<Self>, Error> {
        let group = MlsGroup::load(device.0.provider.storage(), &GroupId::from_slice(id))
            .map_err(Error::mls)?;
        Ok(group.map(|group| Self {
            device: device.clone(),
            group,
        }))
    }

    /// Entra numa conversa a partir de um Welcome. Confere a credencial de todos os membros;
    /// `inviter` exige que algum outro membro do grupo satisfaça identidade e Peer ID informados.
    pub fn join(device: &CryptoDevice, welcome: &[u8], inviter: &Expected) -> Result<Self, Error> {
        let inner = &device.0;
        let MlsMessageBodyIn::Welcome(welcome) = MlsMessageIn::tls_deserialize_exact(welcome)
            .map_err(|_| Error::Malformed)?
            .extract()
        else {
            return Err(Error::Malformed);
        };
        // Confere todos os membros com o Welcome ainda só "staged": se algo não bater, o grupo não
        // chega a ser gravado no banco (o KeyPackage, porém, já foi consumido pelo openmls).
        let staged =
            StagedWelcome::new_from_welcome(&inner.provider, config().join_config(), welcome, None)
                .map_err(|_| Error::NotForThisDevice)?;

        let own = staged.own_leaf_index();
        let (mut found_identity, mut found_both) = (
            inviter.identity.is_none(),
            inviter.peer.is_none() && inviter.identity.is_none(),
        );
        for member in staged.members() {
            let who = credential::verify(
                &member.credential,
                &member.signature_key,
                &Expected::default(),
            )?;
            if member.index == own {
                continue;
            }
            found_identity |= inviter.identity.is_some_and(|id| id == &who.identity);
            found_both |= inviter.accepts(&who);
        }
        if !found_identity {
            return Err(Error::UnexpectedIdentity);
        }
        if !found_both {
            return Err(Error::UnexpectedPeer);
        }
        let group = staged.into_group(&inner.provider).map_err(Error::mls)?;
        Ok(Self {
            device: device.clone(),
            group,
        })
    }

    /// Identificador da conversa (igual nos dois lados).
    pub fn id(&self) -> Vec<u8> {
        self.group.group_id().as_slice().to_vec()
    }

    /// Época atual do grupo; sobe a cada commit.
    pub fn epoch(&self) -> u64 {
        self.group.epoch().as_u64()
    }

    /// Identidades dos outros membros.
    pub fn peers(&self) -> Result<Vec<IdentityId>, Error> {
        let own = self.group.own_leaf_index();
        self.group
            .members()
            .filter(|m| m.index != own)
            .map(|m| credential::identity_of(&m.credential))
            .collect()
    }

    /// Peer IDs de rede certificados dos outros membros.
    pub fn peer_ids(&self) -> Result<Vec<PeerId>, Error> {
        let own = self.group.own_leaf_index();
        self.group
            .members()
            .filter(|m| m.index != own)
            .map(|m| credential::peer_of(&m.credential))
            .collect()
    }

    /// Convida o dono do KeyPackage. Valida o KeyPackage e a credencial dele antes de aceitar;
    /// `expected` exige identidade e/ou Peer ID de rede específicos.
    pub fn invite(&mut self, key_package: &[u8], expected: &Expected) -> Result<Invite, Error> {
        let inner = &self.device.0;
        let key_package = KeyPackageIn::tls_deserialize_exact(key_package)
            .map_err(|_| Error::Malformed)?
            .validate(inner.provider.crypto(), ProtocolVersion::Mls10)
            .map_err(|_| Error::Malformed)?;
        credential::verify(
            key_package.leaf_node().credential(),
            key_package.leaf_node().signature_key().as_slice(),
            expected,
        )?;

        let (commit, welcome, _) = self
            .group
            .add_members(
                &inner.provider,
                &inner.signer,
                core::slice::from_ref(&key_package),
            )
            .map_err(Error::mls)?;
        self.group
            .merge_pending_commit(&inner.provider)
            .map_err(Error::mls)?;
        Ok(Invite {
            commit: commit.to_bytes().map_err(Error::mls)?,
            welcome: welcome.to_bytes().map_err(Error::mls)?,
        })
    }

    /// Este device é o committer da conversa (P9)? Só ele emite commits.
    ///
    /// O committer é o membro de menor índice de folha: em 1:1, quem criou a conversa. A regra é
    /// determinística e igual em todos os devices, então ninguém precisa combinar nada pela rede.
    pub fn is_committer(&self) -> bool {
        let own = self.group.own_leaf_index();
        self.group.members().all(|m| m.index >= own)
    }

    /// Propõe renovar a chave deste membro (Update proposal). Qualquer membro pode; quem efetiva é o
    /// committer, com [`Self::commit_pending`].
    pub fn propose_rotation(&mut self) -> Result<Vec<u8>, Error> {
        let inner = &self.device.0;
        let (proposal, _) = self
            .group
            .propose_self_update(
                &inner.provider,
                &inner.signer,
                openmls::prelude::LeafNodeParameters::default(),
            )
            .map_err(Error::mls)?;
        proposal.to_bytes().map_err(Error::mls)
    }

    /// Efetiva as propostas guardadas num commit para os outros membros, que o aplicam com
    /// [`Self::decrypt`]. Só o committer (P9): sem isso, dois lados poderiam commitar da mesma época.
    pub fn commit_pending(&mut self) -> Result<Vec<u8>, Error> {
        if !self.is_committer() {
            return Err(Error::NotCommitter);
        }
        let inner = &self.device.0;
        let (commit, _welcome, _) = self
            .group
            .commit_to_pending_proposals(&inner.provider, &inner.signer)
            .map_err(Error::mls)?;
        self.group
            .merge_pending_commit(&inner.provider)
            .map_err(Error::mls)?;
        commit.to_bytes().map_err(Error::mls)
    }

    /// Renova as chaves do grupo (post-compromise security). Devolve o commit para os outros
    /// membros, que o aplicam com [`Self::decrypt`]. Só o committer (P9); os demais usam
    /// [`Self::propose_rotation`].
    pub fn rotate_keys(&mut self) -> Result<Vec<u8>, Error> {
        if !self.is_committer() {
            return Err(Error::NotCommitter);
        }
        let inner = &self.device.0;
        let bundle = self
            .group
            .self_update(
                &inner.provider,
                &inner.signer,
                openmls::prelude::LeafNodeParameters::default(),
            )
            .map_err(Error::mls)?;
        self.group
            .merge_pending_commit(&inner.provider)
            .map_err(Error::mls)?;
        bundle.into_commit().to_bytes().map_err(Error::mls)
    }

    /// Cifra `plaintext` para os outros membros.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        let inner = &self.device.0;
        self.group
            .create_message(&inner.provider, &inner.signer, plaintext)
            .map_err(Error::mls)?
            .to_bytes()
            .map_err(Error::mls)
    }

    /// Processa uma mensagem recebida (aplicação ou commit). Idempotente: reentregas viram
    /// [`Error::Duplicate`], sem alterar o estado.
    pub fn decrypt(&mut self, bytes: &[u8]) -> Result<Decrypted, Error> {
        let inner = &self.device.0;
        let message = MlsMessageIn::tls_deserialize_exact(bytes)
            .map_err(|_| Error::Malformed)?
            .try_into_protocol_message()
            .map_err(|_| Error::Malformed)?;
        let processed = self
            .group
            .process_message(&inner.provider, message)
            .map_err(map_process_error)?;

        let sender = credential::identity_of(processed.credential())?;
        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(m) => Ok(Decrypted::Message {
                sender,
                plaintext: m.into_bytes(),
            }),
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                for add in staged.add_proposals() {
                    let leaf = add.add_proposal().key_package().leaf_node();
                    credential::verify(
                        leaf.credential(),
                        leaf.signature_key().as_slice(),
                        &Expected::default(),
                    )?;
                }
                self.group
                    .merge_staged_commit(&inner.provider, *staged)
                    .map_err(Error::mls)?;
                Ok(Decrypted::EpochChanged)
            }
            // Só Update: Add/Remove soltos de um peer não podem entrar na fila que o committer
            // efetiva depois, ou o peer escolheria quem entra e quem sai da conversa.
            ProcessedMessageContent::ProposalMessage(proposal)
                if matches!(proposal.proposal(), Proposal::Update(_)) =>
            {
                self.group
                    .store_pending_proposal(inner.provider.storage(), *proposal)
                    .map_err(Error::mls)?;
                Ok(Decrypted::ProposalQueued)
            }
            _ => Err(Error::Unsupported),
        }
    }
}

fn map_process_error<S: std::error::Error>(e: openmls::prelude::ProcessMessageError<S>) -> Error {
    use openmls::prelude::ProcessMessageError as P;
    use openmls::prelude::ValidationError as V;
    match &e {
        P::ValidationError(V::UnableToDecrypt(inner)) => {
            let text = inner.to_string();
            if text.contains("deleted to preserve forward secrecy") {
                Error::Duplicate
            } else if text.contains("too old") {
                Error::TooOld
            } else {
                Error::mls(e)
            }
        }
        P::ValidationError(V::NoPastEpochData) => Error::TooOld,
        P::ValidationError(V::WrongGroupId) => Error::WrongConversation,
        P::ValidationError(V::WrongEpoch) => Error::UnknownEpoch,
        _ => Error::mls(e),
    }
}
