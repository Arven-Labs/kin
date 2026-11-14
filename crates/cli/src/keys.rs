//! Custódia das chaves no disco (P13, P30 fatia 1a, ADR 0012): decide se `identity.key` e `device.key`
//! ficam cifrados, de onde vem a senha e como migrar um diretório antigo em claro.

use std::path::{Path, PathBuf};

use kin_identity::{
    AuthorityCertificate, AuthorityIdentity, AuthorityKey, DeviceKey, IdentityId, IdentityProvider,
    Issuer, KdfParams, StandaloneIdentity,
};
use zeroize::Zeroizing;

use crate::cold::{AUTHORITY_CERT_FILE, AUTHORITY_KEY_FILE};

/// A identidade que este `--data-dir` carrega (P30 2b, ADR 0015): a mestra (modo de sempre) ou o
/// **primário**, que tem só a chave de autoridade e o certificado dela, sem a mestra.
///
/// As duas variantes em `Box`: as duas são grandes e de tamanhos diferentes (clippy `large_enum_variant`).
pub enum LocalIdentity {
    Master(Box<StandaloneIdentity>),
    Primary(Box<AuthorityIdentity>),
}

impl LocalIdentity {
    pub fn id(&self) -> IdentityId {
        self.provider().id()
    }

    /// O que o `Chat` usa para certificar o device (v3 pela mestra, v4 pela autoridade).
    pub fn provider(&self) -> &dyn IdentityProvider {
        match self {
            Self::Master(master) => master.as_ref(),
            Self::Primary(primary) => primary.as_ref(),
        }
    }

    /// A mestra, se este diretório a tem (só o modo antigo; o primário nunca).
    pub fn master(&self) -> Option<&StandaloneIdentity> {
        match self {
            Self::Master(master) => Some(master.as_ref()),
            Self::Primary(_) => None,
        }
    }
}

/// Auditoria da 2b, achado 1: o primário só roda se o certificado **em uso** (o guardado no
/// `mls.sqlite`, se já existia) é da autoridade atual. Um v3 da mestra (migração no mesmo diretório)
/// ou um v4 de uma autoridade anterior (rotate) deixariam o primário fora da cadeia atual.
pub fn ensure_current_issuer(
    identity: &LocalIdentity,
    in_use: &kin_identity::DeviceCertificate,
) -> Result<(), String> {
    let LocalIdentity::Primary(primary) = identity else {
        return Ok(());
    };
    match &in_use.issuer {
        Issuer::Authority(authority) if **authority == *primary.certificate() => Ok(()),
        Issuer::Master(_) => Err(
            "o mls.sqlite deste primário guarda um certificado de device assinado pela mestra (de quando ela estava aqui): use um --data-dir novo para o primário"
                .into(),
        ),
        Issuer::Authority(_) => Err(
            "o mls.sqlite deste primário guarda um certificado de device de outra autoridade (anterior a esta): use um --data-dir novo para o primário"
                .into(),
        ),
    }
}

/// Se `path` existe **como entrada do diretório**, mesmo um link quebrado (auditoria da 2b, achado 5):
/// um volume não montado não pode fazer o primário parecer um diretório novo. Erro de E/S propaga.
fn present(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

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
    // O `authority.key` do primário (P30 2b) conta como os outros: cifrado = pedir a senha uma vez.
    for name in ["identity.key", AUTHORITY_KEY_FILE, "device.key"] {
        let path = dir.join(name);
        // `present`, não `exists`: um link quebrado falha aqui em vez de contar como ausente.
        if present(&path)? {
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
) -> Result<(LocalIdentity, DeviceKey), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    if present(&dir.join(AUTHORITY_KEY_FILE))? || present(&dir.join(AUTHORITY_CERT_FILE))? {
        return load_primary(dir, pair_text.is_some(), passphrase, protect, params);
    }
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
    let device = load_or_create_device(&device_path, passphrase, params)?;
    Ok((LocalIdentity::Master(Box::new(identity)), device))
}

/// Teto do `authority.cert` (o real tem ~160 bytes).
const AUTHORITY_CERT_LIMIT: u64 = 4096;

/// Lê o `authority.cert`: só arquivo regular (recusa `/dev/zero`, FIFO, dispositivo) e no máximo
/// `AUTHORITY_CERT_LIMIT` bytes **na leitura**, não só no `stat` (auditoria da 2b, achado 2: o arquivo
/// pode ser trocado entre os dois).
fn read_authority_cert(path: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use std::io::Read as _;
    if !std::fs::metadata(path)?.is_file() {
        return Err(format!("{AUTHORITY_CERT_FILE} não é um arquivo regular").into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(AUTHORITY_CERT_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > AUTHORITY_CERT_LIMIT {
        return Err(format!("{AUTHORITY_CERT_FILE} grande demais").into());
    }
    Ok(bytes)
}

/// Carrega o **primário** (P30 2b, ADR 0015): `authority.key` + `authority.cert`, sem a mestra.
/// Tudo o que este passo recusa é conferido **antes** de criar o `device.key`. O `Chat::open` ainda
/// pode recusar depois (estado MLS de outra identidade, `ensure_current_issuer`), com o `device.key`
/// já criado, como no modo mestra.
fn load_primary(
    dir: &Path,
    pairing: bool,
    passphrase: Option<&[u8]>,
    protect: bool,
    params: KdfParams,
) -> Result<(LocalIdentity, DeviceKey), Box<dyn std::error::Error>> {
    let key_path = dir.join(AUTHORITY_KEY_FILE);
    let cert_path = dir.join(AUTHORITY_CERT_FILE);
    let device_path = dir.join("device.key");
    if pairing {
        return Err(
            "este --data-dir é de um primário (authority.key): --pair não importa a mestra aqui"
                .into(),
        );
    }
    if present(&dir.join("identity.key"))? {
        return Err(
            "este --data-dir tem a mestra (identity.key) e a autoridade (authority.key): a mestra não pode ficar no primário; leve o identity.key para fora (mídia offline)"
                .into(),
        );
    }
    if !present(&key_path)? || !present(&cert_path)? {
        return Err(format!(
            "primário incompleto: são precisos {AUTHORITY_KEY_FILE} e {AUTHORITY_CERT_FILE} juntos"
        )
        .into());
    }
    let certificate_bytes = read_authority_cert(&cert_path)?;
    if protect {
        let pass = passphrase.ok_or("--protect exige uma senha")?;
        if !kin_identity::key_file_is_sealed(&key_path)? {
            AuthorityKey::protect(&key_path, pass, params)?;
        }
        if device_path.exists() && !kin_identity::key_file_is_sealed(&device_path)? {
            DeviceKey::protect(&device_path, pass, params)?;
        }
    }
    let key = AuthorityKey::load_with(&key_path, passphrase)?;
    let certificate = AuthorityCertificate::from_bytes(&certificate_bytes)?;
    let primary = AuthorityIdentity::new(key, certificate)?;
    let device = load_or_create_device(&device_path, passphrase, params)?;
    Ok((LocalIdentity::Primary(Box::new(primary)), device))
}

/// Abre o `device.key` ou, se não existe, gera e grava um (cifrado com a senha, ou em claro sem ela).
fn load_or_create_device(
    path: &Path,
    passphrase: Option<&[u8]>,
    params: KdfParams,
) -> Result<DeviceKey, Box<dyn std::error::Error>> {
    if path.exists() {
        return Ok(DeviceKey::load_with(path, passphrase)?);
    }
    let device = DeviceKey::generate();
    match passphrase {
        Some(pass) => device.save_sealed(path, pass, params)?,
        None => crate::write_private(path, &device.to_bytes()?)?,
    }
    Ok(device)
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

    // ---- primário (P30 2b, ADR 0015): authority.key + authority.cert, sem a mestra ----

    const MASTER_PASS: &[u8] = b"senha da mestra fria";

    /// Roda o `kin cold init` de verdade: mestra em `root/offline`, primário em `root/primary`.
    /// A senha do primário é `PASS` (a "senha da autoridade").
    fn cold_init(root: &Path) -> (PathBuf, kin_identity::IdentityId) {
        let primary = root.join("primary");
        let outcome = crate::cold::init(
            &root.join("offline/master.key"),
            &primary,
            MASTER_PASS,
            PASS,
            KdfParams::MIN,
            1_000,
        )
        .unwrap();
        (primary, outcome.identity)
    }

    fn primary_load(dir: &Path) -> Result<(LocalIdentity, DeviceKey), Box<dyn std::error::Error>> {
        load(dir, None, Some(PASS), false, KdfParams::MIN)
    }

    #[test]
    fn a_cold_init_directory_opens_as_the_primary_of_the_master_identity() {
        let root = tempfile::tempdir().unwrap();
        let (primary, identity) = cold_init(root.path());

        let (loaded, device) = primary_load(&primary).unwrap();

        assert!(matches!(loaded, LocalIdentity::Primary(_)));
        assert_eq!(loaded.id(), identity);
        assert!(loaded.master().is_none(), "primário com a mestra");
        assert!(!id_path(&primary).exists(), "não pode criar identity.key");
        assert!(is_sealed(&std::fs::read(dev_path(&primary)).unwrap()));
        // Reabrir dá o mesmo device e a mesma identidade.
        let (again, device_again) = primary_load(&primary).unwrap();
        assert_eq!(again.id(), identity);
        assert_eq!(device_again.peer_id(), device.peer_id());
    }

    #[test]
    fn the_primary_certifies_its_device_in_v4_under_the_master_identity() {
        let root = tempfile::tempdir().unwrap();
        let (primary, identity) = cold_init(root.path());
        let (loaded, device) = primary_load(&primary).unwrap();

        let cert = loaded
            .provider()
            .authorize_device(
                &DeviceKey::generate().public_key(),
                &device.public_key(),
                &device.nostr_public_key(),
                2_000,
            )
            .unwrap();

        cert.verify().unwrap();
        assert_eq!(cert.identity(), &identity);
        assert!(matches!(cert.issuer, kin_identity::Issuer::Authority(_)));
    }

    #[test]
    fn a_primary_directory_counts_as_sealed_before_the_device_key_exists() {
        let root = tempfile::tempdir().unwrap();
        let (primary, _) = cold_init(root.path());
        assert!(!dev_path(&primary).exists());

        // Assim a CLI pede a senha uma vez (sem confirmar) em vez de tratar como diretório novo.
        assert_eq!(files_state(&primary).unwrap(), FilesState::Sealed);
    }

    #[test]
    fn a_directory_with_the_master_and_the_authority_is_refused_untouched() {
        let root = tempfile::tempdir().unwrap();
        let (primary, _) = cold_init(root.path());
        StandaloneIdentity::generate()
            .save_sealed(&id_path(&primary), PASS, KdfParams::MIN)
            .unwrap();

        assert!(primary_load(&primary).is_err());
        assert!(!dev_path(&primary).exists(), "não pode criar device.key");
    }

    #[test]
    fn half_an_authority_is_refused() {
        for missing in [AUTHORITY_KEY_FILE, AUTHORITY_CERT_FILE] {
            let root = tempfile::tempdir().unwrap();
            let (primary, _) = cold_init(root.path());
            std::fs::remove_file(primary.join(missing)).unwrap();

            assert!(primary_load(&primary).is_err(), "aceitou sem {missing}");
            assert!(!id_path(&primary).exists(), "sem {missing}: mestra nova");
            assert!(!dev_path(&primary).exists());
        }
    }

    #[test]
    fn pairing_into_a_primary_directory_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let (primary, _) = cold_init(root.path());
        let link = StandaloneIdentity::generate().to_pair_link().unwrap();

        assert!(load(&primary, Some(&link), Some(PASS), false, KdfParams::MIN).is_err());
        assert!(!id_path(&primary).exists(), "--pair gravou a mestra");
    }

    #[test]
    fn an_authority_certificate_for_another_key_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let (primary, _) = cold_init(root.path());
        let other = tempfile::tempdir().unwrap();
        let (other_primary, _) = cold_init(other.path());
        std::fs::remove_file(primary.join(AUTHORITY_CERT_FILE)).unwrap();
        std::fs::copy(
            other_primary.join(AUTHORITY_CERT_FILE),
            primary.join(AUTHORITY_CERT_FILE),
        )
        .unwrap();

        let err = primary_load(&primary).err().expect("aceitou o cert");

        assert!(matches!(
            err.downcast_ref::<Error>(),
            Some(Error::AuthorityKeyMismatch)
        ));
        assert!(!dev_path(&primary).exists());
    }

    #[test]
    fn a_primary_with_the_wrong_passphrase_fails_and_creates_nothing() {
        let root = tempfile::tempdir().unwrap();
        let (primary, _) = cold_init(root.path());

        assert!(load(&primary, None, Some(b"errada"), false, KdfParams::MIN).is_err());
        assert!(load(&primary, None, None, false, KdfParams::MIN).is_err());
        assert!(!dev_path(&primary).exists());
        assert!(!id_path(&primary).exists());
    }

    #[test]
    fn a_directory_without_authority_files_still_opens_with_the_master() {
        let dir = tempfile::tempdir().unwrap();
        let (loaded, _) = load(dir.path(), None, Some(PASS), false, KdfParams::MIN).unwrap();

        assert!(matches!(loaded, LocalIdentity::Master(_)));
        assert!(loaded.master().is_some());
    }

    // ---- auditoria da 2b ----

    /// Certificado de um device qualquer, emitido por `issuer`.
    fn device_cert(issuer: &dyn IdentityProvider) -> kin_identity::DeviceCertificate {
        let device = DeviceKey::generate();
        issuer
            .authorize_device(
                &DeviceKey::generate().public_key(),
                &device.public_key(),
                &device.nostr_public_key(),
                2_000,
            )
            .unwrap()
    }

    fn cold_master(root: &Path) -> StandaloneIdentity {
        StandaloneIdentity::load_with(&root.join("offline/master.key"), Some(MASTER_PASS)).unwrap()
    }

    #[test]
    fn the_primary_runs_with_a_certificate_from_its_own_authority() {
        let root = tempfile::tempdir().unwrap();
        let (primary, _) = cold_init(root.path());
        let (loaded, _) = primary_load(&primary).unwrap();

        let own = device_cert(loaded.provider());

        assert!(ensure_current_issuer(&loaded, &own).is_ok());
    }

    /// Achado 1: migração no mesmo diretório (o `mls.sqlite` guardou o v3 de quando a mestra estava ali).
    #[test]
    fn the_primary_refuses_a_stored_certificate_signed_by_the_master() {
        let root = tempfile::tempdir().unwrap();
        let (primary, _) = cold_init(root.path());
        let (loaded, _) = primary_load(&primary).unwrap();

        let v3 = device_cert(&cold_master(root.path()));

        assert!(ensure_current_issuer(&loaded, &v3).is_err());
    }

    /// Achado 1: rotate no mesmo diretório (o `mls.sqlite` guardou o v4 da autoridade anterior).
    #[test]
    fn the_primary_refuses_a_stored_certificate_from_a_previous_authority() {
        let root = tempfile::tempdir().unwrap();
        let (primary, _) = cold_init(root.path());
        let (loaded, _) = primary_load(&primary).unwrap();
        let master = cold_master(root.path());
        let previous_key = kin_identity::AuthorityKey::generate();
        let previous_cert = master
            .certify_authority(&previous_key.public_key(), 500)
            .unwrap();
        let previous = AuthorityIdentity::new(previous_key, previous_cert).unwrap();

        let old = device_cert(&previous);

        assert!(ensure_current_issuer(&loaded, &old).is_err());
    }

    #[test]
    fn the_master_mode_does_not_check_the_issuer() {
        let dir = tempfile::tempdir().unwrap();
        let (loaded, _) = load(dir.path(), None, Some(PASS), false, KdfParams::MIN).unwrap();

        let own = device_cert(loaded.provider());

        assert!(ensure_current_issuer(&loaded, &own).is_ok());
    }

    /// Achado 5: com os arquivos da autoridade apontando para um volume não montado, o diretório não
    /// pode virar "novo" e ganhar uma mestra nova.
    #[test]
    fn dangling_authority_links_are_refused_instead_of_creating_a_new_master() {
        let root = tempfile::tempdir().unwrap();
        let (primary, _) = cold_init(root.path());
        let gone = root.path().join("volume-desmontado");
        for name in [AUTHORITY_KEY_FILE, AUTHORITY_CERT_FILE] {
            std::fs::remove_file(primary.join(name)).unwrap();
            std::os::unix::fs::symlink(gone.join(name), primary.join(name)).unwrap();
        }

        assert!(!matches!(files_state(&primary), Ok(FilesState::New)));
        assert!(primary_load(&primary).is_err());
        assert!(!id_path(&primary).exists(), "virou uma mestra nova");
        assert!(!dev_path(&primary).exists());
    }

    /// Achado 2: um `authority.cert` que aponta para `/dev/zero` (tamanho 0 no `stat`) é recusado sem
    /// ler até a memória acabar. Sem prova de vermelho: antes da correção, este teste não terminaria.
    #[test]
    fn an_authority_certificate_that_is_not_a_regular_file_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let (primary, _) = cold_init(root.path());
        std::fs::remove_file(primary.join(AUTHORITY_CERT_FILE)).unwrap();
        std::os::unix::fs::symlink("/dev/zero", primary.join(AUTHORITY_CERT_FILE)).unwrap();

        assert!(primary_load(&primary).is_err());
        assert!(!dev_path(&primary).exists());
    }
}
