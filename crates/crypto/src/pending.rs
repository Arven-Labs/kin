//! Device pendente de pareamento (P30 fatia 2c-ii): tem a chave MLS e o pedido, mas ainda não tem
//! certificado. É o lado do device novo; o primário responde com o certificado v4 (ADR 0016).
//!
//! O pedido fica guardado no banco: o código de comparação que a pessoa confere é calculado sobre ele,
//! então reabrir não pode trocá-lo, e o certificado só é aceito se for o deste pedido.

use std::path::Path;

use kin_identity::{
    DeviceCertificate, DeviceKey, IdentityId, PairRequest, public_key_from_ed25519,
};
use openmls::prelude::SignatureScheme;
use openmls_basic_credential::SignatureKeyPair;
use openmls_traits::OpenMlsProvider as _;
use openmls_traits::signatures::Signer as _;

use crate::device::{META_CERT, META_REQUEST, META_REQUESTED_AT, META_SIGNER};
use crate::provider::Provider;
use crate::{CryptoDevice, Error};

/// Quanto o pedido de pareamento vale, em segundos (15 min): tempo de levar o link ao primário, comparar
/// o código e voltar com a resposta, sem deixar um pedido esquecido aberto (P30 2d-ii, decisão do dono).
pub const PAIR_REQUEST_TTL_SECS: u64 = 900;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Banco com a chave MLS e o pedido de pareamento, à espera do certificado.
pub struct PendingDevice {
    provider: Provider,
    signer: SignatureKeyPair,
}

impl PendingDevice {
    /// Abre (ou cria) o banco pendente em `path`. Recusa um banco que já tem certificado
    /// ([`Error::AlreadyPaired`]). Reaproveita a chave MLS guardada ou gera uma, sem certificá-la.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let provider = Provider::open(Some(path))?;
        if provider.meta(META_CERT)?.is_some() {
            return Err(Error::AlreadyPaired);
        }
        let signer = match provider.meta(META_SIGNER)? {
            Some(public) => {
                SignatureKeyPair::read(provider.storage(), &public, SignatureScheme::ED25519)
                    .ok_or_else(|| Error::storage("chave de assinatura ausente no banco"))?
            }
            None => {
                let signer = SignatureKeyPair::new(SignatureScheme::ED25519).map_err(Error::mls)?;
                signer.store(provider.storage()).map_err(Error::mls)?;
                provider.set_meta(META_SIGNER, signer.public())?;
                signer
            }
        };
        Ok(Self { provider, signer })
    }

    /// O pedido de pareamento deste device: o guardado, se já houve um (mesmo nonce, mesmo código de
    /// comparação depois de reabrir), ou um novo, que fica guardado.
    ///
    /// O pedido vale [`PAIR_REQUEST_TTL_SECS`] a partir de quando foi criado; passado o prazo, este método
    /// o descarta e gera outro (novo nonce, novo código de comparação). Usa o relógio do sistema.
    pub fn request(&self, device_key: &DeviceKey) -> Result<PairRequest, Error> {
        self.request_at(device_key, now_secs())
    }

    /// Como [`Self::request`], com o instante `now` (segundos Unix) passado de fora, para testar o prazo.
    pub fn request_at(&self, device_key: &DeviceKey, now: u64) -> Result<PairRequest, Error> {
        let signing_key = public_key_from_ed25519(self.signer.public())?;
        if let Some(bytes) = self.provider.meta(META_REQUEST)? {
            if !self.is_expired(now)? || now > 0 {
                return Self::check_stored(&bytes, &signing_key, device_key);
            }
            self.discard_request()?;
        }
        let request = PairRequest::generate(device_key, &signing_key, |payload| {
            self.signer
                .sign(payload)
                .map_err(|_| kin_identity::Error::InvalidPairRequest)
        })?;
        // A hora primeiro: um corte entre as duas gravações deixa a hora sem pedido (inofensivo), nunca um
        // pedido sem hora (que contaria como expirado e obrigaria a pedir de novo).
        self.provider
            .set_meta(META_REQUESTED_AT, &now.to_be_bytes())?;
        self.provider.set_meta(META_REQUEST, &request.to_bytes())?;
        Ok(request)
    }

    /// O pedido guardado passou do prazo? Hora ausente ou ilegível conta como expirada (falha fechado:
    /// um banco da 2c-ii, sem hora, ou editado à mão, obriga a pedir de novo). Um relógio que voltou para
    /// antes da criação do pedido o trata como recém-criado.
    fn is_expired(&self, now: u64) -> Result<bool, Error> {
        let Some(raw) = self.provider.meta(META_REQUESTED_AT)? else {
            return Ok(true);
        };
        let Ok(bytes) = <[u8; 8]>::try_from(raw.as_slice()) else {
            return Ok(true);
        };
        Ok(now.saturating_sub(u64::from_be_bytes(bytes)) > PAIR_REQUEST_TTL_SECS)
    }

    /// Descarta o pedido guardado (a pessoa viu um código diferente, ou desistiu). A chave MLS fica; o
    /// próximo [`Self::request`] gera um pedido novo, com outro nonce e outro código. O certificado cobre
    /// as chaves (não o nonce): um emitido para as chaves deste device continua válido; um para as chaves
    /// de um intruso nunca casa.
    pub fn discard_request(&self) -> Result<(), Error> {
        self.provider.delete_meta(META_REQUEST)?;
        self.provider.delete_meta(META_REQUESTED_AT)
    }

    /// O banco é só um arquivo: o pedido guardado só vale se for deste signer e desta `device.key`.
    fn check_stored(
        bytes: &[u8],
        signing_key: &kin_identity::PublicKey,
        device_key: &DeviceKey,
    ) -> Result<PairRequest, Error> {
        let request = PairRequest::from_bytes(bytes)?;
        if &request.signing_key != signing_key
            || request.network_key != device_key.public_key()
            || request.nostr_key != device_key.nostr_public_key()
        {
            return Err(Error::DeviceMismatch);
        }
        request.verify()?;
        Ok(request)
    }

    /// Instala o certificado devolvido pelo primário, só se for da identidade `expected` (recebida fora de
    /// banda), válido e das chaves do pedido guardado. Se algo falhar, o banco **continua pendente**: o
    /// certificado e o fim do pedido só são gravados depois de tudo conferido, numa transação.
    ///
    /// Recusa com [`Error::PairRequestExpired`] se o pedido passou do prazo ([`PAIR_REQUEST_TTL_SECS`]).
    /// Usa o relógio do sistema.
    pub fn install_certificate(
        self,
        device_key: &DeviceKey,
        certificate: &DeviceCertificate,
        expected: &IdentityId,
    ) -> Result<CryptoDevice, Error> {
        self.install_certificate_at(device_key, certificate, expected, now_secs())
    }

    /// Como [`Self::install_certificate`], com o instante `now` (segundos Unix) passado de fora.
    pub fn install_certificate_at(
        self,
        device_key: &DeviceKey,
        certificate: &DeviceCertificate,
        expected: &IdentityId,
        now: u64,
    ) -> Result<CryptoDevice, Error> {
        let bytes = self.provider.meta(META_REQUEST)?.ok_or(Error::NotPending)?;
        if self.is_expired(now)? {
            return Err(Error::PairRequestExpired);
        }
        let signing_key = public_key_from_ed25519(self.signer.public())?;
        let request = Self::check_stored(&bytes, &signing_key, device_key)?;
        request.accept_certificate(certificate, expected)?;
        self.provider
            .finish_pairing(&certificate.to_bytes(), META_CERT, &bytes, META_REQUEST)?;
        Ok(CryptoDevice::assemble(
            self.provider,
            self.signer,
            certificate.clone(),
        ))
    }
}
