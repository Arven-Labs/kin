use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use libp2p_identity::{Keypair, PublicKey};

use crate::{
    DeviceCertificate, Error, IdentityId, IdentityProvider, KdfParams, NostrPublicKey, store,
};

const PAIR_PREFIX: &str = "kin://pair/";

/// Provedor padrão (D7): keypair Ed25519 gerado pelo Kin, sem conta e sem blockchain.
pub struct StandaloneIdentity {
    master: Keypair,
}

impl StandaloneIdentity {
    pub fn generate() -> Self {
        Self {
            master: Keypair::generate_ed25519(),
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let master = Keypair::from_protobuf_encoding(bytes)?;
        master
            .clone()
            .try_into_ed25519()
            .map_err(|_| Error::NotEd25519)?;
        Ok(Self { master })
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        Ok(self.master.to_protobuf_encoding()?)
    }

    /// A chave mestra como link de pareamento (`kin://pair/` + base64url sem padding), para levar a
    /// identidade a outro device (3.5, ADR 0006). **É o segredo inteiro**: quem vir o link é esta
    /// identidade. Não há cifra de transporte; quem chama avisa o usuário e não registra o link em log.
    pub fn to_pair_link(&self) -> Result<String, Error> {
        Ok(format!(
            "{PAIR_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(self.to_bytes()?)
        ))
    }

    /// Lê um link de pareamento. Aceita espaço e quebra de linha nas pontas (vem de arquivo ou stdin).
    /// Qualquer defeito vira [`Error::InvalidPairLink`], sem ecoar o conteúdo.
    pub fn from_pair_link(link: &str) -> Result<Self, Error> {
        let body = link
            .trim()
            .strip_prefix(PAIR_PREFIX)
            .ok_or(Error::InvalidPairLink)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| Error::InvalidPairLink)?;
        Self::from_bytes(&bytes).map_err(|_| Error::InvalidPairLink)
    }

    /// Grava a chave mestra em `path` (permissão 0600 em Unix). Não cifra em repouso (P13).
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        store::write_secret(path, &self.to_bytes()?)
    }

    pub fn load(path: &Path) -> Result<Self, Error> {
        Self::from_bytes(&store::read_secret(path)?)
    }

    /// Grava a chave mestra **cifrada** com a senha (P13, ADR 0012), de forma atômica e com modo 0600.
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

    /// Cifra no lugar um `identity.key` **em claro** (migração). Confere que o conteúdo é uma mestra
    /// válida antes de cifrar e que a cifra reabre depois de gravada; recusa arquivo já cifrado.
    /// O arquivo em claro antigo pode continuar recuperável no disco (ver `store::protect_with`).
    pub fn protect(path: &Path, passphrase: &[u8], params: KdfParams) -> Result<(), Error> {
        store::protect_with(path, passphrase, params, |bytes| {
            Self::from_bytes(bytes).map(|_| ())
        })
    }

    /// Certifica uma chave de autoridade (P30 fatia 1b): "esta chave é a minha autoridade". Deve rodar
    /// com a mestra **fria** (offline). `created_at` em segundos Unix, passado de fora.
    pub fn certify_authority(
        &self,
        _authority_key: &PublicKey,
        _created_at: u64,
    ) -> Result<crate::AuthorityCertificate, Error> {
        todo!()
    }

    /// Lê a chave mestra de um arquivo cifrado ou em claro (formato antigo). Cifrado sem senha:
    /// [`Error::PassphraseRequired`]; em claro **com** senha: [`Error::PlainFileWhereSealedExpected`].
    pub fn load_with(path: &Path, passphrase: Option<&[u8]>) -> Result<Self, Error> {
        Self::from_bytes(&store::read_key(path, passphrase)?)
    }
}

impl IdentityProvider for StandaloneIdentity {
    fn id(&self) -> IdentityId {
        IdentityId::new(self.master.public())
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self.master.sign(message)?)
    }

    fn authorize_device(
        &self,
        signing_key: &PublicKey,
        network_key: &PublicKey,
        nostr_key: &NostrPublicKey,
        created_at: u64,
    ) -> Result<DeviceCertificate, Error> {
        let payload =
            DeviceCertificate::signing_payload(signing_key, network_key, nostr_key, created_at);
        Ok(DeviceCertificate {
            identity: self.id(),
            signing_key: signing_key.clone(),
            network_key: network_key.clone(),
            nostr_key: *nostr_key,
            created_at,
            signature: self.master.sign(&payload)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASS: &[u8] = b"senha de teste";

    #[test]
    fn sealed_identity_loads_back_as_the_same_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        let identity = StandaloneIdentity::generate();
        identity.save_sealed(&path, PASS, KdfParams::MIN).unwrap();
        let loaded = StandaloneIdentity::load_with(&path, Some(PASS)).unwrap();
        assert_eq!(loaded.id(), identity.id());
        // Mesma chave: a assinatura de um verifica na pública do outro.
        let sig = loaded.sign(b"msg").unwrap();
        assert!(identity.id().public_key().verify(b"msg", &sig));
    }

    #[test]
    fn sealed_file_is_not_the_protobuf_of_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        let identity = StandaloneIdentity::generate();
        identity.save_sealed(&path, PASS, KdfParams::MIN).unwrap();
        let on_disk = std::fs::read(&path).unwrap();
        let plain = identity.to_bytes().unwrap();
        assert!(crate::is_sealed(&on_disk));
        assert!(!on_disk.windows(plain.len()).any(|w| w == plain.as_slice()));
    }

    #[test]
    fn sealed_identity_without_or_with_wrong_passphrase_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        StandaloneIdentity::generate()
            .save_sealed(&path, PASS, KdfParams::MIN)
            .unwrap();
        assert!(matches!(
            StandaloneIdentity::load_with(&path, None).err().unwrap(),
            Error::PassphraseRequired
        ));
        assert!(matches!(
            StandaloneIdentity::load_with(&path, Some(b"errada"))
                .err()
                .unwrap(),
            Error::WrongPassphraseOrCorrupt
        ));
    }

    #[test]
    fn legacy_plain_file_loads_only_without_a_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        let identity = StandaloneIdentity::generate();
        identity.save(&path).unwrap();
        assert_eq!(
            StandaloneIdentity::load_with(&path, None).unwrap().id(),
            identity.id()
        );
        // Com senha, um arquivo em claro é uma troca suspeita: não abre.
        assert!(matches!(
            StandaloneIdentity::load_with(&path, Some(PASS))
                .err()
                .unwrap(),
            Error::PlainFileWhereSealedExpected
        ));
    }

    #[test]
    fn protect_refuses_garbage_empty_and_already_sealed_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        for content in [&b""[..], &b"lixo qualquer"[..]] {
            std::fs::write(&path, content).unwrap();
            assert!(StandaloneIdentity::protect(&path, PASS, KdfParams::MIN).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), content, "original alterado");
        }
        StandaloneIdentity::generate()
            .save_sealed(&path, PASS, KdfParams::MIN)
            .unwrap();
        assert!(matches!(
            StandaloneIdentity::protect(&path, PASS, KdfParams::MIN).unwrap_err(),
            Error::AlreadySealed
        ));
    }

    #[test]
    fn protecting_a_legacy_file_keeps_the_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        let identity = StandaloneIdentity::generate();
        identity.save(&path).unwrap();
        StandaloneIdentity::protect(&path, PASS, KdfParams::MIN).unwrap();
        assert_eq!(
            StandaloneIdentity::load_with(&path, Some(PASS))
                .unwrap()
                .id(),
            identity.id()
        );
    }

    #[cfg(unix)]
    #[test]
    fn sealed_file_has_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        StandaloneIdentity::generate()
            .save_sealed(&path, PASS, KdfParams::MIN)
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
