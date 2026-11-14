use std::rc::Rc;

use kin_identity::{
    DeviceCertificate, IdentityId, IdentityProvider, NostrPublicKey, PublicKey,
    public_key_from_ed25519,
};
use openmls::prelude::tls_codec::Serialize as _;
use openmls::prelude::{
    BasicCredential, Ciphersuite, CredentialWithKey, KeyPackage, SignatureScheme,
};
use openmls_basic_credential::SignatureKeyPair;
use openmls_traits::OpenMlsProvider as _;

use crate::Error;
use crate::provider::Provider;

pub(crate) const CIPHERSUITE: Ciphersuite =
    Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

pub(crate) struct Inner {
    pub(crate) provider: Provider,
    pub(crate) signer: SignatureKeyPair,
    pub(crate) credential: CredentialWithKey,
    pub(crate) certificate: DeviceCertificate,
    pub(crate) identity: IdentityId,
}

/// Material criptográfico de um device: chave de assinatura MLS própria, certificada pela
/// identidade, e o estado (SQLite) de todas as suas conversas. Clonar é barato. Não é `Send`: a
/// conexão SQLite não é `Sync`, então o device vive numa thread só (ex.: `LocalSet`).
#[derive(Clone)]
pub struct CryptoDevice(pub(crate) Rc<Inner>);

impl CryptoDevice {
    /// Device só em memória: gera a chave MLS e a certifica com `identity`, junto da chave de rede
    /// `network_key` e da chave Nostr `nostr_key` do device (`created_at` em segundos Unix). Nada
    /// sobrevive ao processo.
    pub fn new(
        identity: &dyn IdentityProvider,
        network_key: &PublicKey,
        nostr_key: &NostrPublicKey,
        created_at: u64,
    ) -> Result<Self, Error> {
        Self::build(
            Provider::open(None)?,
            identity,
            network_key,
            nostr_key,
            created_at,
        )
    }

    /// Device persistente: o estado MLS vive no banco SQLite em `path`. Na primeira vez gera e
    /// certifica a chave; nas seguintes recarrega a mesma (e falha se o banco é de outra identidade
    /// ou foi criado com outra chave de rede ou Nostr).
    pub fn open(
        identity: &dyn IdentityProvider,
        network_key: &PublicKey,
        nostr_key: &NostrPublicKey,
        path: &std::path::Path,
        created_at: u64,
    ) -> Result<Self, Error> {
        Self::build(
            Provider::open(Some(path))?,
            identity,
            network_key,
            nostr_key,
            created_at,
        )
    }

    /// Reabre um device **pareado** (P30 2c-ii): o certificado, instalado por
    /// [`crate::PendingDevice::install_certificate`], está no banco. Não pede `IdentityProvider` (o device
    /// não tem a mestra nem a autoridade), mas pede a identidade `expected`, guardada **fora** deste banco
    /// (o chamador a recebeu fora de banda no pareamento): o certificado é verificado a cada abertura e
    /// tem de ser dela ([`Error::IdentityMismatch`]). Falha também se o banco não tem certificado
    /// ([`Error::NotPaired`]) ou se as chaves de rede ou Nostr não são as do certificado
    /// ([`Error::DeviceMismatch`]).
    pub fn open_paired(
        expected: &IdentityId,
        network_key: &PublicKey,
        nostr_key: &NostrPublicKey,
        path: &std::path::Path,
    ) -> Result<Self, Error> {
        let provider = Provider::open(Some(path))?;
        let Some((signer, certificate)) = load_signer(&provider)? else {
            return Err(Error::NotPaired);
        };
        // O banco é só um arquivo: o certificado é conferido a cada abertura (P31(b)), e a chave MLS dele
        // tem de ser a do signer guardado, senão o device falaria com uma credencial que não é a sua.
        certificate.verify()?;
        // A identidade esperada vem de fora do banco (auditoria 2c-ii, I1): `authorize_device` não exige
        // prova de posse, então um certificado válido de outra identidade, com as mesmas três chaves,
        // passaria em todo o resto.
        if certificate.identity() != expected {
            return Err(Error::IdentityMismatch);
        }
        if &certificate.network_key != network_key || &certificate.nostr_key != nostr_key {
            return Err(Error::DeviceMismatch);
        }
        if certificate.signing_key != public_key_from_ed25519(signer.public())? {
            return Err(Error::InvalidCredential);
        }
        Ok(Self::assemble(provider, signer, certificate))
    }

    /// Monta o device a partir do que já foi conferido: o signer, o certificado dele e o banco.
    pub(crate) fn assemble(
        provider: Provider,
        signer: SignatureKeyPair,
        certificate: DeviceCertificate,
    ) -> Self {
        let credential = CredentialWithKey {
            credential: BasicCredential::new(certificate.to_bytes()).into(),
            signature_key: signer.public().into(),
        };
        let identity = certificate.identity().clone();
        Self(Rc::new(Inner {
            provider,
            signer,
            credential,
            certificate,
            identity,
        }))
    }

    fn build(
        provider: Provider,
        identity: &dyn IdentityProvider,
        network_key: &PublicKey,
        nostr_key: &NostrPublicKey,
        created_at: u64,
    ) -> Result<Self, Error> {
        // Um pedido de pareamento pendente já declarou a chave MLS deste banco: gerar outra por cima a
        // orfanaria (P30 2c-ii).
        if provider.meta(META_CERT)?.is_none() && provider.meta(META_REQUEST)?.is_some() {
            return Err(Error::PairingInProgress);
        }
        let (signer, certificate) = match load_signer(&provider)? {
            Some(found) => found,
            None => {
                let signer = SignatureKeyPair::new(SignatureScheme::ED25519).map_err(Error::mls)?;
                signer.store(provider.storage()).map_err(Error::mls)?;
                let signing_key = public_key_from_ed25519(signer.public())?;
                let certificate =
                    identity.authorize_device(&signing_key, network_key, nostr_key, created_at)?;
                provider.set_meta(META_SIGNER, signer.public())?;
                provider.set_meta(META_CERT, &certificate.to_bytes())?;
                (signer, certificate)
            }
        };
        if *certificate.identity() != identity.id() {
            return Err(Error::IdentityMismatch);
        }
        if &certificate.network_key != network_key || &certificate.nostr_key != nostr_key {
            return Err(Error::DeviceMismatch);
        }
        Ok(Self::assemble(provider, signer, certificate))
    }

    /// Guarda que a conversa `conversation_id` pertence a `label` (ex.: um Peer ID), para
    /// recarregá-la depois de reiniciar.
    pub fn remember(&self, label: &str, conversation_id: &[u8]) -> Result<(), Error> {
        self.0.provider.remember(label, conversation_id)
    }

    /// Pares `(label, conversation_id)` guardados com [`Self::remember`].
    pub fn remembered(&self) -> Result<Vec<(String, Vec<u8>)>, Error> {
        self.0.provider.remembered()
    }

    /// Certificado deste device (identidade + chave MLS + chave de rede): é o que um convite carrega
    /// para provar quem é e por qual Peer ID alcançá-lo.
    pub fn certificate(&self) -> &DeviceCertificate {
        &self.0.certificate
    }

    /// Identidade a quem este device pertence.
    pub fn identity(&self) -> &IdentityId {
        &self.0.identity
    }

    /// Gera um KeyPackage de uso único, para quem quiser convidar este device.
    pub fn key_package(&self) -> Result<Vec<u8>, Error> {
        let bundle = KeyPackage::builder()
            .build(
                CIPHERSUITE,
                &self.0.provider,
                &self.0.signer,
                self.0.credential.clone(),
            )
            .map_err(Error::mls)?;
        bundle
            .key_package()
            .tls_serialize_detached()
            .map_err(Error::mls)
    }
}

pub(crate) const META_SIGNER: &str = "signer_public";
pub(crate) const META_CERT: &str = "device_certificate";
/// Pedido de pareamento pendente (P30 2c-ii): existe só enquanto o certificado não chegou.
pub(crate) const META_REQUEST: &str = "pair_request";

/// Recarrega a chave de assinatura e o certificado salvos, se existirem.
fn load_signer(
    provider: &Provider,
) -> Result<Option<(SignatureKeyPair, kin_identity::DeviceCertificate)>, Error> {
    let (Some(public), Some(cert)) = (provider.meta(META_SIGNER)?, provider.meta(META_CERT)?)
    else {
        return Ok(None);
    };
    let signer = SignatureKeyPair::read(provider.storage(), &public, SignatureScheme::ED25519)
        .ok_or_else(|| Error::storage("chave de assinatura ausente no banco"))?;
    let cert = kin_identity::DeviceCertificate::from_bytes(&cert)?;
    Ok(Some((signer, cert)))
}
