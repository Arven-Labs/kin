use std::path::Path;

use zeroize::Zeroizing;

use crate::{Error, KdfParams};

/// Chave de 32 bytes dos bancos SQLite cifrados com SQLCipher (P13, ADR 0045).
///
/// Zera a memória ao sair de escopo e nunca aparece em `Debug`. Não implementa `Display`, `Clone` nem
/// `Serialize`: quem precisa dos bytes (só o ponto que aplica o `PRAGMA key`) chama [`Self::expose_secret`].
pub struct DbKey(Zeroizing<[u8; 32]>);

impl DbKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Os bytes da chave. Não os guarde nem os registre em log.
    pub fn expose_secret(&self) -> &[u8; 32] {
        &self.0
    }

    /// Chave nova, 32 bytes do gerador aleatório do sistema.
    pub fn generate() -> Self {
        unimplemented!("P13 fatia 3")
    }

    /// Grava a chave selada com a senha (formato do ADR 0012), atômico e 0600. **Falha se `path` já existe**:
    /// sobrescrever um `db.key` deixaria os bancos cifrados com a chave antiga sem como abrir.
    pub fn save_sealed(
        &self,
        _path: &Path,
        _passphrase: &[u8],
        _params: KdfParams,
    ) -> Result<(), Error> {
        unimplemented!("P13 fatia 3")
    }

    /// Lê e abre um `db.key` selado. Arquivo em claro, de outro tamanho ou com senha errada é erro.
    pub fn load_with(_path: &Path, _passphrase: &[u8]) -> Result<Self, Error> {
        unimplemented!("P13 fatia 3")
    }
}

impl std::fmt::Debug for DbKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DbKey(<redigida>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASS: &[u8] = b"senha de teste";

    fn path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("db.key")
    }

    #[test]
    fn generate_gives_a_different_nonzero_key_each_time() {
        let a = DbKey::generate();
        let b = DbKey::generate();
        assert_ne!(a.expose_secret(), b.expose_secret());
        assert_ne!(a.expose_secret(), &[0u8; 32]);
    }

    #[test]
    fn a_saved_key_comes_back_with_the_same_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let key = DbKey::generate();
        key.save_sealed(&path(&dir), PASS, KdfParams::MIN).unwrap();
        let back = DbKey::load_with(&path(&dir), PASS).unwrap();
        assert_eq!(back.expose_secret(), key.expose_secret());
    }

    #[test]
    fn the_file_is_sealed_and_does_not_hold_the_raw_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = DbKey::from_bytes([0x5a; 32]);
        key.save_sealed(&path(&dir), PASS, KdfParams::MIN).unwrap();
        assert!(crate::key_file_is_sealed(&path(&dir)).unwrap());
        let bytes = std::fs::read(path(&dir)).unwrap();
        assert!(!bytes.windows(32).any(|w| w == [0x5a; 32]));
    }

    #[test]
    fn a_wrong_passphrase_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        DbKey::generate()
            .save_sealed(&path(&dir), PASS, KdfParams::MIN)
            .unwrap();
        let err = DbKey::load_with(&path(&dir), b"outra senha").unwrap_err();
        assert!(matches!(err, Error::WrongPassphraseOrCorrupt), "{err:?}");
    }

    #[test]
    fn saving_over_an_existing_db_key_is_refused_and_keeps_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let first = DbKey::generate();
        first
            .save_sealed(&path(&dir), PASS, KdfParams::MIN)
            .unwrap();
        let before = std::fs::read(path(&dir)).unwrap();
        let err = DbKey::generate()
            .save_sealed(&path(&dir), PASS, KdfParams::MIN)
            .unwrap_err();
        assert!(
            matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::AlreadyExists),
            "{err:?}"
        );
        assert_eq!(std::fs::read(path(&dir)).unwrap(), before);
    }

    #[test]
    fn a_plain_32_byte_file_is_not_accepted_as_a_db_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(path(&dir), [7u8; 32]).unwrap();
        let err = DbKey::load_with(&path(&dir), PASS).unwrap_err();
        assert!(matches!(err, Error::InvalidSealedFile), "{err:?}");
    }

    #[test]
    fn a_sealed_payload_of_the_wrong_length_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let sealed = crate::seal(&[1u8; 31], PASS, KdfParams::MIN).unwrap();
        std::fs::write(path(&dir), sealed).unwrap();
        let err = DbKey::load_with(&path(&dir), PASS).unwrap_err();
        assert!(matches!(err, Error::InvalidSealedFile), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_0600_and_leaves_no_temporary_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        DbKey::generate()
            .save_sealed(&path(&dir), PASS, KdfParams::MIN)
            .unwrap();
        let mode = std::fs::metadata(path(&dir)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("db.key")]);
    }
}
