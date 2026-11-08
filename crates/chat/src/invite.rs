use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use kin_identity::{DeviceCertificate, IdentityId};
use kin_transport::{Multiaddr, PeerId};

use crate::Error;

const VERSION: u8 = 1;
const PREFIX: &str = "kin://invite/";
const MAX_ADDRS: usize = 32;

/// Convite para conversar: o certificado do device de quem convida e onde alcançá-lo.
///
/// O certificado é assinado pela identidade e cobre a chave de rede do device, então o convite prova
/// **quem** é (identidade) e **qual Peer ID** atende por ela (P15). Quem o aceita exige exatamente isso
/// do outro lado (`Chat::accept`). Gerar e compartilhar o link é o consentimento de quem convida
/// (D5, nível "só por link"): não há pedido de amizade nem caixa de entrada.
///
/// Formato v1 (manual, como `Message`): `versão u8 | len16 certificado | n u8 | n × (len16 endereço)`,
/// em base64url sem padding, depois de `kin://invite/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    certificate: DeviceCertificate,
    addrs: Vec<Multiaddr>,
}

impl Invite {
    /// Monta um convite a partir de um certificado e endereços (no máximo 32). Normalmente se usa
    /// `Chat::invite`; aqui o certificado não é conferido (`from_link` confere).
    pub fn new(certificate: DeviceCertificate, mut addrs: Vec<Multiaddr>) -> Self {
        addrs.truncate(MAX_ADDRS);
        Self { certificate, addrs }
    }

    /// Identidade de quem convidou.
    pub fn identity(&self) -> &IdentityId {
        &self.certificate.identity
    }

    /// Peer ID do device que atende pelo convite.
    pub fn peer_id(&self) -> PeerId {
        self.certificate.network_peer_id()
    }

    /// Endereços onde o device pode ser alcançado (direto ou por circuito de relay).
    pub fn addrs(&self) -> &[Multiaddr] {
        &self.addrs
    }

    /// O convite como link (`kin://invite/...`), próprio para texto ou QR.
    pub fn to_link(&self) -> String {
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(self.encode()))
    }

    /// Lê um link de convite. Confere a assinatura do certificado.
    pub fn from_link(link: &str) -> Result<Self, Error> {
        let body = link
            .trim()
            .strip_prefix(PREFIX)
            .ok_or(Error::InvalidInvite)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| Error::InvalidInvite)?;
        Self::decode(&bytes)
    }

    /// Os bytes do convite sem o prefixo nem base64 (o que `DeviceList` carrega por item).
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        put(&mut out, &self.certificate.to_bytes());
        out.push(self.addrs.len() as u8);
        for addr in &self.addrs {
            put(&mut out, &addr.to_vec());
        }
        out
    }

    /// Lê o formato de [`Self::encode`]; confere a assinatura do certificado.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut cur = bytes;
        if take_u8(&mut cur)? != VERSION {
            return Err(Error::InvalidInvite);
        }
        let certificate =
            DeviceCertificate::from_bytes(take(&mut cur)?).map_err(|_| Error::InvalidInvite)?;
        certificate.verify().map_err(|_| Error::InvalidInvite)?;
        let count = take_u8(&mut cur)? as usize;
        if count > MAX_ADDRS {
            return Err(Error::InvalidInvite);
        }
        let mut addrs = Vec::with_capacity(count);
        for _ in 0..count {
            let addr =
                Multiaddr::try_from(take(&mut cur)?.to_vec()).map_err(|_| Error::InvalidInvite)?;
            addrs.push(addr);
        }
        if !cur.is_empty() {
            return Err(Error::InvalidInvite);
        }
        Ok(Self { certificate, addrs })
    }
}

fn put(out: &mut Vec<u8>, field: &[u8]) {
    let len = u16::try_from(field.len()).expect("campo do convite maior que 64 KiB");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(field);
}

fn take_u8(cur: &mut &[u8]) -> Result<u8, Error> {
    let (&byte, rest) = cur.split_first().ok_or(Error::InvalidInvite)?;
    *cur = rest;
    Ok(byte)
}

fn take<'a>(cur: &mut &'a [u8]) -> Result<&'a [u8], Error> {
    let len = u16::from_be_bytes(
        cur.get(..2)
            .ok_or(Error::InvalidInvite)?
            .try_into()
            .map_err(|_| Error::InvalidInvite)?,
    ) as usize;
    let field = cur.get(2..2 + len).ok_or(Error::InvalidInvite)?;
    *cur = &cur[2 + len..];
    Ok(field)
}
