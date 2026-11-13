//! Custódia das chaves no disco (P13, P30 fatia 1a, ADR 0012): decide se `identity.key` e `device.key`
//! ficam cifrados, de onde vem a senha e como migrar um diretório antigo em claro.

use std::path::{Path, PathBuf};

use kin_identity::{DeviceKey, KdfParams, StandaloneIdentity};
use zeroize::Zeroizing;

/// O que o usuário pediu na linha de comando (e `KIN_PASSPHRASE_FILE`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KeyOptions {
    pub passphrase_file: Option<PathBuf>,
    pub no_passphrase: bool,
    pub protect: bool,
}

/// Estado dos arquivos de chave que **existem** no diretório.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesState {
    /// Nenhum dos dois existe: primeira execução.
    New,
    Sealed,
    Plain,
    /// Um cifrado e outro em claro.
    Mixed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    File(PathBuf),
    /// Prompt sem eco no terminal; `confirm` pede a senha duas vezes (criação e migração).
    Prompt {
        confirm: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlainReason {
    /// `--no-passphrase`: escolha explícita.
    Explicit,
    /// Diretório antigo em claro e nenhuma senha informada: segue abrindo, com aviso.
    Legacy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Plain(PlainReason),
    Sealed { source: Source, protect: bool },
}

/// Decisão pura: dado o pedido, o estado dos arquivos e se há terminal, o que fazer (ou por que recusar).
pub fn decide(opts: &KeyOptions, state: FilesState, tty: bool) -> Result<Access, String> {
    if opts.no_passphrase {
        if opts.passphrase_file.is_some() {
            return Err("--no-passphrase e --passphrase-file são excludentes".into());
        }
        if opts.protect {
            return Err("--protect exige senha: não combina com --no-passphrase".into());
        }
        return match state {
            FilesState::Sealed | FilesState::Mixed => {
                Err("as chaves estão cifradas: informe a senha (sem --no-passphrase)".into())
            }
            FilesState::New | FilesState::Plain => Ok(Access::Plain(PlainReason::Explicit)),
        };
    }
    match (state, opts.protect) {
        (FilesState::New | FilesState::Sealed, true) => {
            return Err("--protect: não há chave em claro para cifrar neste --data-dir".into());
        }
        (FilesState::Plain, false) if opts.passphrase_file.is_none() => {
            return Ok(Access::Plain(PlainReason::Legacy));
        }
        (FilesState::Plain | FilesState::Mixed, false) => {
            return Err(
                "há chave em claro neste --data-dir: rode uma vez com --protect para cifrá-la"
                    .into(),
            );
        }
        _ => {}
    }
    let source = match (&opts.passphrase_file, tty) {
        (Some(path), _) => Source::File(path.clone()),
        (None, true) => Source::Prompt {
            confirm: state == FilesState::New || opts.protect,
        },
        (None, false) => {
            return Err(
                "sem terminal e sem senha: use --passphrase-file ARQUIVO (ou KIN_PASSPHRASE_FILE), ou --no-passphrase para chaves em claro"
                    .into(),
            );
        }
    };
    Ok(Access::Sealed {
        source,
        protect: opts.protect,
    })
}

/// Estado dos arquivos de chave em `dir` (só os que existem).
pub fn files_state(dir: &Path) -> Result<FilesState, Box<dyn std::error::Error>> {
    let mut sealed = Vec::new();
    for name in ["identity.key", "device.key"] {
        let path = dir.join(name);
        if path.exists() {
            sealed.push(kin_identity::key_file_is_sealed(&path)?);
        }
    }
    if sealed.is_empty() {
        return Ok(FilesState::New);
    }
    Ok(if sealed.iter().all(|s| *s) {
        FilesState::Sealed
    } else if sealed.iter().any(|s| *s) {
        FilesState::Mixed
    } else {
        FilesState::Plain
    })
}

/// Lê a senha de um arquivo: uma linha, até 4 KiB, UTF-8, sem a quebra de linha final, normalizada em
/// NFKC. No Unix recusa arquivo legível por grupo ou outros. Os erros nunca citam o conteúdo.
pub fn read_passphrase_file(path: &Path) -> Result<Zeroizing<Vec<u8>>, String> {
    use std::io::Read as _;
    const LIMIT: u64 = 4096;
    let shown = path.display();
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("arquivo de senha {shown}: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = file
            .metadata()
            .map_err(|e| format!("arquivo de senha {shown}: {e}"))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "arquivo de senha {shown} legível por outros usuários (modo {:o}): use chmod 600",
                mode & 0o777
            ));
        }
    }
    let mut raw = Zeroizing::new(Vec::new());
    (&mut file)
        .take(LIMIT + 1)
        .read_to_end(&mut raw)
        .map_err(|e| format!("arquivo de senha {shown}: {e}"))?;
    if raw.len() as u64 > LIMIT {
        return Err(format!("arquivo de senha {shown} maior que {LIMIT} bytes"));
    }
    let body: &[u8] = if let Some(rest) = raw.strip_suffix(b"\r\n") {
        rest
    } else if let Some(rest) = raw.strip_suffix(b"\n") {
        rest
    } else {
        &raw
    };
    let text =
        std::str::from_utf8(body).map_err(|_| format!("arquivo de senha {shown} não é UTF-8"))?;
    if text.is_empty() {
        return Err(format!("arquivo de senha {shown} está vazio"));
    }
    Ok(kin_identity::normalize_passphrase(text))
}

/// Carrega (ou cria) `identity.key` e `device.key` em `dir`.
///
/// - `pair_text`: link `kin://pair/...` já lido (ADR 0006); só vale com `identity.key` e `mls.sqlite`
///   ausentes.
/// - `passphrase`: `Some` cifra e abre com senha; `None` trabalha em claro (legado ou `--no-passphrase`).
/// - `protect`: antes de abrir, cifra no lugar os arquivos que ainda estão em claro.
pub fn load(
    dir: &Path,
    pair_text: Option<&str>,
    passphrase: Option<&[u8]>,
    protect: bool,
    params: KdfParams,
) -> Result<(StandaloneIdentity, DeviceKey), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    let identity_path = dir.join("identity.key");
    let device_path = dir.join("device.key");
    if let Some(text) = pair_text {
        if identity_path.exists() {
            return Err(
                "já existe identity.key neste --data-dir: --pair não sobrescreve uma identidade"
                    .into(),
            );
        }
        // Estado MLS e outbox pertencem à identidade que os criou; com outra identidade, o `Chat`
        // abriria conversas que não são dela.
        if dir.join("mls.sqlite").exists() {
            return Err(
                "este --data-dir já tem estado (mls.sqlite) de outra identidade: use um diretório novo"
                    .into(),
            );
        }
        let imported = StandaloneIdentity::from_pair_link(text)?;
        match passphrase {
            Some(pass) => imported.save_sealed(&identity_path, pass, params)?,
            None => imported.save(&identity_path)?,
        }
    }
    if protect {
        let pass = passphrase.ok_or("--protect exige uma senha")?;
        if identity_path.exists() && !kin_identity::key_file_is_sealed(&identity_path)? {
            StandaloneIdentity::protect(&identity_path, pass, params)?;
        }
        if device_path.exists() && !kin_identity::key_file_is_sealed(&device_path)? {
            DeviceKey::protect(&device_path, pass, params)?;
        }
    }
    let identity = if identity_path.exists() {
        StandaloneIdentity::load_with(&identity_path, passphrase)?
    } else {
        let identity = StandaloneIdentity::generate();
        match passphrase {
            Some(pass) => identity.save_sealed(&identity_path, pass, params)?,
            None => identity.save(&identity_path)?,
        }
        identity
    };
    let device = if device_path.exists() {
        DeviceKey::load_with(&device_path, passphrase)?
    } else {
        let device = DeviceKey::generate();
        match passphrase {
            Some(pass) => device.save_sealed(&device_path, pass, params)?,
            None => crate::write_private(&device_path, &device.to_bytes()?)?,
        }
        device
    };
    Ok((identity, device))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_identity::{Error, IdentityProvider, is_sealed};

    const PASS: &[u8] = b"senha de teste";

    fn opts(file: Option<&str>, no_passphrase: bool, protect: bool) -> KeyOptions {
        KeyOptions {
            passphrase_file: file.map(PathBuf::from),
            no_passphrase,
            protect,
        }
    }

    fn file_source() -> Source {
        Source::File(PathBuf::from("/s"))
    }

    // ---- decide ----

    #[test]
    fn new_dir_with_a_passphrase_file_creates_sealed_keys() {
        let a = decide(&opts(Some("/s"), false, false), FilesState::New, false).unwrap();
        assert_eq!(
            a,
            Access::Sealed {
                source: file_source(),
                protect: false
            }
        );
    }

    #[test]
    fn new_dir_on_a_terminal_prompts_twice() {
        let a = decide(&opts(None, false, false), FilesState::New, true).unwrap();
        assert_eq!(
            a,
            Access::Sealed {
                source: Source::Prompt { confirm: true },
                protect: false
            }
        );
    }

    #[test]
    fn new_dir_without_source_and_without_terminal_is_refused() {
        assert!(decide(&opts(None, false, false), FilesState::New, false).is_err());
    }

    #[test]
    fn no_passphrase_on_a_new_dir_is_explicit_plain() {
        let a = decide(&opts(None, true, false), FilesState::New, false).unwrap();
        assert_eq!(a, Access::Plain(PlainReason::Explicit));
    }

    #[test]
    fn sealed_dir_prompts_once_on_a_terminal() {
        let a = decide(&opts(None, false, false), FilesState::Sealed, true).unwrap();
        assert_eq!(
            a,
            Access::Sealed {
                source: Source::Prompt { confirm: false },
                protect: false
            }
        );
    }

    #[test]
    fn sealed_dir_without_source_and_without_terminal_is_refused() {
        assert!(decide(&opts(None, false, false), FilesState::Sealed, false).is_err());
    }

    #[test]
    fn no_passphrase_cannot_open_sealed_or_mixed_dirs() {
        for state in [FilesState::Sealed, FilesState::Mixed] {
            assert!(decide(&opts(None, true, false), state, true).is_err());
        }
    }

    #[test]
    fn legacy_plain_dir_without_a_passphrase_opens_with_a_warning() {
        for tty in [true, false] {
            let a = decide(&opts(None, false, false), FilesState::Plain, tty).unwrap();
            assert_eq!(a, Access::Plain(PlainReason::Legacy));
        }
    }

    #[test]
    fn plain_dir_with_a_passphrase_requires_protect() {
        assert!(decide(&opts(Some("/s"), false, false), FilesState::Plain, false).is_err());
        let a = decide(&opts(Some("/s"), false, true), FilesState::Plain, false).unwrap();
        assert_eq!(
            a,
            Access::Sealed {
                source: file_source(),
                protect: true
            }
        );
    }

    #[test]
    fn protect_on_a_terminal_prompts_twice() {
        let a = decide(&opts(None, false, true), FilesState::Plain, true).unwrap();
        assert_eq!(
            a,
            Access::Sealed {
                source: Source::Prompt { confirm: true },
                protect: true
            }
        );
    }

    #[test]
    fn protect_without_source_and_without_terminal_is_refused() {
        assert!(decide(&opts(None, false, true), FilesState::Plain, false).is_err());
    }

    #[test]
    fn mixed_dir_needs_protect() {
        assert!(decide(&opts(Some("/s"), false, false), FilesState::Mixed, false).is_err());
        assert!(decide(&opts(None, false, false), FilesState::Mixed, true).is_err());
        let a = decide(&opts(Some("/s"), false, true), FilesState::Mixed, false).unwrap();
        assert_eq!(
            a,
            Access::Sealed {
                source: file_source(),
                protect: true
            }
        );
    }

    #[test]
    fn protect_with_nothing_to_protect_is_refused() {
        assert!(decide(&opts(Some("/s"), false, true), FilesState::New, false).is_err());
        assert!(decide(&opts(Some("/s"), false, true), FilesState::Sealed, false).is_err());
    }

    #[test]
    fn conflicting_flags_are_refused() {
        assert!(decide(&opts(Some("/s"), true, false), FilesState::New, false).is_err());
        assert!(decide(&opts(None, true, true), FilesState::Plain, false).is_err());
    }

    // ---- read_passphrase_file ----

    fn passphrase_file(dir: &Path, content: &[u8], mode: u32) -> PathBuf {
        let path = dir.join("pass");
        std::fs::write(&path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let _ = mode;
        path
    }

    #[test]
    fn passphrase_file_loses_only_the_final_newline() {
        let dir = tempfile::tempdir().unwrap();
        for (raw, want) in [
            (&b"abc def\n"[..], &b"abc def"[..]),
            (b"abc def\r\n", b"abc def"),
            (b"abc def", b"abc def"),
            (b" abc \n", b" abc "),
        ] {
            let path = passphrase_file(dir.path(), raw, 0o600);
            assert_eq!(&*read_passphrase_file(&path).unwrap(), want);
        }
    }

    #[test]
    fn passphrase_file_is_normalized_in_nfkc() {
        let dir = tempfile::tempdir().unwrap();
        let path = passphrase_file(dir.path(), "cafe\u{301}\n".as_bytes(), 0o600);
        assert_eq!(
            &*read_passphrase_file(&path).unwrap(),
            "caf\u{e9}".as_bytes()
        );
    }

    #[test]
    fn empty_oversized_and_non_utf8_passphrase_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for content in [&b""[..], &b"\n"[..], &[0xff, 0xfe, 0x41][..]] {
            let path = passphrase_file(dir.path(), content, 0o600);
            assert!(read_passphrase_file(&path).is_err(), "{content:?}");
        }
        let path = passphrase_file(dir.path(), &vec![b'a'; 4097], 0o600);
        assert!(read_passphrase_file(&path).is_err());
    }

    #[test]
    fn missing_passphrase_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_passphrase_file(&dir.path().join("nao-existe")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn passphrase_file_readable_by_others_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [0o644, 0o640, 0o604] {
            let path = passphrase_file(dir.path(), b"senha\n", mode);
            assert!(read_passphrase_file(&path).is_err(), "modo {mode:o}");
        }
    }

    // ---- files_state e load ----

    fn id_path(dir: &Path) -> PathBuf {
        dir.join("identity.key")
    }

    fn dev_path(dir: &Path) -> PathBuf {
        dir.join("device.key")
    }

    fn make_plain(dir: &Path) -> (kin_identity::IdentityId, kin_identity::PeerId) {
        let identity = StandaloneIdentity::generate();
        identity.save(&id_path(dir)).unwrap();
        let device = DeviceKey::generate();
        std::fs::write(dev_path(dir), device.to_bytes().unwrap()).unwrap();
        (identity.id(), device.peer_id())
    }

    #[test]
    fn files_state_reads_only_what_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(files_state(dir.path()).unwrap(), FilesState::New);
        StandaloneIdentity::generate()
            .save_sealed(&id_path(dir.path()), PASS, KdfParams::MIN)
            .unwrap();
        assert_eq!(files_state(dir.path()).unwrap(), FilesState::Sealed);
        std::fs::write(
            dev_path(dir.path()),
            DeviceKey::generate().to_bytes().unwrap(),
        )
        .unwrap();
        assert_eq!(files_state(dir.path()).unwrap(), FilesState::Mixed);
        DeviceKey::protect(&dev_path(dir.path()), PASS, KdfParams::MIN).unwrap();
        assert_eq!(files_state(dir.path()).unwrap(), FilesState::Sealed);
        let plain = tempfile::tempdir().unwrap();
        make_plain(plain.path());
        assert_eq!(files_state(plain.path()).unwrap(), FilesState::Plain);
    }

    #[test]
    fn load_creates_sealed_keys_that_reopen_with_the_same_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let (id, dev) = load(dir.path(), None, Some(PASS), false, KdfParams::MIN).unwrap();
        assert!(is_sealed(&std::fs::read(id_path(dir.path())).unwrap()));
        assert!(is_sealed(&std::fs::read(dev_path(dir.path())).unwrap()));
        let (id2, dev2) = load(dir.path(), None, Some(PASS), false, KdfParams::MIN).unwrap();
        assert_eq!(id.id(), id2.id());
        assert_eq!(dev.peer_id(), dev2.peer_id());
    }

    #[test]
    fn load_with_the_wrong_or_no_passphrase_fails_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        load(dir.path(), None, Some(PASS), false, KdfParams::MIN).unwrap();
        let before = (
            std::fs::read(id_path(dir.path())).unwrap(),
            std::fs::read(dev_path(dir.path())).unwrap(),
        );
        assert!(load(dir.path(), None, Some(b"errada"), false, KdfParams::MIN).is_err());
        assert!(load(dir.path(), None, None, false, KdfParams::MIN).is_err());
        let after = (
            std::fs::read(id_path(dir.path())).unwrap(),
            std::fs::read(dev_path(dir.path())).unwrap(),
        );
        assert_eq!(before, after);
    }

    #[test]
    fn load_without_a_passphrase_keeps_the_plain_format() {
        let dir = tempfile::tempdir().unwrap();
        let (id, _) = load(dir.path(), None, None, false, KdfParams::MIN).unwrap();
        assert!(!is_sealed(&std::fs::read(id_path(dir.path())).unwrap()));
        assert!(!is_sealed(&std::fs::read(dev_path(dir.path())).unwrap()));
        let (id2, _) = load(dir.path(), None, None, false, KdfParams::MIN).unwrap();
        assert_eq!(id.id(), id2.id());
    }

    #[test]
    fn plain_dir_with_a_passphrase_and_no_protect_is_refused_untouched() {
        let dir = tempfile::tempdir().unwrap();
        make_plain(dir.path());
        let before = std::fs::read(id_path(dir.path())).unwrap();
        let err = load(dir.path(), None, Some(PASS), false, KdfParams::MIN)
            .err()
            .unwrap();
        assert!(matches!(
            err.downcast_ref::<Error>(),
            Some(Error::PlainFileWhereSealedExpected)
        ));
        assert_eq!(std::fs::read(id_path(dir.path())).unwrap(), before);
    }

    #[test]
    fn protect_seals_both_files_and_keeps_the_identity_and_device() {
        let dir = tempfile::tempdir().unwrap();
        let (id, peer) = make_plain(dir.path());
        let (loaded, device) = load(dir.path(), None, Some(PASS), true, KdfParams::MIN).unwrap();
        assert_eq!(loaded.id(), id);
        assert_eq!(device.peer_id(), peer);
        assert!(is_sealed(&std::fs::read(id_path(dir.path())).unwrap()));
        assert!(is_sealed(&std::fs::read(dev_path(dir.path())).unwrap()));
    }

    #[test]
    fn protect_on_a_mixed_dir_only_touches_the_plain_file() {
        let dir = tempfile::tempdir().unwrap();
        StandaloneIdentity::generate()
            .save_sealed(&id_path(dir.path()), PASS, KdfParams::MIN)
            .unwrap();
        std::fs::write(
            dev_path(dir.path()),
            DeviceKey::generate().to_bytes().unwrap(),
        )
        .unwrap();
        let identity_before = std::fs::read(id_path(dir.path())).unwrap();
        load(dir.path(), None, Some(PASS), true, KdfParams::MIN).unwrap();
        assert_eq!(std::fs::read(id_path(dir.path())).unwrap(), identity_before);
        assert!(is_sealed(&std::fs::read(dev_path(dir.path())).unwrap()));
    }

    #[test]
    fn pair_import_with_a_passphrase_writes_a_sealed_identity() {
        let dir = tempfile::tempdir().unwrap();
        let origin = StandaloneIdentity::generate();
        let link = origin.to_pair_link().unwrap();
        let (loaded, _) = load(dir.path(), Some(&link), Some(PASS), false, KdfParams::MIN).unwrap();
        assert_eq!(loaded.id(), origin.id());
        assert!(is_sealed(&std::fs::read(id_path(dir.path())).unwrap()));
    }

    #[test]
    fn pair_import_refuses_an_existing_identity_or_mls_state() {
        let link = StandaloneIdentity::generate().to_pair_link().unwrap();
        let with_identity = tempfile::tempdir().unwrap();
        make_plain(with_identity.path());
        assert!(
            load(
                with_identity.path(),
                Some(&link),
                None,
                false,
                KdfParams::MIN
            )
            .is_err()
        );
        let with_mls = tempfile::tempdir().unwrap();
        std::fs::write(with_mls.path().join("mls.sqlite"), b"x").unwrap();
        assert!(load(with_mls.path(), Some(&link), None, false, KdfParams::MIN).is_err());
        assert!(!id_path(with_mls.path()).exists());
    }

    #[test]
    fn pair_import_with_a_bad_link_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load(
                dir.path(),
                Some("kin://pair/lixo"),
                None,
                false,
                KdfParams::MIN
            )
            .is_err()
        );
        assert!(!id_path(dir.path()).exists());
    }
}
