use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kin_identity::{DeviceCertificate, DeviceKey, IdentityId};
use kin_transport::{Multiaddr, PeerId};

use crate::Error;

/// Prazo padrão de um convite (P18, ADR 0033).
pub const DEFAULT_INVITE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Maior validade aceita ao ler um link: além disso o link é recusado (relógio errado ou forjado).
pub const MAX_INVITE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Versão do convite dentro da `DeviceList` (sem validade nem assinatura própria).
const VERSION: u8 = 1;
/// Versão do link, com validade e assinatura (P18).
const LINK_VERSION: u8 = 2;
const SIGN_DOMAIN: &[u8] = b"kin/invite/v2";
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
        self.certificate.identity()
    }

    /// Peer ID do device que atende pelo convite.
    pub fn peer_id(&self) -> PeerId {
        self.certificate.network_peer_id()
    }

    /// Endereços onde o device pode ser alcançado (direto ou por circuito de relay).
    pub fn addrs(&self) -> &[Multiaddr] {
        &self.addrs
    }

    /// O convite como link (`kin://invite/...`), próprio para texto ou QR, válido até `expires_at`
    /// (segundos Unix) e assinado por `key`, a chave de rede do device do convite (P18).
    ///
    /// Formato v2: `versão 2 | len16 certificado | n u8 | n × (len16 endereço) | expires_at u64 (BE) |
    /// len16 assinatura`. A assinatura (Ed25519, chave de rede) cobre `DOMAIN | tudo antes dela`.
    pub fn to_link(&self, key: &DeviceKey, expires_at: u64) -> String {
        let mut out = self.encode_as(LINK_VERSION);
        out.extend_from_slice(&expires_at.to_be_bytes());
        let signature = key
            .keypair()
            .sign(&signed_message(&out))
            .expect("assinar com Ed25519 não falha");
        put(&mut out, &signature);
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(out))
    }

    /// Lê um link de convite com o relógio do sistema. Ver [`Self::from_link_at`].
    pub fn from_link(link: &str) -> Result<Self, Error> {
        Self::from_link_at(link, now_secs())
    }

    /// Lê um link de convite no instante `now` (segundos Unix). Confere a assinatura do certificado, a
    /// assinatura do link pela chave de rede do certificado e a validade: vencido é
    /// `Error::ExpiredInvite`; validade além de `MAX_INVITE_TTL` no futuro é `Error::InvalidInvite`.
    pub fn from_link_at(link: &str, now: u64) -> Result<Self, Error> {
        let body = link
            .trim()
            .strip_prefix(PREFIX)
            .ok_or(Error::InvalidInvite)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| Error::InvalidInvite)?;
        let (invite, rest) = Self::parse(&bytes, LINK_VERSION)?;
        let signed_len = bytes.len() - rest.len() + 8;
        let mut tail = rest;
        let expires_at = u64::from_be_bytes(
            tail.get(..8)
                .ok_or(Error::InvalidInvite)?
                .try_into()
                .map_err(|_| Error::InvalidInvite)?,
        );
        tail = &tail[8..];
        let signature = take(&mut tail)?;
        if !tail.is_empty() {
            return Err(Error::InvalidInvite);
        }
        // Só quem tem a chave de rede do certificado escolhe a validade (e os endereços).
        let network_key = &invite.certificate.network_key;
        if !network_key.verify(&signed_message(&bytes[..signed_len]), signature) {
            return Err(Error::InvalidInvite);
        }
        if expires_at > now.saturating_add(MAX_INVITE_TTL.as_secs()) {
            return Err(Error::InvalidInvite);
        }
        if now >= expires_at {
            return Err(Error::ExpiredInvite);
        }
        Ok(invite)
    }

    /// Os bytes do convite sem o prefixo nem base64 (o que `DeviceList` carrega por item).
    pub(crate) fn encode(&self) -> Vec<u8> {
        self.encode_as(VERSION)
    }

    fn encode_as(&self, version: u8) -> Vec<u8> {
        let mut out = vec![version];
        put(&mut out, &self.certificate.to_bytes());
        out.push(self.addrs.len() as u8);
        for addr in &self.addrs {
            put(&mut out, &addr.to_vec());
        }
        out
    }

    /// Lê o formato de [`Self::encode`]; confere a assinatura do certificado.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let (invite, rest) = Self::parse(bytes, VERSION)?;
        if !rest.is_empty() {
            return Err(Error::InvalidInvite);
        }
        Ok(invite)
    }

    /// Lê um convite da `version` dada e devolve o que sobra depois dele.
    fn parse(bytes: &[u8], version: u8) -> Result<(Self, &[u8]), Error> {
        let mut cur = bytes;
        if take_u8(&mut cur)? != version {
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
        Ok((Self { certificate, addrs }, cur))
    }
}

/// O que a assinatura do link cobre: o domínio e os bytes do link até a validade.
fn signed_message(body: &[u8]) -> Vec<u8> {
    [SIGN_DOMAIN, body].concat()
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
