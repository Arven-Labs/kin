use std::path::Path;

use libp2p_identity::{Keypair, PeerId, PublicKey};
use secp256k1::{Keypair as SchnorrKeypair, Secp256k1, XOnlyPublicKey};

use crate::{AuthorityCertificate, Error, IdentityId, KdfParams, store};

const CERT_DOMAIN: &[u8] = b"kin/device-cert/v3";

/// Versão do formato do arquivo de chave do device (`DeviceKey::to_bytes`). O formato antigo era só o
/// protobuf do Ed25519 (começa por 0x08), então qualquer outro primeiro byte é recusado.
const DEVICE_KEY_FORMAT: u8 = 1;

/// Chave pública Nostr de um device: Schnorr BIP-340 sobre secp256k1, só a coordenada X (32 bytes).
/// É o endereço do device nos relays Nostr (ADR 0004). Só existe se for um ponto válido da curva.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NostrPublicKey(XOnlyPublicKey);

impl NostrPublicKey {
    /// Lê 32 bytes x-only. Falha se o tamanho for outro ou se a coordenada não for de um ponto da curva.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let array: [u8; 32] = bytes.try_into().map_err(|_| Error::InvalidNostrKey)?;
        XOnlyPublicKey::from_byte_array(&array)
            .map(Self)
            .map_err(|_| Error::InvalidNostrKey)
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.serialize()
    }
}

/// Chaves de um device: a de rede (Ed25519, de onde sai o Peer ID) e a Nostr (Schnorr secp256k1).
/// São geradas independentemente; saber uma não revela a outra.
pub struct DeviceKey {
    network: Keypair,
    nostr: SchnorrKeypair,
}

impl DeviceKey {
    pub fn generate() -> Self {
        let secp = Secp256k1::signing_only();
        Self {
            network: Keypair::generate_ed25519(),
            nostr: SchnorrKeypair::new(&secp, &mut secp256k1::rand::thread_rng()),
        }
    }

    pub fn public_key(&self) -> PublicKey {
        self.network.public()
    }

    pub fn peer_id(&self) -> PeerId {
        self.network.public().to_peer_id()
    }

    /// Chave pública Nostr deste device, a que o certificado cobre.
    pub fn nostr_public_key(&self) -> NostrPublicKey {
        NostrPublicKey(self.nostr.x_only_public_key().0)
    }

    /// Chave **secreta** Nostr do device, para o backend Nostr assinar e decifrar. Quem a recebe deve
    /// tratá-la como segredo: nunca registrar, nunca serializar fora do `device.key`.
    pub fn nostr_secret_bytes(&self) -> [u8; 32] {
        self.nostr.secret_bytes()
    }

    /// Acesso ao keypair para o transporte (libp2p) assinar o handshake.
    pub fn keypair(&self) -> &Keypair {
        &self.network
    }

    /// `versão u8 | len16 | keypair de rede (protobuf) | chave secreta Nostr (32 bytes)`.
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        let mut out = vec![DEVICE_KEY_FORMAT];
        put(
            &mut out,
            &zeroize::Zeroizing::new(self.network.to_protobuf_encoding()?),
        );
        out.extend_from_slice(&zeroize::Zeroizing::new(self.nostr.secret_bytes())[..]);
        Ok(out)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let (&format, mut cur) = bytes.split_first().ok_or(Error::InvalidDeviceKey)?;
        if format != DEVICE_KEY_FORMAT {
            return Err(Error::InvalidDeviceKey);
        }
        let network =
            Keypair::from_protobuf_encoding(take(&mut cur).map_err(|_| Error::InvalidDeviceKey)?)?;
        network
            .clone()
            .try_into_ed25519()
            .map_err(|_| Error::NotEd25519)?;
        let secret = zeroize::Zeroizing::new(
            <[u8; 32]>::try_from(cur).map_err(|_| Error::InvalidDeviceKey)?,
        );
        let nostr = SchnorrKeypair::from_seckey_slice(&Secp256k1::signing_only(), &secret[..])
            .map_err(|_| Error::InvalidDeviceKey)?;
        Ok(Self { network, nostr })
    }

    /// Grava a chave do device **cifrada** com a senha (P13, ADR 0012), de forma atômica e com modo 0600.
    pub fn save_sealed(
        &self,
        path: &Path,
        passphrase: &[u8],
        params: KdfParams,
    ) -> Result<(), Error> {
        store::write_key_sealed(
            path,
            &zeroize::Zeroizing::new(self.to_bytes()?),
            passphrase,
            params,
        )
    }

    /// Cifra no lugar um `device.key` **em claro** (migração), com as mesmas garantias de
    /// `StandaloneIdentity::protect`.
    pub fn protect(path: &Path, passphrase: &[u8], params: KdfParams) -> Result<(), Error> {
        store::protect_with(path, passphrase, params, |bytes| {
            Self::from_bytes(bytes).map(|_| ())
        })
    }

    /// Lê a chave do device de um arquivo cifrado ou em claro. Cifrado sem senha:
    /// [`Error::PassphraseRequired`]; em claro **com** senha: [`Error::PlainFileWhereSealedExpected`].
    pub fn load_with(path: &Path, passphrase: Option<&[u8]>) -> Result<Self, Error> {
        Self::from_bytes(&store::read_key(path, passphrase)?)
    }
}

/// Quem assinou um [`DeviceCertificate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Issuer {
    /// A própria mestra (formato v3, o de todos os devices até o P30).
    Master,
    /// A chave de autoridade, com o certificado que a mestra deu a ela (formato v4, P30 fatia 2a,
    /// ADR 0014). A cadeia viaja dentro do certificado do device: verificar não depende de estado.
    /// Em `Box` porque o certificado de autoridade é grande e o `Master` não carrega nada.
    Authority(Box<AuthorityCertificate>),
}

/// Prova assinada pela identidade de que um device lhe pertence. Cobre as três chaves do device:
/// a de assinatura MLS, a de rede (libp2p, de onde sai o Peer ID) e a Nostr, ligando-as (P15, ADR 0004).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceCertificate {
    pub identity: IdentityId,
    /// Quem assinou: a mestra (v3) ou a autoridade certificada por ela (v4).
    pub issuer: Issuer,
    /// Chave de assinatura MLS do device.
    pub signing_key: PublicKey,
    /// Chave de rede do device (a mesma do [`DeviceKey`]).
    pub network_key: PublicKey,
    /// Chave Nostr do device: onde os relays entregam o que é para ele.
    pub nostr_key: NostrPublicKey,
    pub created_at: u64,
    pub signature: Vec<u8>,
}

impl DeviceCertificate {
    /// Bytes que a identidade assina (com separador de domínio para evitar reuso da assinatura).
    pub(crate) fn signing_payload(
        signing_key: &PublicKey,
        network_key: &PublicKey,
        nostr_key: &NostrPublicKey,
        created_at: u64,
    ) -> Vec<u8> {
        let mut payload = CERT_DOMAIN.to_vec();
        // Prefixo de tamanho: evita que duas chaves diferentes produzam o mesmo payload.
        for encoded in [
            signing_key.encode_protobuf(),
            network_key.encode_protobuf(),
            nostr_key.to_bytes().to_vec(),
        ] {
            payload.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
            payload.extend_from_slice(&encoded);
        }
        payload.extend_from_slice(&created_at.to_be_bytes());
        payload
    }

    /// Bytes que a autoridade assina no v4: domínio próprio, a identidade e a chave de autoridade
    /// (no v4 quem assina não é a identidade, então ela precisa estar no conteúdo) e as do device.
    #[allow(dead_code)] // stub da etapa de testes; some na implementação
    pub(crate) fn signing_payload_v4(
        identity: &IdentityId,
        authority_key: &PublicKey,
        signing_key: &PublicKey,
        network_key: &PublicKey,
        nostr_key: &NostrPublicKey,
        created_at: u64,
    ) -> Vec<u8> {
        let _ = (
            identity,
            authority_key,
            signing_key,
            network_key,
            nostr_key,
            created_at,
        );
        todo!("P30 2a: payload v4")
    }

    /// Confere a assinatura do certificado (e, no v4, a cadeia mestra → autoridade → device).
    pub fn verify(&self) -> Result<(), Error> {
        if let Issuer::Authority(_) = &self.issuer {
            todo!("P30 2a: verificação do v4")
        }
        let payload = Self::signing_payload(
            &self.signing_key,
            &self.network_key,
            &self.nostr_key,
            self.created_at,
        );
        if self.identity.public_key().verify(&payload, &self.signature) {
            Ok(())
        } else {
            Err(Error::InvalidCertificate)
        }
    }

    /// Serializa o certificado: `len16|identidade | len16|chave MLS | len16|chave de rede | len16|chave Nostr | created_at u64 BE | len16|assinatura`.
    pub fn to_bytes(&self) -> Vec<u8> {
        if let Issuer::Authority(_) = &self.issuer {
            todo!("P30 2a: serialização do v4")
        }
        let mut out = Vec::new();
        put(&mut out, &self.identity.public_key().encode_protobuf());
        put(&mut out, &self.signing_key.encode_protobuf());
        put(&mut out, &self.network_key.encode_protobuf());
        put(&mut out, &self.nostr_key.to_bytes());
        out.extend_from_slice(&self.created_at.to_be_bytes());
        put(&mut out, &self.signature);
        out
    }

    /// Lê um certificado serializado por [`Self::to_bytes`]. Não confere a assinatura: use `verify`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.first().is_some_and(|&b| b != 0) {
            todo!("P30 2a: leitura do v4")
        }
        let mut cur = bytes;
        let identity = PublicKey::try_decode_protobuf(take(&mut cur)?)?;
        let signing_key = PublicKey::try_decode_protobuf(take(&mut cur)?)?;
        let network_key = PublicKey::try_decode_protobuf(take(&mut cur)?)?;
        let nostr_key =
            NostrPublicKey::from_bytes(take(&mut cur)?).map_err(|_| Error::InvalidCertificate)?;
        let created_at = u64::from_be_bytes(
            cur.get(..8)
                .ok_or(Error::InvalidCertificate)?
                .try_into()
                .map_err(|_| Error::InvalidCertificate)?,
        );
        cur = &cur[8..];
        let signature = take(&mut cur)?.to_vec();
        if !cur.is_empty() {
            return Err(Error::InvalidCertificate);
        }
        Ok(Self {
            identity: IdentityId::new(identity),
            issuer: Issuer::Master,
            signing_key,
            network_key,
            nostr_key,
            created_at,
            signature,
        })
    }

    /// Peer ID de rede do device certificado.
    pub fn network_peer_id(&self) -> PeerId {
        self.network_key.to_peer_id()
    }
}

pub(crate) fn put(out: &mut Vec<u8>, field: &[u8]) {
    let len = u16::try_from(field.len()).expect("campo do certificado maior que 64 KiB");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(field);
}

pub(crate) fn take<'a>(cur: &mut &'a [u8]) -> Result<&'a [u8], Error> {
    let len = u16::from_be_bytes(
        cur.get(..2)
            .ok_or(Error::InvalidCertificate)?
            .try_into()
            .map_err(|_| Error::InvalidCertificate)?,
    ) as usize;
    let field = cur.get(2..2 + len).ok_or(Error::InvalidCertificate)?;
    *cur = &cur[2 + len..];
    Ok(field)
}

/// Monta uma [`PublicKey`] a partir dos 32 bytes de uma chave pública Ed25519.
pub fn public_key_from_ed25519(bytes: &[u8]) -> Result<PublicKey, Error> {
    let key = libp2p_identity::ed25519::PublicKey::try_from_bytes(bytes)
        .map_err(|_| Error::NotEd25519)?;
    Ok(libp2p_identity::PublicKey::from(key))
}

/// Extrai os 32 bytes de uma chave pública Ed25519.
pub fn ed25519_bytes(key: &PublicKey) -> Result<[u8; 32], Error> {
    Ok(key
        .clone()
        .try_into_ed25519()
        .map_err(|_| Error::NotEd25519)?
        .to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASS: &[u8] = b"senha de teste";

    #[test]
    fn sealed_device_key_loads_back_with_the_same_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.key");
        let device = DeviceKey::generate();
        device.save_sealed(&path, PASS, KdfParams::MIN).unwrap();
        let loaded = DeviceKey::load_with(&path, Some(PASS)).unwrap();
        assert_eq!(loaded.peer_id(), device.peer_id());
        assert_eq!(loaded.nostr_public_key(), device.nostr_public_key());
        assert_eq!(loaded.nostr_secret_bytes(), device.nostr_secret_bytes());
    }

    #[test]
    fn sealed_device_key_does_not_leak_the_nostr_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.key");
        let device = DeviceKey::generate();
        device.save_sealed(&path, PASS, KdfParams::MIN).unwrap();
        let on_disk = std::fs::read(&path).unwrap();
        let secret = device.nostr_secret_bytes();
        assert!(!on_disk.windows(32).any(|w| w == secret));
    }

    #[test]
    fn sealed_device_key_needs_the_right_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.key");
        DeviceKey::generate()
            .save_sealed(&path, PASS, KdfParams::MIN)
            .unwrap();
        assert!(matches!(
            DeviceKey::load_with(&path, None).err().unwrap(),
            Error::PassphraseRequired
        ));
        assert!(matches!(
            DeviceKey::load_with(&path, Some(b"errada")).err().unwrap(),
            Error::WrongPassphraseOrCorrupt
        ));
    }

    #[test]
    fn legacy_plain_device_key_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.key");
        let device = DeviceKey::generate();
        std::fs::write(&path, device.to_bytes().unwrap()).unwrap();
        let loaded = DeviceKey::load_with(&path, None).unwrap();
        assert_eq!(loaded.peer_id(), device.peer_id());
    }

    #[test]
    fn protecting_a_legacy_device_key_keeps_the_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.key");
        let device = DeviceKey::generate();
        std::fs::write(&path, device.to_bytes().unwrap()).unwrap();
        DeviceKey::protect(&path, PASS, KdfParams::MIN).unwrap();
        let loaded = DeviceKey::load_with(&path, Some(PASS)).unwrap();
        assert_eq!(loaded.nostr_secret_bytes(), device.nostr_secret_bytes());
    }

    #[test]
    fn legacy_plain_device_key_is_refused_when_a_passphrase_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.key");
        std::fs::write(&path, DeviceKey::generate().to_bytes().unwrap()).unwrap();
        assert!(matches!(
            DeviceKey::load_with(&path, Some(PASS)).err().unwrap(),
            Error::PlainFileWhereSealedExpected
        ));
    }

    #[test]
    fn protecting_a_device_key_refuses_garbage_and_keeps_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.key");
        for content in [&b""[..], &b"lixo qualquer"[..]] {
            std::fs::write(&path, content).unwrap();
            assert!(DeviceKey::protect(&path, PASS, KdfParams::MIN).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), content, "original alterado");
        }
    }
}
