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
pub(crate) fn write_secret_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    write_atomic(path, bytes, true)
}

/// Como [`write_secret_atomic`], mas **falha com `AlreadyExists` se `path` existe**, inclusive se
/// aparecer durante a gravação: publica com `hard_link` (que não substitui) em vez de `rename`.
/// Exige sistema de arquivos com hard link.
pub(crate) fn write_secret_new(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    write_atomic(path, bytes, false)
}

fn write_atomic(path: &Path, bytes: &[u8], replace: bool) -> Result<(), Error> {
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "caminho sem nome de arquivo",
        )
    })?;
    // Nome único por tentativa: dois processos gravando juntos não pisam no temp um do outro. Uma
    // sobra de queda é só texto cifrado e não é apagada às cegas.
    let mut file = None;
    let mut tmp = path.to_path_buf();
    for _ in 0..8 {
        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(name);
        tmp_name.push(format!(".{:016x}.tmp", secp256k1::rand::random::<u64>()));
        tmp = path.with_file_name(tmp_name);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&tmp) {
            Ok(opened) => {
                file = Some(opened);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut file = file.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "sem nome de arquivo temporário livre",
        )
    })?;
    let mut write = || -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        if replace {
            fs::rename(&tmp, path)
        } else {
            fs::hard_link(&tmp, path)?;
            fs::remove_file(&tmp)
        }
    };
    if let Err(e) = write() {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    // Persiste a entrada do diretório (o rename); falhar aqui não desfaz o que já foi gravado.
    #[cfg(unix)]
    {
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if let Ok(dir) = fs::File::open(dir) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// Teto de leitura de um arquivo de chave (o maior, o `device.key`, tem ~150 bytes): um arquivo
/// gigante ou `/dev/zero` no caminho da chave não estoura a memória.
const KEY_FILE_LIMIT: u64 = 64 * 1024;

pub(crate) fn read_limited(path: &Path) -> Result<Zeroizing<Vec<u8>>, Error> {
    use std::io::Read as _;
    let mut bytes = Zeroizing::new(Vec::new());
    fs::File::open(path)?
        .take(KEY_FILE_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > KEY_FILE_LIMIT {
        return Err(Error::KeyFileTooLarge);
    }
    Ok(bytes)
}

/// Lê um arquivo de chave. Cifrado exige `passphrase`. Em claro (formato antigo) só abre **sem** senha:
/// com senha informada é anômalo (troca de arquivo por quem tem escrita no diretório) e é recusado.
pub(crate) fn read_key(
    path: &Path,
    passphrase: Option<&[u8]>,
) -> Result<Zeroizing<Vec<u8>>, Error> {
    let bytes = read_limited(path)?;
    if crate::is_sealed(&bytes) {
        return crate::open(&bytes, passphrase.ok_or(Error::PassphraseRequired)?);
    }
    match passphrase {
        None => Ok(bytes),
        Some(_) => Err(Error::PlainFileWhereSealedExpected),
    }
}

/// Grava `bytes` cifrados com a senha, de forma atômica.
pub(crate) fn write_key_sealed(
    path: &Path,
    bytes: &[u8],
    passphrase: &[u8],
    params: KdfParams,
) -> Result<(), Error> {
    write_secret_atomic(path, &crate::seal(bytes, passphrase, params)?)
}

/// Como [`write_key_sealed`], mas falha se `path` já existe (ver [`write_secret_new`]).
pub(crate) fn write_key_sealed_new(
    path: &Path,
    bytes: &[u8],
    passphrase: &[u8],
    params: KdfParams,
) -> Result<(), Error> {
    write_secret_new(path, &crate::seal(bytes, passphrase, params)?)
}

/// `true` se o arquivo de chave em `path` está cifrado.
pub fn key_file_is_sealed(path: &Path) -> Result<bool, Error> {
    use std::io::Read as _;
    // Só o magic: o arquivo em claro é um segredo e não precisa ser carregado para isto.
    let mut head = Vec::with_capacity(4);
    fs::File::open(path)?.take(4).read_to_end(&mut head)?;
    Ok(crate::is_sealed(&head))
}

/// Migra um arquivo de chave **em claro** para o formato cifrado, no lugar e de forma atômica.
/// `validate` confere o conteúdo antes de cifrar (um arquivo vazio, truncado ou de outro tipo não é
/// selado). Recusa um arquivo já cifrado ([`Error::AlreadySealed`]) e senha vazia; em qualquer falha o
/// original fica intacto.
///
/// **Limite**: o `rename` troca o inode; os blocos do arquivo em claro antigo seguem no disco (SSD,
/// journal, snapshot, backup) até serem reutilizados. Uma chave que já esteve em claro deve ser
/// considerada exposta se o disco ou o backup não for confiável.
pub(crate) fn protect_with(
    path: &Path,
    passphrase: &[u8],
    params: KdfParams,
    validate: impl Fn(&[u8]) -> Result<(), Error>,
) -> Result<(), Error> {
    let plain = read_limited(path)?;
    if crate::is_sealed(&plain) {
        return Err(Error::AlreadySealed);
    }
    validate(&plain)?;
    let sealed = crate::seal(&plain, passphrase, params)?;
    // Reabre antes de substituir o original: uma cifra que não volta nunca chega ao disco.
    if *crate::open(&sealed, passphrase)? != *plain {
        return Err(Error::WrongPassphraseOrCorrupt);
    }
    write_secret_atomic(path, &sealed)
}

/// Em que estado está um arquivo de chave selado numa troca de senha (P13, `kin passwd`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RekeyState {
    /// Abre com a senha velha: ainda falta trocar.
    Old,
    /// Já abre com a senha nova (troca anterior, interrompida ou repetida): nada a fazer.
    New,
}

/// Abre os bytes selados com a senha nova (`New`) ou, se não abre, com a velha (`Old`). A nova vem
/// primeiro: é o que torna a troca retomável. Se nenhuma abre: `WrongPassphraseOrCorrupt`.
fn rekey_open(
    sealed: &[u8],
    old: &[u8],
    new: &[u8],
) -> Result<(RekeyState, Zeroizing<Vec<u8>>), Error> {
    if !crate::is_sealed(sealed) {
        return Err(Error::PlainFileWhereSealedExpected);
    }
    if let Ok(plain) = crate::open(sealed, new) {
        return Ok((RekeyState::New, plain));
    }
    Ok((RekeyState::Old, crate::open(sealed, old)?))
}

/// Diz se o arquivo selado em `path` já está na senha nova ou ainda na velha, sem gravar nada. Serve
/// para conferir **todos** os arquivos de um diretório antes de trocar qualquer um.
pub fn rekey_state(path: &Path, old: &[u8], new: &[u8]) -> Result<RekeyState, Error> {
    Ok(rekey_open(&read_limited(path)?, old, new)?.0)
}

/// Troca a senha de um arquivo de chave **selado**, no lugar e de forma atômica (P13, `kin passwd`).
/// Idempotente: se já abre com a senha nova, não grava nada e devolve [`RekeyState::New`]. Senão abre
/// com a velha, sela com a nova (salt e nonce novos), **reabre** antes de substituir e troca por
/// `rename`; em qualquer falha o original fica intacto. Recusa senha nova vazia e arquivo em claro.
///
/// **Limite**: os blocos do arquivo antigo (senha velha) podem seguir no disco, como no `protect_with`.
pub fn rekey_file(
    path: &Path,
    old: &[u8],
    new: &[u8],
    params: KdfParams,
) -> Result<RekeyState, Error> {
    if new.is_empty() {
        return Err(Error::EmptyPassphrase);
    }
    let (state, plain) = rekey_open(&read_limited(path)?, old, new)?;
    if state == RekeyState::New {
        return Ok(state);
    }
    let sealed = crate::seal(&plain, new, params)?;
    if *crate::open(&sealed, new)? != *plain {
        return Err(Error::WrongPassphraseOrCorrupt);
    }
    write_secret_atomic(path, &sealed)?;
    Ok(RekeyState::Old)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open;

    const PASS: &[u8] = b"senha de teste";
    const SECRET: &[u8] = b"conteudo-da-chave";

    fn ok(_: &[u8]) -> Result<(), Error> {
        Ok(())
    }

    fn reject(_: &[u8]) -> Result<(), Error> {
        Err(Error::InvalidDeviceKey)
    }

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
        protect_with(&path, PASS, KdfParams::MIN, ok).unwrap();
        assert!(key_file_is_sealed(&path).unwrap());
        assert_eq!(&*open(&fs::read(&path).unwrap(), PASS).unwrap(), SECRET);
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn protect_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = plain_file(dir.path());
        protect_with(&path, PASS, KdfParams::MIN, ok).unwrap();
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
        protect_with(&path, PASS, KdfParams::MIN, ok).unwrap();
        let before = fs::read(&path).unwrap();
        let err = protect_with(&path, b"outra", KdfParams::MIN, ok).unwrap_err();
        assert!(matches!(err, Error::AlreadySealed));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn protect_with_empty_passphrase_keeps_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = plain_file(dir.path());
        let err = protect_with(&path, b"", KdfParams::MIN, ok).unwrap_err();
        assert!(matches!(err, Error::EmptyPassphrase));
        assert_eq!(fs::read(&path).unwrap(), SECRET);
    }

    #[test]
    fn protect_of_a_missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err =
            protect_with(&dir.path().join("nao-existe"), PASS, KdfParams::MIN, ok).unwrap_err();
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
        let result = protect_with(&path, PASS, KdfParams::MIN, ok);
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
        // Com senha informada, um arquivo em claro é anômalo (troca de arquivo): não abre.
        assert!(matches!(
            read_key(&path, Some(PASS)).unwrap_err(),
            Error::PlainFileWhereSealedExpected
        ));
        protect_with(&path, PASS, KdfParams::MIN, ok).unwrap();
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
    fn protect_does_not_seal_content_the_validator_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let path = plain_file(dir.path());
        let err = protect_with(&path, PASS, KdfParams::MIN, reject).unwrap_err();
        assert!(matches!(err, Error::InvalidDeviceKey));
        assert_eq!(fs::read(&path).unwrap(), SECRET);
    }

    // ---- troca de senha (P13, `kin passwd`) ----

    const NEW_PASS: &[u8] = b"outra senha";

    fn sealed_file(dir: &Path) -> std::path::PathBuf {
        let path = plain_file(dir);
        protect_with(&path, PASS, KdfParams::MIN, ok).unwrap();
        path
    }

    #[test]
    fn rekey_moves_the_content_to_the_new_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let path = sealed_file(dir.path());
        let state = rekey_file(&path, PASS, NEW_PASS, KdfParams::MIN).unwrap();
        assert_eq!(state, RekeyState::Old);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(&*open(&bytes, NEW_PASS).unwrap(), SECRET);
        assert!(matches!(
            open(&bytes, PASS).unwrap_err(),
            Error::WrongPassphraseOrCorrupt
        ));
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn rekey_uses_a_fresh_salt_and_nonce() {
        let dir = tempfile::tempdir().unwrap();
        let path = sealed_file(dir.path());
        let before = fs::read(&path).unwrap();
        rekey_file(&path, PASS, NEW_PASS, KdfParams::MIN).unwrap();
        let after = fs::read(&path).unwrap();
        // Cabeçalho (magic, versão, custo) igual; salt e nonce (bytes 14..54) mudam.
        assert_eq!(before[..14], after[..14]);
        assert_ne!(before[14..54], after[14..54]);
    }

    #[test]
    fn rekey_repeated_is_a_no_op_that_reports_new() {
        let dir = tempfile::tempdir().unwrap();
        let path = sealed_file(dir.path());
        rekey_file(&path, PASS, NEW_PASS, KdfParams::MIN).unwrap();
        let after_first = fs::read(&path).unwrap();
        let state = rekey_file(&path, PASS, NEW_PASS, KdfParams::MIN).unwrap();
        assert_eq!(state, RekeyState::New);
        assert_eq!(fs::read(&path).unwrap(), after_first);
    }

    #[test]
    fn rekey_with_a_wrong_old_passphrase_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = sealed_file(dir.path());
        let before = fs::read(&path).unwrap();
        let err = rekey_file(&path, b"errada", NEW_PASS, KdfParams::MIN).unwrap_err();
        assert!(matches!(err, Error::WrongPassphraseOrCorrupt));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn rekey_refuses_a_plain_file_and_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = plain_file(dir.path());
        let err = rekey_file(&path, PASS, NEW_PASS, KdfParams::MIN).unwrap_err();
        assert!(matches!(err, Error::PlainFileWhereSealedExpected));
        assert_eq!(fs::read(&path).unwrap(), SECRET);
    }

    #[test]
    fn rekey_to_an_empty_passphrase_keeps_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = sealed_file(dir.path());
        let before = fs::read(&path).unwrap();
        let err = rekey_file(&path, PASS, b"", KdfParams::MIN).unwrap_err();
        assert!(matches!(err, Error::EmptyPassphrase));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn rekey_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = sealed_file(dir.path());
        rekey_file(&path, PASS, NEW_PASS, KdfParams::MIN).unwrap();
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("identity.key")]);
    }

    #[test]
    fn rekey_state_tells_old_new_and_neither_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = sealed_file(dir.path());
        let before = fs::read(&path).unwrap();
        assert_eq!(rekey_state(&path, PASS, NEW_PASS).unwrap(), RekeyState::Old);
        assert!(matches!(
            rekey_state(&path, b"errada", NEW_PASS).unwrap_err(),
            Error::WrongPassphraseOrCorrupt
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        rekey_file(&path, PASS, NEW_PASS, KdfParams::MIN).unwrap();
        assert_eq!(rekey_state(&path, PASS, NEW_PASS).unwrap(), RekeyState::New);
    }

    #[test]
    fn a_file_at_the_old_fixed_temp_name_is_left_alone() {
        // O temp tem nome único: gravar não apaga nem reaproveita o arquivo de outro processo.
        let dir = tempfile::tempdir().unwrap();
        let path = plain_file(dir.path());
        let other = dir.path().join(".identity.key.tmp");
        fs::write(&other, b"de outro processo").unwrap();
        write_key_sealed(&path, b"novo", PASS, KdfParams::MIN).unwrap();
        assert_eq!(fs::read(&other).unwrap(), b"de outro processo");
    }

    #[test]
    fn oversized_key_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        fs::write(&path, vec![0u8; 70 * 1024]).unwrap();
        assert!(matches!(
            read_key(&path, None).unwrap_err(),
            Error::KeyFileTooLarge
        ));
        assert!(matches!(
            protect_with(&path, PASS, KdfParams::MIN, ok).unwrap_err(),
            Error::KeyFileTooLarge
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
