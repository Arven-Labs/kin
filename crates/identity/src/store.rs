use std::fs;
use std::io::Write;
use std::path::Path;

use zeroize::Zeroizing;

use crate::{Error, KdfParams};

pub(crate) fn write_secret(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)?;
    Ok(())
}

pub(crate) fn read_secret(path: &Path) -> Result<Vec<u8>, Error> {
    Ok(fs::read(path)?)
}

/// Grava `bytes` em `path` por arquivo temporário no mesmo diretório + `fsync` + `rename`, com modo 0600:
/// uma queda no meio deixa o arquivo antigo intacto (o `write_secret` trunca no lugar).
#[allow(dead_code)] // stub: sai quando a implementação entrar
pub(crate) fn write_secret_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let _ = (path, bytes);
    unimplemented!("stub: write_secret_atomic")
}

/// Lê um arquivo de chave. Cifrado exige `passphrase`; em claro (formato antigo) ignora-a.
#[allow(dead_code)] // stub: sai quando a implementação entrar
pub(crate) fn read_key(
    path: &Path,
    passphrase: Option<&[u8]>,
) -> Result<Zeroizing<Vec<u8>>, Error> {
    let _ = (path, passphrase);
    unimplemented!("stub: read_key")
}

/// Grava `bytes` cifrados com a senha, de forma atômica.
#[allow(dead_code)] // stub: sai quando a implementação entrar
pub(crate) fn write_key_sealed(
    path: &Path,
    bytes: &[u8],
    passphrase: &[u8],
    params: KdfParams,
) -> Result<(), Error> {
    let _ = (path, bytes, passphrase, params);
    unimplemented!("stub: write_key_sealed")
}

/// `true` se o arquivo de chave em `path` está cifrado.
pub fn key_file_is_sealed(path: &Path) -> Result<bool, Error> {
    let _ = path;
    unimplemented!("stub: key_file_is_sealed")
}

/// Migra um arquivo de chave **em claro** para o formato cifrado, no lugar e de forma atômica.
/// Recusa um arquivo já cifrado ([`Error::AlreadySealed`]) e senha vazia; em qualquer falha o original
/// fica intacto.
pub fn protect_key_file(path: &Path, passphrase: &[u8], params: KdfParams) -> Result<(), Error> {
    let _ = (path, passphrase, params);
    unimplemented!("stub: protect_key_file")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open;

    const PASS: &[u8] = b"senha de teste";
    const SECRET: &[u8] = b"conteudo-da-chave";

    fn plain_file(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("identity.key");
        write_secret(&path, SECRET).unwrap();
        path
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn protect_turns_a_plain_file_into_a_sealed_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = plain_file(dir.path());
        assert!(!key_file_is_sealed(&path).unwrap());
        protect_key_file(&path, PASS, KdfParams::MIN).unwrap();
        assert!(key_file_is_sealed(&path).unwrap());
        assert_eq!(&*open(&fs::read(&path).unwrap(), PASS).unwrap(), SECRET);
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn protect_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = plain_file(dir.path());
        protect_key_file(&path, PASS, KdfParams::MIN).unwrap();
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("identity.key")]);
    }

    #[test]
    fn protect_refuses_an_already_sealed_file_and_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = plain_file(dir.path());
        protect_key_file(&path, PASS, KdfParams::MIN).unwrap();
        let before = fs::read(&path).unwrap();
        let err = protect_key_file(&path, b"outra", KdfParams::MIN).unwrap_err();
        assert!(matches!(err, Error::AlreadySealed));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn protect_with_empty_passphrase_keeps_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = plain_file(dir.path());
        let err = protect_key_file(&path, b"", KdfParams::MIN).unwrap_err();
        assert!(matches!(err, Error::EmptyPassphrase));
        assert_eq!(fs::read(&path).unwrap(), SECRET);
    }

    #[test]
    fn protect_of_a_missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err =
            protect_key_file(&dir.path().join("nao-existe"), PASS, KdfParams::MIN).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_write_keeps_the_original_intact() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = plain_file(dir.path());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        // Como root o diretório continua gravável e o cenário não existe: não há o que provar.
        let writable = fs::File::create(dir.path().join("probe")).is_ok();
        let result = protect_key_file(&path, PASS, KdfParams::MIN);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        if !writable {
            assert!(result.is_err());
            assert_eq!(fs::read(&path).unwrap(), SECRET);
        }
    }

    #[test]
    fn read_key_requires_the_passphrase_only_for_sealed_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = plain_file(dir.path());
        assert_eq!(&*read_key(&path, None).unwrap(), SECRET);
        assert_eq!(&*read_key(&path, Some(PASS)).unwrap(), SECRET);
        protect_key_file(&path, PASS, KdfParams::MIN).unwrap();
        assert!(matches!(
            read_key(&path, None).unwrap_err(),
            Error::PassphraseRequired
        ));
        assert_eq!(&*read_key(&path, Some(PASS)).unwrap(), SECRET);
        assert!(matches!(
            read_key(&path, Some(b"errada")).unwrap_err(),
            Error::WrongPassphraseOrCorrupt
        ));
    }

    #[test]
    fn write_key_sealed_replaces_an_existing_file_with_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = plain_file(dir.path());
        write_key_sealed(&path, b"novo", PASS, KdfParams::MIN).unwrap();
        assert_eq!(&*read_key(&path, Some(PASS)).unwrap(), b"novo");
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
    }
}
