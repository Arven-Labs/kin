use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kin_identity::{DeviceCertificate, IdentityId, InviteSigner};
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
/// Versão do link com validade e assinatura (P18, ADR 0033). Ainda é lida, sem token.
const LINK_V2: u8 = 2;
/// Versão do link com validade, assinatura e o token de uso único (P18, ADR 0039).
const LINK_V3: u8 = 3;
const PREFIX: &str = "kin://invite/";
const MAX_ADDRS: usize = 32;

/// Um convite emitido por este device e ainda não usado nem vencido (P18, ADR 0039).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveInvite {
    /// Os 4 primeiros bytes do token em hexadecimal: basta para `Chat::revoke_invite` e não revela o token.
    pub id: String,
    /// Quando o token vence (segundos Unix).
    pub expires_at: u64,
}

/// Convite para conversar: o certificado do device de quem convida e onde alcançá-lo.
///
/// O certificado é assinado pela identidade e cobre a chave de rede do device, então o convite prova
/// **quem** é (identidade) e **qual Peer ID** atende por ela (P15). Quem o aceita exige exatamente isso
/// do outro lado (`Chat::accept`). Gerar e compartilhar o link é o consentimento de quem convida
/// (D5, nível "só por link"): não há pedido de amizade nem caixa de entrada.
///
/// Formato do convite (manual, como `Message`): `versão u8 | len16 certificado | n u8 | n × (len16
/// endereço)`. A versão 1 é a que a `DeviceList` carrega por item. O **link** usa a versão 3 (ver
/// [`Invite::to_link_with_token`]; a 2, sem token, ainda é lida), com validade e assinatura, em base64url
/// sem padding, depois de `kin://invite/`. A validade só existe no link: depois de `from_link`, o `Invite`
/// não a carrega (ADR 0033); o token (ADR 0039) fica em [`Invite::token`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    certificate: DeviceCertificate,
    addrs: Vec<Multiaddr>,
    /// Só o link v3 traz; `new` e a `DeviceList` ficam sem.
    token: Option<[u8; 16]>,
}

impl Invite {
    /// Monta um convite a partir de um certificado e endereços (no máximo 32). Normalmente se usa
    /// `Chat::invite`; aqui o certificado não é conferido (`from_link` confere).
    pub fn new(certificate: DeviceCertificate, mut addrs: Vec<Multiaddr>) -> Self {
        addrs.truncate(MAX_ADDRS);
        Self {
            certificate,
            addrs,
            token: None,
        }
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
    /// (segundos Unix) e assinado por `signer`, o da chave de rede do device do convite (P18).
    ///
    /// Formato v2: `versão 2 | len16 certificado | n u8 | n × (len16 endereço) | expires_at u64 (BE) |
    /// len16 assinatura`. A assinatura (Ed25519, chave de rede, domínio `kin/invite/v2` imposto pelo
    /// `InviteSigner`) cobre tudo antes dela.
    pub fn to_link(&self, signer: &InviteSigner, expires_at: u64) -> String {
        self.signed_link(LINK_V2, None, signer, expires_at)
    }

    fn signed_link(
        &self,
        version: u8,
        token: Option<[u8; 16]>,
        signer: &InviteSigner,
        expires_at: u64,
    ) -> String {
        let mut out = self.encode_as(version);
        if let Some(token) = token {
            out.extend_from_slice(&token);
        }
        out.extend_from_slice(&expires_at.to_be_bytes());
        let signature = signer.sign(&out);
        put(&mut out, &signature);
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(out))
    }

    /// Token de uso único que o link carrega (P18, ADR 0039); só `from_link` o preenche.
    pub fn token(&self) -> Option<[u8; 16]> {
        self.token
    }

    /// Como [`Self::to_link`], mas no formato v3, com o `token` dentro da parte assinada.
    ///
    /// Formato v3: `versão 3 | len16 certificado | n u8 | n × (len16 endereço) | token [u8; 16] |
    /// expires_at u64 (BE) | len16 assinatura`. A assinatura cobre tudo antes dela, inclusive o byte de
    /// versão, então um v3 não vira um v2 válido (nem o contrário) com o mesmo domínio de assinatura.
    pub fn to_link_with_token(
        &self,
        signer: &InviteSigner,
        expires_at: u64,
        token: [u8; 16],
    ) -> String {
        self.signed_link(LINK_V3, Some(token), signer, expires_at)
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
        // O primeiro byte escolhe o formato; qualquer outro (inclusive o v1, sem validade) é recusado.
        let version = match bytes.first() {
            Some(&version @ (LINK_V2 | LINK_V3)) => version,
            _ => return Err(Error::InvalidInvite),
        };
        let (mut invite, rest) = Self::parse(&bytes, version)?;
        let mut tail = rest;
        if version == LINK_V3 {
            let token = tail.get(..16).ok_or(Error::InvalidInvite)?;
            invite.token = Some(<[u8; 16]>::try_from(token).map_err(|_| Error::InvalidInvite)?);
            tail = &tail[16..];
        }
        let signed_len = bytes.len() - tail.len() + 8;
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
        if !invite
            .certificate
            .verify_invite(&bytes[..signed_len], signature)
        {
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
        Ok((
            Self {
                certificate,
                addrs,
                token: None,
            },
            cur,
        ))
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
