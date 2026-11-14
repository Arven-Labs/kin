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

use crate::device::{META_CERT, META_REQUEST, META_SIGNER};
use crate::provider::Provider;
use crate::{CryptoDevice, Error};

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
    pub fn request(&self, device_key: &DeviceKey) -> Result<PairRequest, Error> {
        let signing_key = public_key_from_ed25519(self.signer.public())?;
        if let Some(bytes) = self.provider.meta(META_REQUEST)? {
            let request = PairRequest::from_bytes(&bytes)?;
            // O banco é só um arquivo: o pedido guardado tem de ser deste signer e desta device.key.
            if request.signing_key != signing_key
            {
                return Err(Error::DeviceMismatch);
            }
            request.verify()?;
            return Ok(request);
        }
        let request = PairRequest::generate(device_key, &signing_key, |payload| {
            self.signer
                .sign(payload)
                .map_err(|_| kin_identity::Error::InvalidPairRequest)
        })?;
        self.provider.set_meta(META_REQUEST, &request.to_bytes())?;
        Ok(request)
    }

    /// Instala o certificado devolvido pelo primário, só se for da identidade `expected` (recebida fora de
    /// banda), válido e das chaves do pedido guardado. Se algo falhar, o banco **continua pendente**: o
    /// certificado e o fim do pedido só são gravados depois de tudo conferido, numa transação.
    pub fn install_certificate(
        self,
        certificate: &DeviceCertificate,
        expected: &IdentityId,
    ) -> Result<CryptoDevice, Error> {
        let bytes = self.provider.meta(META_REQUEST)?.ok_or(Error::NotPending)?;
        let request = PairRequest::from_bytes(&bytes)?;
        request.verify()?;
        if request.signing_key != public_key_from_ed25519(self.signer.public())? {
            return Err(Error::DeviceMismatch);
        }
        request.accept_certificate(certificate, expected)?;
        self.provider
            .finish_pairing(&certificate.to_bytes(), META_CERT, META_REQUEST)?;
        Ok(CryptoDevice::assemble(
            self.provider,
            self.signer,
            certificate.clone(),
        ))
    }
}
