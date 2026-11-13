//! Chave de autoridade e seu certificado (P30 fatia 1b, opção D do ADR 0011).
//!
//! A mestra fica **fria** (offline, só assina) e certifica uma chave de **autoridade** que vive no
//! primário. Esta fatia é só custódia: nenhum byte de fio muda e a autoridade ainda não assina
//! devices nem revogações (fatias 2 e 3).

use std::path::Path;

use libp2p_identity::{Keypair, PublicKey};
use zeroize::Zeroizing;

use crate::device::{put, take};
use crate::{Error, IdentityId, KdfParams, store};

/// Domínio de assinatura do certificado de autoridade. Diferente do de `DeviceCertificate`, para uma
/// assinatura de um nunca valer como a do outro.
pub(crate) const AUTHORITY_DOMAIN: &[u8] = b"kin/authority-cert/v1";

/// Chave de autoridade (Ed25519): faz o trabalho diário no primário enquanto a mestra fica fria.
pub struct AuthorityKey {
    keypair: Keypair,
}

impl AuthorityKey {
    pub fn generate() -> Self {
        Self {
            keypair: Keypair::generate_ed25519(),
        }
    }

    pub fn public_key(&self) -> PublicKey {
        self.keypair.public()
    }

    pub fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self.keypair.sign(message)?)
    }

    /// Protobuf do keypair Ed25519 (o mesmo formato da mestra). É segredo: quem chama não registra nem copia.
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        Ok(self.keypair.to_protobuf_encoding()?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let keypair =
            Keypair::from_protobuf_encoding(bytes).map_err(|_| Error::InvalidAuthorityKey)?;
        keypair
            .clone()
            .try_into_ed25519()
            .map_err(|_| Error::NotEd25519)?;
        Ok(Self { keypair })
    }

    /// Grava a chave de autoridade **cifrada** com a senha (ADR 0012), de forma atômica e com modo 0600.
    pub fn save_sealed(
        &self,
        path: &Path,
        passphrase: &[u8],
        params: KdfParams,
    ) -> Result<(), Error> {
        store::write_key_sealed(path, &Zeroizing::new(self.to_bytes()?), passphrase, params)
    }

    /// Cifra no lugar um `authority.key` em claro (migração), com as garantias de `store::protect_with`.
    pub fn protect(path: &Path, passphrase: &[u8], params: KdfParams) -> Result<(), Error> {
        store::protect_with(path, passphrase, params, |bytes| {
            Self::from_bytes(bytes).map(|_| ())
        })
    }

    /// Lê a chave de autoridade de um arquivo cifrado ou em claro. Cifrado sem senha:
    /// [`Error::PassphraseRequired`]; em claro **com** senha: [`Error::PlainFileWhereSealedExpected`].
    pub fn load_with(path: &Path, passphrase: Option<&[u8]>) -> Result<Self, Error> {
        Self::from_bytes(&store::read_key(path, passphrase)?)
    }
}

/// A mestra afirma: "esta chave é a minha autoridade". Sem validade nesta fatia: `created_at` entra
/// na assinatura, mas nenhum código o lê como prazo (o frescor é a fatia 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityCertificate {
    pub identity: IdentityId,
    pub authority_key: PublicKey,
    pub created_at: u64,
    pub signature: Vec<u8>,
}

impl AuthorityCertificate {
    /// Bytes que a mestra assina (domínio + chave de autoridade + `created_at`).
    pub(crate) fn signing_payload(authority_key: &PublicKey, created_at: u64) -> Vec<u8> {
        let mut payload = AUTHORITY_DOMAIN.to_vec();
        let encoded = authority_key.encode_protobuf();
        // Prefixo de tamanho, como no `DeviceCertificate`.
        payload.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
        payload.extend_from_slice(&encoded);
        payload.extend_from_slice(&created_at.to_be_bytes());
        payload
    }

    /// Confere a assinatura da mestra (`identity`) sobre a chave de autoridade. Não diz se essa
    /// identidade é a que o chamador espera: isso é com quem chama.
    pub fn verify(&self) -> Result<(), Error> {
        let payload = Self::signing_payload(&self.authority_key, self.created_at);
        if self.identity.public_key().verify(&payload, &self.signature) {
            Ok(())
        } else {
            Err(Error::InvalidAuthorityCertificate)
        }
    }

    /// `len16|identidade | len16|chave de autoridade | created_at u64 BE | len16|assinatura`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, &self.identity.public_key().encode_protobuf());
        put(&mut out, &self.authority_key.encode_protobuf());
        out.extend_from_slice(&self.created_at.to_be_bytes());
        put(&mut out, &self.signature);
        out
    }

    /// Lê um certificado serializado por [`Self::to_bytes`]. Não confere a assinatura: use `verify`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        fn bad<E>(_: E) -> Error {
            Error::InvalidAuthorityCertificate
        }
        let mut cur = bytes;
        let identity = PublicKey::try_decode_protobuf(take(&mut cur).map_err(bad)?).map_err(bad)?;
        let authority_key =
            PublicKey::try_decode_protobuf(take(&mut cur).map_err(bad)?).map_err(bad)?;
        let created_at = u64::from_be_bytes(
            cur.get(..8)
                .ok_or(Error::InvalidAuthorityCertificate)?
                .try_into()
                .map_err(|_| Error::InvalidAuthorityCertificate)?,
        );
        cur = &cur[8..];
        let signature = take(&mut cur).map_err(bad)?.to_vec();
        Ok(Self {
            identity: IdentityId::new(identity),
            authority_key,
            created_at,
            signature,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeviceCertificate, DeviceKey, IdentityProvider, StandaloneIdentity};

    const PASS: &[u8] = b"senha de teste";

    fn issue(master: &StandaloneIdentity, authority: &AuthorityKey) -> AuthorityCertificate {
        master
            .certify_authority(&authority.public_key(), 1_000)
            .unwrap()
    }

    // ---- certificado ----

    #[test]
    fn a_certificate_issued_by_the_master_verifies() {
        let master = StandaloneIdentity::generate();
        let authority = AuthorityKey::generate();
        let cert = issue(&master, &authority);
        assert_eq!(cert.identity, master.id());
        assert_eq!(cert.authority_key, authority.public_key());
        assert_eq!(cert.created_at, 1_000);
        cert.verify().unwrap();
    }

    #[test]
    fn tampering_with_any_field_breaks_verification() {
        let master = StandaloneIdentity::generate();
        let authority = AuthorityKey::generate();
        let good = issue(&master, &authority);

        let mut other_authority = good.clone();
        other_authority.authority_key = AuthorityKey::generate().public_key();
        let mut other_identity = good.clone();
        other_identity.identity = StandaloneIdentity::generate().id();
        let mut other_time = good.clone();
        other_time.created_at += 1;
        let mut other_signature = good.clone();
        other_signature.signature[0] ^= 1;
        let mut no_signature = good.clone();
        no_signature.signature.clear();

        for (name, bad) in [
            ("authority_key", other_authority),
            ("identity", other_identity),
            ("created_at", other_time),
            ("signature", other_signature),
            ("empty signature", no_signature),
        ] {
            assert!(bad.verify().is_err(), "{name} adulterado foi aceito");
        }
    }

    #[test]
    fn the_authority_key_cannot_certify_itself() {
        // Quem tem só a autoridade não vira a identidade: a assinatura precisa ser da mestra.
        let master = StandaloneIdentity::generate();
        let authority = AuthorityKey::generate();
        let mut cert = issue(&master, &authority);
        cert.signature = authority
            .sign(&AuthorityCertificate::signing_payload(
                &cert.authority_key,
                cert.created_at,
            ))
            .unwrap();
        assert!(cert.verify().is_err());
    }

    #[test]
    fn a_signature_made_under_the_device_domain_is_refused() {
        let master = StandaloneIdentity::generate();
        let authority = AuthorityKey::generate();
        let mut cert = issue(&master, &authority);
        let payload = AuthorityCertificate::signing_payload(&cert.authority_key, cert.created_at);
        assert!(payload.starts_with(AUTHORITY_DOMAIN));
        // Mesmo corpo, domínio do certificado de device.
        let forged = [
            b"kin/device-cert/v3".as_slice(),
            &payload[AUTHORITY_DOMAIN.len()..],
        ]
        .concat();
        cert.signature = master.sign(&forged).unwrap();
        assert!(cert.verify().is_err());
    }

    #[test]
    fn certifying_the_same_key_at_another_time_changes_the_signature() {
        let master = StandaloneIdentity::generate();
        let authority = AuthorityKey::generate();
        let a = master
            .certify_authority(&authority.public_key(), 1)
            .unwrap();
        let b = master
            .certify_authority(&authority.public_key(), 2)
            .unwrap();
        assert_ne!(a.signature, b.signature);
        a.verify().unwrap();
        b.verify().unwrap();
    }

    // ---- serialização ----

    #[test]
    fn bytes_round_trip() {
        let master = StandaloneIdentity::generate();
        let cert = issue(&master, &AuthorityKey::generate());
        let back = AuthorityCertificate::from_bytes(&cert.to_bytes()).unwrap();
        assert_eq!(back, cert);
        back.verify().unwrap();
    }

    #[test]
    fn truncated_and_padded_bytes_are_refused() {
        let master = StandaloneIdentity::generate();
        let bytes = issue(&master, &AuthorityKey::generate()).to_bytes();
        for cut in [0, 1, bytes.len() / 2, bytes.len() - 1] {
            assert!(
                AuthorityCertificate::from_bytes(&bytes[..cut]).is_err(),
                "truncado em {cut} foi aceito"
            );
        }
        let mut padded = bytes;
        padded.push(0);
        assert!(AuthorityCertificate::from_bytes(&padded).is_err());
    }

    #[test]
    fn a_device_certificate_is_not_an_authority_certificate_and_vice_versa() {
        let master = StandaloneIdentity::generate();
        let device = DeviceKey::generate();
        let device_cert = master
            .authorize_device(
                &device.public_key(),
                &DeviceKey::generate().public_key(),
                &device.nostr_public_key(),
                1,
            )
            .unwrap();
        assert!(AuthorityCertificate::from_bytes(&device_cert.to_bytes()).is_err());
        let authority_cert = issue(&master, &AuthorityKey::generate());
        assert!(DeviceCertificate::from_bytes(&authority_cert.to_bytes()).is_err());
    }

    // ---- rotação ----

    #[test]
    fn rotating_the_authority_keeps_the_identity() {
        let master = StandaloneIdentity::generate();
        let old = issue(&master, &AuthorityKey::generate());
        let new = issue(&master, &AuthorityKey::generate());
        assert_eq!(old.identity, new.identity);
        assert_ne!(old.authority_key, new.authority_key);
        old.verify().unwrap();
        new.verify().unwrap();
    }

    // ---- chave de autoridade em repouso (ADR 0012) ----

    #[test]
    fn sealed_authority_key_loads_back_as_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authority.key");
        let authority = AuthorityKey::generate();
        authority.save_sealed(&path, PASS, KdfParams::MIN).unwrap();
        let loaded = AuthorityKey::load_with(&path, Some(PASS)).unwrap();
        assert_eq!(loaded.public_key(), authority.public_key());
        let sig = loaded.sign(b"msg").unwrap();
        assert!(authority.public_key().verify(b"msg", &sig));
    }

    #[test]
    fn sealed_authority_file_does_not_contain_the_key_in_the_clear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authority.key");
        let authority = AuthorityKey::generate();
        authority.save_sealed(&path, PASS, KdfParams::MIN).unwrap();
        let on_disk = std::fs::read(&path).unwrap();
        let plain = authority.to_bytes().unwrap();
        assert!(crate::is_sealed(&on_disk));
        assert!(!on_disk.windows(plain.len()).any(|w| w == plain.as_slice()));
    }

    #[test]
    fn sealed_authority_key_needs_the_right_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authority.key");
        AuthorityKey::generate()
            .save_sealed(&path, PASS, KdfParams::MIN)
            .unwrap();
        assert!(matches!(
            AuthorityKey::load_with(&path, None).err().unwrap(),
            Error::PassphraseRequired
        ));
        assert!(matches!(
            AuthorityKey::load_with(&path, Some(b"errada"))
                .err()
                .unwrap(),
            Error::WrongPassphraseOrCorrupt
        ));
    }

    #[test]
    fn a_plain_authority_file_is_refused_when_a_passphrase_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authority.key");
        let authority = AuthorityKey::generate();
        std::fs::write(&path, authority.to_bytes().unwrap()).unwrap();
        assert_eq!(
            AuthorityKey::load_with(&path, None).unwrap().public_key(),
            authority.public_key()
        );
        assert!(matches!(
            AuthorityKey::load_with(&path, Some(PASS)).err().unwrap(),
            Error::PlainFileWhereSealedExpected
        ));
    }

    #[test]
    fn protecting_a_plain_authority_file_keeps_the_key_and_refuses_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authority.key");
        let authority = AuthorityKey::generate();
        std::fs::write(&path, authority.to_bytes().unwrap()).unwrap();
        AuthorityKey::protect(&path, PASS, KdfParams::MIN).unwrap();
        assert_eq!(
            AuthorityKey::load_with(&path, Some(PASS))
                .unwrap()
                .public_key(),
            authority.public_key()
        );
        std::fs::write(&path, b"lixo qualquer").unwrap();
        assert!(AuthorityKey::protect(&path, PASS, KdfParams::MIN).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"lixo qualquer");
    }

    #[test]
    fn authority_key_bytes_that_are_not_ed25519_or_are_garbage_are_refused() {
        assert!(AuthorityKey::from_bytes(b"").is_err());
        assert!(AuthorityKey::from_bytes(b"lixo qualquer").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn sealed_authority_file_has_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("authority.key");
        AuthorityKey::generate()
            .save_sealed(&path, PASS, KdfParams::MIN)
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
