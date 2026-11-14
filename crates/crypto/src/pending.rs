//! Device pendente de pareamento (P30 fatia 2c-ii): tem a chave MLS e o pedido, mas ainda não tem
//! certificado. É o lado do device novo; o primário responde com o certificado v4 (ADR 0016).
//!
//! Rascunho (stubs da fatia 2c-ii): o corpo vem depois dos testes.

use std::path::Path;

use kin_identity::{DeviceCertificate, DeviceKey, IdentityId, PairRequest};

use crate::{CryptoDevice, Error};

/// Banco com a chave MLS e o pedido de pareamento, à espera do certificado.
pub struct PendingDevice {
    _private: (),
}

impl PendingDevice {
    /// Abre (ou cria) o banco pendente em `path`. Recusa um banco que já tem certificado
    /// ([`Error::AlreadyPaired`]). Reaproveita a chave MLS guardada ou gera uma, sem certificá-la.
    pub fn open(_path: &Path) -> Result<Self, Error> {
        todo!("2c-ii: abrir o banco pendente")
    }

    /// O pedido de pareamento deste device: o guardado, se já houve um (mesmo nonce, mesmo código de
    /// comparação depois de reabrir), ou um novo, que fica guardado.
    pub fn request(&self, _device_key: &DeviceKey) -> Result<PairRequest, Error> {
        todo!("2c-ii: gerar e guardar o pedido")
    }

    /// Instala o certificado devolvido pelo primário, só se for da identidade `expected` (recebida fora de
    /// banda), válido e das chaves do pedido guardado. Se algo falhar, o banco **continua pendente**.
    pub fn install_certificate(
        self,
        _certificate: &DeviceCertificate,
        _expected: &IdentityId,
    ) -> Result<CryptoDevice, Error> {
        todo!("2c-ii: conferir, gravar o certificado e apagar o pedido")
    }
}
