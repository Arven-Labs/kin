//! Chave de autoridade e seu certificado (P30 fatia 1b, opção D do ADR 0011).
//!
//! A mestra fica **fria** (offline, só assina) e certifica uma chave de **autoridade** que vive no
//! primário. Esta fatia é só custódia: nenhum byte de fio muda e a autoridade ainda não assina
//! devices nem revogações (fatias 2 e 3).

// STUB (etapa 1 do plano): só as assinaturas, para os testes compilarem e falharem.

use std::path::Path;

use libp2p_identity::PublicKey;

use crate::{Error, IdentityId, KdfParams};

/// Domínio de assinatura do certificado de autoridade. Diferente do de `DeviceCertificate`, para uma
/// assinatura de um nunca valer como a do outro.
pub(crate) const AUTHORITY_DOMAIN: &[u8] = b"kin/authority-cert/v1";

/// Chave de autoridade (Ed25519): faz o trabalho diário no primário enquanto a mestra fica fria.
pub struct AuthorityKey {
    _private: (),
}

impl AuthorityKey {
    pub fn generate() -> Self {
        todo!()
    }

    pub fn public_key(&self) -> PublicKey {
        todo!()
    }

    pub fn sign(&self, _message: &[u8]) -> Result<Vec<u8>, Error> {
        todo!()
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        todo!()
    }

    pub fn from_bytes(_bytes: &[u8]) -> Result<Self, Error> {
        todo!()
    }

    /// Grava a chave de autoridade **cifrada** com a senha (ADR 0012).
    pub fn save_sealed(
        &self,
        _path: &Path,
        _passphrase: &[u8],
        _params: KdfParams,
    ) -> Result<(), Error> {
        todo!()
    }

    /// Cifra no lugar um `authority.key` em claro (migração).
    pub fn protect(_path: &Path, _passphrase: &[u8], _params: KdfParams) -> Result<(), Error> {
        todo!()
    }

    /// Lê a chave de autoridade de um arquivo cifrado ou em claro (mesma política do ADR 0012).
    pub fn load_with(_path: &Path, _passphrase: Option<&[u8]>) -> Result<Self, Error> {
        todo!()
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
    pub(crate) fn signing_payload(_authority_key: &PublicKey, _created_at: u64) -> Vec<u8> {
        todo!()
    }

    /// Confere a assinatura da mestra (`identity`) sobre a chave de autoridade.
    pub fn verify(&self) -> Result<(), Error> {
        todo!()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        todo!()
    }

    /// Lê um certificado serializado por [`Self::to_bytes`]. Não confere a assinatura: use `verify`.
    pub fn from_bytes(_bytes: &[u8]) -> Result<Self, Error> {
        todo!()
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
