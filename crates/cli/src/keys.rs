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
    /// Device **pareado** (P30 2d-ii-b, ADR 0021): só a identidade do primário (de `primary.id`), sem
    /// mestra nem autoridade; o certificado v4 já está no `mls.sqlite`.
    Paired(IdentityId),
}

/// Aviso de migração para quem roda no modo `Master` (mestra no próprio device, v3): essa identidade
/// pode ter sido copiada por `/pair` e não ganha mais devices nem revoga (P30 fatia 6, ADR 0035).
/// `None` para o primário e para o device pareado.
pub fn notice_for(identity: &LocalIdentity) -> Option<&'static str> {
    match identity {
        LocalIdentity::Master(_) => Some(MASTER_NOTICE),
        LocalIdentity::Primary(_) | LocalIdentity::Paired(_) => None,
    }
}

const MASTER_NOTICE: &str = "aviso: a chave mestra desta identidade (v3) está neste device, e copiá-la para outro device (o `/pair` \
antigo, ou copiar o identity.key à mão) a expõe do mesmo jeito: essa mestra nunca revoga um device. Se você já copiou, \
trate-a como exposta. Para ter vários devices com revogação é preciso uma identidade NOVA (outro ID): \
1) `kin cold init --master-out ARQUIVO --authority-dir DIR` (mestra fria offline e chave de autoridade; o primário usa \
um --data-dir novo); 2) pareie os outros devices com `kin pair request`, `kin pair answer` e `kin pair accept`; \
3) troque convites com cada contato e confira o ID novo fora de banda (`/accept`, depois `/trust`); 4) em cada contato, \
`/untrust` da identidade antiga; 5) apague o identity.key antigo e os backups dele. Quem não reconfirmar continua \
confiando na chave antiga.";

impl LocalIdentity {
    pub fn id(&self) -> IdentityId {
        match self {
            Self::Master(master) => master.id(),
            Self::Primary(primary) => primary.id(),
            Self::Paired(id) => id.clone(),
        }
    }

    /// O que o `Chat` usa para certificar o device (v3 pela mestra, v4 pela autoridade). **`None` no
    /// pareado**: ele não certifica nada, o certificado já está no banco (`Chat::open_paired`).
    pub fn provider(&self) -> Option<&dyn IdentityProvider> {
        match self {
            Self::Master(master) => Some(master.as_ref()),
            Self::Primary(primary) => Some(primary.as_ref()),
            Self::Paired(_) => None,
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
    let primary = match identity {
        LocalIdentity::Master(_) => return Ok(()),
        // Pareado (ADR 0021): o certificado tem de ser v4, de uma autoridade. Um v3 (de um banco criado
        // pela mestra) deixaria o device fora do regime novo. `Chat::open_paired` já recusa (P30 3e-i) e
        // roda antes desta função em `main`; esta checagem é defesa em profundidade.
        LocalIdentity::Paired(_) => {
            return match &in_use.issuer {
                Issuer::Authority(_) => Ok(()),
                Issuer::Master(_) => Err(
                    "o mls.sqlite deste device pareado guarda um certificado assinado pela mestra (v3), não por uma autoridade: use um --data-dir novo e pareie de novo com `kin pair`"
                        .into(),
                ),
            };
        }
        LocalIdentity::Primary(primary) => primary,
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
        // Chaves já seladas aceitam `--protect`: é o caminho de cifrar os bancos (`open_db_key`).
        (FilesState::New, true) => {
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
            // Chaves já seladas: a senha existe, não há o que confirmar.
            confirm: state == FilesState::New || (opts.protect && state != FilesState::Sealed),
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
/// - `passphrase`: `Some` cifra e abre com senha; `None` trabalha em claro (legado ou `--no-passphrase`).
/// - `protect`: antes de abrir, cifra no lugar os arquivos que ainda estão em claro.
pub fn load(
    dir: &Path,
    passphrase: Option<&[u8]>,
    protect: bool,
    params: KdfParams,
) -> Result<(LocalIdentity, DeviceKey), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    // `primary.id` só existe num device pareado (`kin pair request`): decide o modo antes de qualquer outra coisa.
    if present(&dir.join(crate::pair::PRIMARY_ID_FILE))? {
        return load_paired(dir, passphrase, protect, params);
    }
    if present(&dir.join(AUTHORITY_KEY_FILE))? || present(&dir.join(AUTHORITY_CERT_FILE))? {
        return load_primary(dir, passphrase, protect, params);
    }
    let identity_path = dir.join("identity.key");
    let device_path = dir.join("device.key");
    // Estado MLS sem nenhuma identidade (nem `identity.key`, nem `primary.id`, nem autoridade) não é um
    // diretório novo: um device pareado a quem apagaram o `primary.id` não pode ganhar uma mestra nova
    // (auditoria da 2d-ii-b, I1).
    if !present(&identity_path)? && present(&dir.join("mls.sqlite"))? {
        return Err(
            "este --data-dir tem estado (mls.sqlite) mas nenhuma identidade: restaure identity.key ou primary.id (device pareado), ou use um diretório novo"
                .into(),
        );
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

/// Chave dos bancos, selada com a mesma senha das outras chaves (P13 fatia 3, ADR 0045).
pub const DB_KEY_FILE: &str = "db.key";

/// Os bancos SQLite do `--data-dir`: o MLS (também o pendente do pareamento) e o `outbox` do `Chat`.
pub const DB_FILES: [&str; 2] = ["mls.sqlite", "mls.outbox.db"];

/// Decide, cria e desbloqueia a chave dos bancos, e com `protect` migra os bancos em claro (ADR 0045).
///
/// O marcador de "sem chave" é o **estado das outras chaves**, não o `db.key`: `passphrase` `Some` (chaves
/// seladas) exige bancos cifrados, e um `db.key` ausente com banco cifrado é erro, nunca `None`. Chamar
/// **depois** de `load`, que já validou a senha e as chaves.
pub fn open_db_key(
    dir: &Path,
    passphrase: Option<&[u8]>,
    protect: bool,
    params: KdfParams,
) -> Result<Option<kin_identity::DbKey>, Box<dyn std::error::Error>> {
    use kin_crypto::DbKind;

    let key_path = dir.join(DB_KEY_FILE);
    let mut plain = Vec::new();
    let mut encrypted = false;
    for name in DB_FILES {
        match kin_crypto::db_kind(&dir.join(name))? {
            DbKind::Plain => plain.push(name),
            DbKind::Encrypted => encrypted = true,
            DbKind::Absent => {}
        }
    }
    let has_key = present(&key_path)?;

    let Some(pass) = passphrase else {
        // Chaves em claro (legado ou `--no-passphrase`): os bancos também ficam em claro.
        if protect {
            return Err("--protect exige uma senha".into());
        }
        if has_key {
            return Err(format!(
                "há {DB_KEY_FILE} neste --data-dir, mas as chaves estão em claro: informe a senha (sem --no-passphrase)"
            )
            .into());
        }
        if encrypted {
            return Err("os bancos estão cifrados: informe a senha (sem --no-passphrase)".into());
        }
        return Ok(None);
    };

    // Chaves seladas exigem bancos cifrados: nunca "sem chave" por o `db.key` ter sumido.
    if !plain.is_empty() && !protect {
        return Err(format!(
            "as chaves estão cifradas mas há banco em claro ({}): rode uma vez com --protect para cifrá-lo",
            plain.join(", ")
        )
        .into());
    }
    let key = if has_key {
        kin_identity::DbKey::load_with(&key_path, pass)?
    } else if encrypted {
        return Err(format!(
            "há banco cifrado mas o {DB_KEY_FILE} não está neste --data-dir: restaure-o do backup (sem ele os bancos não abrem)"
        )
        .into());
    } else {
        // Gravado **antes** de qualquer migração: uma queda no meio nunca deixa banco cifrado sem chave.
        let key = kin_identity::DbKey::generate();
        key.save_sealed(&key_path, pass, params)?;
        key
    };
    for name in plain {
        kin_crypto::migrate_to_encrypted(&dir.join(name), &key)?;
    }
    Ok(Some(key))
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
    passphrase: Option<&[u8]>,
    protect: bool,
    params: KdfParams,
) -> Result<(LocalIdentity, DeviceKey), Box<dyn std::error::Error>> {
    let key_path = dir.join(AUTHORITY_KEY_FILE);
    let cert_path = dir.join(AUTHORITY_CERT_FILE);
    let device_path = dir.join("device.key");
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

/// Carrega um device **pareado** (P30 2d-ii-b, ADR 0021): `primary.id` + `device.key` já existente,
/// sem mestra nem autoridade. Nunca cria chave.
fn load_paired(
    dir: &Path,
    passphrase: Option<&[u8]>,
    protect: bool,
    params: KdfParams,
) -> Result<(LocalIdentity, DeviceKey), Box<dyn std::error::Error>> {
    for name in ["identity.key", AUTHORITY_KEY_FILE, AUTHORITY_CERT_FILE] {
        if present(&dir.join(name))? {
            return Err(format!(
                "este --data-dir tem {name} e primary.id: um device pareado não guarda a mestra nem a autoridade; leve {name} para fora"
            )
            .into());
        }
    }
    // A identidade esperada vem do arquivo (fora do banco); `Chat::open_paired` a confere contra o
    // certificado a cada abertura (ADR 0017).
    let expected = crate::pair::read_primary_id(dir)?
        .ok_or("primary.id sumiu enquanto o diretório era aberto")?;
    // Nunca cria chave: sem `device.key` o pareamento está incompleto (ou o arquivo foi perdido).
    let device_path = dir.join("device.key");
    if !present(&device_path)? {
        return Err(
            "device pareado sem device.key: restaure-o do backup ou pareie de novo num --data-dir novo (`kin pair request`)"
                .into(),
        );
    }
    if protect {
        let pass = passphrase.ok_or("--protect exige uma senha")?;
        if !kin_identity::key_file_is_sealed(&device_path)? {
            DeviceKey::protect(&device_path, pass, params)?;
        }
    }
    let device = DeviceKey::load_with(&device_path, passphrase)?;
    Ok((LocalIdentity::Paired(expected), device))
}

/// Abre só a identidade do **primário** (`authority.key` + `authority.cert`), sem criar `device.key`
/// (P30 2d-ii-a2): o `kin pair answer` só precisa certificar, não de um device.
pub fn load_primary_identity(
    dir: &Path,
    passphrase: Option<&[u8]>,
) -> Result<AuthorityIdentity, Box<dyn std::error::Error>> {
    let key_path = dir.join(AUTHORITY_KEY_FILE);
    let cert_path = dir.join(AUTHORITY_CERT_FILE);
    if !present(&key_path)? || !present(&cert_path)? {
        return Err(format!(
            "não é um diretório de primário: são precisos {AUTHORITY_KEY_FILE} e {AUTHORITY_CERT_FILE}"
        )
        .into());
    }
    let certificate_bytes = read_authority_cert(&cert_path)?;
    let key = AuthorityKey::load_with(&key_path, passphrase)?;
    let certificate = AuthorityCertificate::from_bytes(&certificate_bytes)?;
    Ok(AuthorityIdentity::new(key, certificate)?)
}

/// Abre o `device.key` ou, se não existe, gera e grava um (cifrado com a senha, ou em claro sem ela).
pub(crate) fn load_or_create_device(
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

/// Os arquivos de chave que a senha sela, na ordem da troca (`db.key` por último).
const SEALED_KEY_FILES: [&str; 4] = [
    "identity.key",
    AUTHORITY_KEY_FILE,
    "device.key",
    DB_KEY_FILE,
];

/// Troca a senha de todos os arquivos de chave de `dir` (P13, `kin passwd`) e devolve os que foram
/// regravados. Os bancos não são tocados: o SQLCipher usa o `db.key`, e só ele é re-selado.
///
/// **Retomável**: um arquivo que já abre com a senha nova é pulado, então repetir o comando depois de
/// uma queda termina o serviço. Antes de gravar qualquer arquivo, confere **todos**: se algum não abre
/// nem com a velha nem com a nova, aborta sem gravar nada. Recusa senha nova vazia ou igual à velha,
/// diretório sem arquivo de chave e arquivo em claro (isso é o `--protect`).
pub fn change_passphrase(
    dir: &Path,
    old: &[u8],
    new: &[u8],
    params: KdfParams,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    if new.is_empty() {
        return Err("a senha nova não pode ser vazia".into());
    }
    if new == old {
        return Err("a senha nova é igual à atual".into());
    }
    let mut files = Vec::new();
    for name in SEALED_KEY_FILES {
        let path = dir.join(name);
        if present(&path)? {
            if !kin_identity::key_file_is_sealed(&path)? {
                return Err(format!(
                    "{name} está em claro: rode uma vez com --protect para cifrá-lo antes de trocar a senha"
                )
                .into());
            }
            files.push((name, path));
        }
    }
    if files.is_empty() {
        return Err("não há arquivo de chave neste --data-dir".into());
    }
    // Confere todos antes de gravar o primeiro.
    let mut pending = Vec::new();
    for (name, path) in &files {
        let state = kin_identity::rekey_state(path, old, new)
            .map_err(|e| format!("{name}: {e} (nada foi alterado)"))?;
        if state == kin_identity::RekeyState::Old {
            pending.push((*name, path));
        }
    }
    let mut changed = Vec::new();
    for (name, path) in pending {
        kin_identity::rekey_file(path, old, new, params)
            .map_err(|e| format!("{name}: {e} (repita o comando para terminar a troca)"))?;
        changed.push(name.to_string());
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_identity::{Error, IdentityProvider, is_sealed};

    const PASS: &[u8] = b"senha de teste";

    // ---- aviso de migração do modo Master (P30 fatia 6) ----

    fn master_identity() -> StandaloneIdentity {
        StandaloneIdentity::generate()
    }

    fn primary_identity(master: &StandaloneIdentity) -> kin_identity::AuthorityIdentity {
        let key = kin_identity::AuthorityKey::generate();
        let chain = master.certify_authority(&key.public_key(), 1).unwrap();
        kin_identity::AuthorityIdentity::new(key, chain).unwrap()
    }

    #[test]
    fn the_master_mode_notice_gives_the_migration_roadmap() {
        let notice = notice_for(&LocalIdentity::Master(Box::new(master_identity())))
            .expect("o modo Master tem de avisar");

        for needle in [
            "kin cold init",
            "kin pair request",
            "/untrust",
            "/accept",
            "identidade NOVA",
            "identity.key",
        ] {
            assert!(
                notice.contains(needle),
                "o aviso não cita {needle}: {notice}"
            );
        }
        // o aviso nunca carrega segredo nem o prefixo do link antigo
        assert!(!notice.contains("kin://pair/"), "{notice}");
    }

    #[test]
    fn the_primary_and_the_paired_device_get_no_migration_notice() {
        let master = master_identity();
        let primary = LocalIdentity::Primary(Box::new(primary_identity(&master)));
        let paired = LocalIdentity::Paired(master.id());

        assert_eq!(notice_for(&primary), None);
        assert_eq!(notice_for(&paired), None);
    }

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
    fn protect_on_a_new_dir_is_refused() {
        assert!(decide(&opts(Some("/s"), false, true), FilesState::New, false).is_err());
    }

    /// P13 fatia 3: chaves já seladas (ADR 0012) com bancos ainda em claro precisam do `--protect` para
    /// migrar os bancos; a decisão sobre os bancos é do `open_db_key`, aqui só não pode recusar.
    #[test]
    fn protect_on_sealed_keys_is_accepted_to_migrate_the_databases() {
        let a = decide(&opts(Some("/s"), false, true), FilesState::Sealed, false).unwrap();
        assert_eq!(
            a,
            Access::Sealed {
                source: file_source(),
                protect: true
            }
        );
    }

    #[test]
    fn protect_on_sealed_keys_prompts_once_because_the_passphrase_already_exists() {
        let a = decide(&opts(None, false, true), FilesState::Sealed, true).unwrap();
        assert_eq!(
            a,
            Access::Sealed {
                source: Source::Prompt { confirm: false },
                protect: true
            }
        );
    }

    #[test]
    fn conflicting_flags_are_refused() {
        assert!(decide(&opts(Some("/s"), true, false), FilesState::New, false).is_err());
        assert!(decide(&opts(None, true, true), FilesState::Plain, false).is_err());
    }

    // ---- open_db_key (P13 fatia 3, ADR 0045) ----

    const DB_SECRET: &str = "segredo-do-banco-p13";

    fn plain_db(path: &Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE t (v TEXT NOT NULL);
             INSERT INTO t (v) VALUES ('{DB_SECRET}');
             PRAGMA user_version = 3;"
        ))
        .unwrap();
    }

    fn both_plain(dir: &Path) {
        for name in DB_FILES {
            plain_db(&dir.join(name));
        }
    }

    fn kind(dir: &Path, name: &str) -> kin_crypto::DbKind {
        kin_crypto::db_kind(&dir.join(name)).unwrap()
    }

    fn call(
        dir: &Path,
        pass: Option<&[u8]>,
        protect: bool,
    ) -> Result<Option<kin_identity::DbKey>, Box<dyn std::error::Error>> {
        open_db_key(dir, pass, protect, KdfParams::MIN)
    }

    fn assert_rows_and_version(dir: &Path, name: &str, key: &kin_identity::DbKey) {
        let conn = kin_crypto::open_db(&dir.join(name), Some(key)).unwrap();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!((rows, version), (1, 3), "{name}");
    }

    fn bytes(dir: &Path, name: &str) -> Vec<u8> {
        std::fs::read(dir.join(name)).unwrap()
    }

    #[test]
    fn a_new_sealed_dir_gets_a_sealed_db_key_and_no_database_yet() {
        let dir = tempfile::tempdir().unwrap();
        let key = call(dir.path(), Some(PASS), false).unwrap().expect("chave");
        let path = dir.path().join(DB_KEY_FILE);
        assert!(kin_identity::key_file_is_sealed(&path).unwrap());
        let back = kin_identity::DbKey::load_with(&path, PASS).unwrap();
        assert_eq!(back.expose_secret(), key.expose_secret());
        for name in DB_FILES {
            assert!(!dir.path().join(name).exists(), "{name} criado cedo");
        }
    }

    #[test]
    fn the_second_start_returns_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let first = call(dir.path(), Some(PASS), false).unwrap().unwrap();
        let second = call(dir.path(), Some(PASS), false).unwrap().unwrap();
        assert_eq!(first.expose_secret(), second.expose_secret());
    }

    #[test]
    fn a_new_plain_dir_has_no_key_and_creates_no_db_key() {
        let dir = tempfile::tempdir().unwrap();
        assert!(call(dir.path(), None, false).unwrap().is_none());
        assert!(!dir.path().join(DB_KEY_FILE).exists());
    }

    #[test]
    fn plain_keys_with_plain_databases_stay_as_they_are() {
        let dir = tempfile::tempdir().unwrap();
        both_plain(dir.path());
        let before = bytes(dir.path(), "mls.sqlite");
        assert!(call(dir.path(), None, false).unwrap().is_none());
        assert_eq!(bytes(dir.path(), "mls.sqlite"), before);
        assert!(!dir.path().join(DB_KEY_FILE).exists());
    }

    #[test]
    fn sealed_keys_with_plain_databases_are_refused_without_protect_and_nothing_changes() {
        let dir = tempfile::tempdir().unwrap();
        both_plain(dir.path());
        let before = bytes(dir.path(), "mls.outbox.db");

        let err = call(dir.path(), Some(PASS), false).unwrap_err().to_string();

        assert!(err.contains("--protect"), "{err}");
        assert_eq!(bytes(dir.path(), "mls.outbox.db"), before);
        assert!(!dir.path().join(DB_KEY_FILE).exists(), "criou db.key à toa");
    }

    #[test]
    fn protect_migrates_both_databases_with_rows_and_user_version() {
        let dir = tempfile::tempdir().unwrap();
        both_plain(dir.path());

        let key = call(dir.path(), Some(PASS), true).unwrap().expect("chave");

        for name in DB_FILES {
            assert_eq!(kind(dir.path(), name), kin_crypto::DbKind::Encrypted);
            assert_rows_and_version(dir.path(), name, &key);
            let raw = bytes(dir.path(), name);
            assert!(
                !raw.windows(DB_SECRET.len())
                    .any(|w| w == DB_SECRET.as_bytes())
            );
        }
        let back = kin_identity::DbKey::load_with(&dir.path().join(DB_KEY_FILE), PASS).unwrap();
        assert_eq!(back.expose_secret(), key.expose_secret());
    }

    #[test]
    fn protect_resumes_after_a_crash_between_the_two_databases() {
        let dir = tempfile::tempdir().unwrap();
        both_plain(dir.path());
        let key = call(dir.path(), Some(PASS), true).unwrap().unwrap();
        // Estado de uma queda no meio: o segundo banco voltou a estar em claro.
        std::fs::remove_file(dir.path().join("mls.outbox.db")).unwrap();
        plain_db(&dir.path().join("mls.outbox.db"));
        let key_file_before = bytes(dir.path(), DB_KEY_FILE);

        let again = call(dir.path(), Some(PASS), true).unwrap().unwrap();

        assert_eq!(again.expose_secret(), key.expose_secret());
        assert_eq!(
            bytes(dir.path(), DB_KEY_FILE),
            key_file_before,
            "regravou o db.key"
        );
        for name in DB_FILES {
            assert_eq!(kind(dir.path(), name), kin_crypto::DbKind::Encrypted);
            assert_rows_and_version(dir.path(), name, &key);
        }
    }

    #[test]
    fn mixed_databases_without_protect_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        both_plain(dir.path());
        call(dir.path(), Some(PASS), true).unwrap();
        std::fs::remove_file(dir.path().join("mls.outbox.db")).unwrap();
        plain_db(&dir.path().join("mls.outbox.db"));

        assert!(call(dir.path(), Some(PASS), false).is_err());
        assert_eq!(kind(dir.path(), "mls.outbox.db"), kin_crypto::DbKind::Plain);
    }

    #[test]
    fn protect_when_everything_is_already_encrypted_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        both_plain(dir.path());
        let key = call(dir.path(), Some(PASS), true).unwrap().unwrap();
        let before = bytes(dir.path(), "mls.sqlite");

        let again = call(dir.path(), Some(PASS), true).unwrap().unwrap();

        assert_eq!(again.expose_secret(), key.expose_secret());
        assert_eq!(bytes(dir.path(), "mls.sqlite"), before);
    }

    /// O requisito da auditoria da fatia 2: apagar o `db.key` não pode virar "sem chave".
    #[test]
    fn sealed_keys_with_an_encrypted_database_and_no_db_key_are_an_error_not_none() {
        let dir = tempfile::tempdir().unwrap();
        both_plain(dir.path());
        call(dir.path(), Some(PASS), true).unwrap();
        std::fs::remove_file(dir.path().join(DB_KEY_FILE)).unwrap();
        let before = bytes(dir.path(), "mls.sqlite");

        for protect in [false, true] {
            let err = call(dir.path(), Some(PASS), protect)
                .unwrap_err()
                .to_string();
            assert!(err.contains(DB_KEY_FILE), "{err}");
        }
        assert!(!dir.path().join(DB_KEY_FILE).exists(), "gerou chave nova");
        assert_eq!(bytes(dir.path(), "mls.sqlite"), before);
    }

    /// O atacante apaga o `db.key` **e** troca o banco por um em claro: sem `--protect` é recusado, e o
    /// banco falso não é migrado em silêncio.
    #[test]
    fn deleting_the_db_key_and_swapping_in_a_plain_database_is_not_a_downgrade() {
        let dir = tempfile::tempdir().unwrap();
        both_plain(dir.path());
        call(dir.path(), Some(PASS), true).unwrap();
        std::fs::remove_file(dir.path().join(DB_KEY_FILE)).unwrap();
        std::fs::remove_file(dir.path().join("mls.sqlite")).unwrap();
        plain_db(&dir.path().join("mls.sqlite"));

        assert!(call(dir.path(), Some(PASS), false).is_err());
        assert_eq!(kind(dir.path(), "mls.sqlite"), kin_crypto::DbKind::Plain);
    }

    #[test]
    fn a_wrong_passphrase_does_not_open_the_db_key_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        call(dir.path(), Some(PASS), false).unwrap();
        let before = bytes(dir.path(), DB_KEY_FILE);
        assert!(call(dir.path(), Some(b"outra senha"), false).is_err());
        assert_eq!(bytes(dir.path(), DB_KEY_FILE), before);
    }

    #[test]
    fn plain_keys_with_a_db_key_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        call(dir.path(), Some(PASS), false).unwrap();
        assert!(call(dir.path(), None, false).is_err());
    }

    #[test]
    fn plain_keys_with_an_encrypted_database_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        both_plain(dir.path());
        call(dir.path(), Some(PASS), true).unwrap();
        std::fs::remove_file(dir.path().join(DB_KEY_FILE)).unwrap();
        assert!(call(dir.path(), None, false).is_err());
    }

    #[test]
    fn protect_without_a_passphrase_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        both_plain(dir.path());
        assert!(call(dir.path(), None, true).is_err());
        assert_eq!(kind(dir.path(), "mls.sqlite"), kin_crypto::DbKind::Plain);
    }

    #[test]
    fn the_error_messages_never_carry_the_key() {
        let dir = tempfile::tempdir().unwrap();
        both_plain(dir.path());
        let key = call(dir.path(), Some(PASS), true).unwrap().unwrap();
        let hex: String = key
            .expose_secret()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        std::fs::remove_file(dir.path().join(DB_KEY_FILE)).unwrap();
        let err = call(dir.path(), Some(PASS), false).unwrap_err().to_string();
        assert!(!err.contains(&hex), "{err}");
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
        let (id, dev) = load(dir.path(), Some(PASS), false, KdfParams::MIN).unwrap();
        assert!(is_sealed(&std::fs::read(id_path(dir.path())).unwrap()));
        assert!(is_sealed(&std::fs::read(dev_path(dir.path())).unwrap()));
        let (id2, dev2) = load(dir.path(), Some(PASS), false, KdfParams::MIN).unwrap();
        assert_eq!(id.id(), id2.id());
        assert_eq!(dev.peer_id(), dev2.peer_id());
    }

    #[test]
    fn load_with_the_wrong_or_no_passphrase_fails_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        load(dir.path(), Some(PASS), false, KdfParams::MIN).unwrap();
        let before = (
            std::fs::read(id_path(dir.path())).unwrap(),
            std::fs::read(dev_path(dir.path())).unwrap(),
        );
        assert!(load(dir.path(), Some(b"errada"), false, KdfParams::MIN).is_err());
        assert!(load(dir.path(), None, false, KdfParams::MIN).is_err());
        let after = (
            std::fs::read(id_path(dir.path())).unwrap(),
            std::fs::read(dev_path(dir.path())).unwrap(),
        );
        assert_eq!(before, after);
    }

    #[test]
    fn load_without_a_passphrase_keeps_the_plain_format() {
        let dir = tempfile::tempdir().unwrap();
        let (id, _) = load(dir.path(), None, false, KdfParams::MIN).unwrap();
        assert!(!is_sealed(&std::fs::read(id_path(dir.path())).unwrap()));
        assert!(!is_sealed(&std::fs::read(dev_path(dir.path())).unwrap()));
        let (id2, _) = load(dir.path(), None, false, KdfParams::MIN).unwrap();
        assert_eq!(id.id(), id2.id());
    }

    #[test]
    fn plain_dir_with_a_passphrase_and_no_protect_is_refused_untouched() {
        let dir = tempfile::tempdir().unwrap();
        make_plain(dir.path());
        let before = std::fs::read(id_path(dir.path())).unwrap();
        let err = load(dir.path(), Some(PASS), false, KdfParams::MIN)
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
        let (loaded, device) = load(dir.path(), Some(PASS), true, KdfParams::MIN).unwrap();
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
        load(dir.path(), Some(PASS), true, KdfParams::MIN).unwrap();
        assert_eq!(std::fs::read(id_path(dir.path())).unwrap(), identity_before);
        assert!(is_sealed(&std::fs::read(dev_path(dir.path())).unwrap()));
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
        load(dir, Some(PASS), false, KdfParams::MIN)
    }

    #[test]
    fn a_cold_init_directory_opens_as_the_primary_of_the_master_identity() {
        let root = tempfile::tempdir().unwrap();
        let (primary, identity) = cold_init(root.path());

        let (loaded, device) = primary_load(&primary).unwrap();

        assert!(matches!(loaded, LocalIdentity::Primary(_)));
        assert_eq!(loaded.id(), identity);
        assert!(
            !matches!(loaded, LocalIdentity::Master(_)),
            "primário com a mestra"
        );
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
            .unwrap()
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

        assert!(load(&primary, Some(b"errada"), false, KdfParams::MIN).is_err());
        assert!(load(&primary, None, false, KdfParams::MIN).is_err());
        assert!(!dev_path(&primary).exists());
        assert!(!id_path(&primary).exists());
    }

    #[test]
    fn a_directory_without_authority_files_still_opens_with_the_master() {
        let dir = tempfile::tempdir().unwrap();
        let (loaded, _) = load(dir.path(), Some(PASS), false, KdfParams::MIN).unwrap();

        assert!(matches!(loaded, LocalIdentity::Master(_)));
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

        let own = device_cert(loaded.provider().unwrap());

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
        let (loaded, _) = load(dir.path(), Some(PASS), false, KdfParams::MIN).unwrap();

        let own = device_cert(loaded.provider().unwrap());

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

    #[test]
    fn load_primary_identity_opens_the_authority_without_creating_a_device_key() {
        let root = tempfile::tempdir().unwrap();
        let (primary, identity) = cold_init(root.path());

        let loaded = load_primary_identity(&primary, Some(PASS)).unwrap();
        assert_eq!(loaded.id(), identity);
        assert!(!dev_path(&primary).exists(), "criou device.key");
        assert!(load_primary_identity(&primary, Some(b"senha errada")).is_err());
    }

    #[test]
    fn load_primary_identity_refuses_a_directory_that_is_not_a_primary() {
        let root = tempfile::tempdir().unwrap();
        assert!(load_primary_identity(root.path(), Some(PASS)).is_err());
        let (primary, _) = cold_init(root.path());
        std::fs::remove_file(primary.join(AUTHORITY_CERT_FILE)).unwrap();
        assert!(load_primary_identity(&primary, Some(PASS)).is_err());
    }

    // ---- device pareado (P30 2d-ii-b, ADR 0021) ----

    /// Um device pareado de verdade: `cold init` + `kin pair request`, `answer` e `accept` (a senha do
    /// device é `PASS`, a mesma da autoridade). Devolve o diretório, a identidade e o certificado v4.
    fn paired_dir(
        root: &Path,
    ) -> (
        PathBuf,
        kin_identity::IdentityId,
        kin_identity::DeviceCertificate,
    ) {
        let (primary_dir, identity) = cold_init(root);
        let primary = load_primary_identity(&primary_dir, Some(PASS)).unwrap();
        let dir = root.join("paired");
        let shown =
            crate::pair::request(&dir, &identity, Some(PASS), KdfParams::MIN, 1_000).unwrap();
        let link = crate::pair::answer(&primary, &shown.link, &shown.sas, 1_000).unwrap();
        crate::pair::accept(&dir, &link, Some(PASS), 1_000).unwrap();
        (
            dir,
            identity,
            crate::pair::certificate_from_link(&link).unwrap(),
        )
    }

    fn paired_load(dir: &Path) -> Result<(LocalIdentity, DeviceKey), Box<dyn std::error::Error>> {
        load(dir, Some(PASS), false, KdfParams::MIN)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn a_paired_directory_opens_as_the_primary_identity_and_creates_no_key() {
        let root = tempfile::tempdir().unwrap();
        let (dir, identity, _) = paired_dir(root.path());
        let files_before = names(&dir);
        let device_bytes = std::fs::read(dev_path(&dir)).unwrap();
        let id_file = std::fs::read(dir.join(crate::pair::PRIMARY_ID_FILE)).unwrap();

        let (loaded, device) = paired_load(&dir).unwrap();

        assert!(matches!(loaded, LocalIdentity::Paired(_)));
        assert_eq!(loaded.id(), identity);
        assert!(
            !matches!(loaded, LocalIdentity::Master(_)),
            "pareado com a mestra"
        );
        assert_eq!(names(&dir), files_before, "criou ou apagou arquivo");
        assert!(!id_path(&dir).exists() && !dir.join(AUTHORITY_KEY_FILE).exists());
        assert_eq!(std::fs::read(dev_path(&dir)).unwrap(), device_bytes);
        assert_eq!(
            std::fs::read(dir.join(crate::pair::PRIMARY_ID_FILE)).unwrap(),
            id_file
        );
        // reabrir dá o mesmo device
        assert_eq!(paired_load(&dir).unwrap().1.peer_id(), device.peer_id());
    }

    /// Auditoria da 2d-ii-b, I1: sem `primary.id` (apagado pelo usuário) o diretório pareado parecia novo e o
    /// `load` gerava uma mestra. Estado (`mls.sqlite`) sem nenhuma identidade nunca é um diretório novo.
    #[test]
    fn state_without_any_identity_is_not_a_new_directory() {
        let root = tempfile::tempdir().unwrap();
        let (dir, _, _) = paired_dir(root.path());
        std::fs::remove_file(dir.join(crate::pair::PRIMARY_ID_FILE)).unwrap();
        let before = names(&dir);

        assert!(paired_load(&dir).is_err());

        assert!(!id_path(&dir).exists(), "gerou uma mestra");
        assert_eq!(names(&dir), before, "criou ou apagou arquivo");
    }

    #[test]
    fn a_directory_with_a_pending_request_is_also_recognized_as_paired_mode() {
        // Antes do `accept`, só há `primary.id` + `device.key` + banco pendente: o `load` não pode cair no
        // ramo "diretório novo" e gerar uma mestra; quem recusa depois é o `Chat::open_paired`.
        let root = tempfile::tempdir().unwrap();
        let (_, identity) = cold_init(root.path());
        let dir = root.path().join("pending");
        crate::pair::request(&dir, &identity, Some(PASS), KdfParams::MIN, 1_000).unwrap();

        let (loaded, _) = paired_load(&dir).unwrap();

        assert_eq!(loaded.id(), identity);
        assert!(!id_path(&dir).exists(), "gerou uma mestra");
    }

    #[test]
    fn paired_mode_refuses_what_it_must_never_mix() {
        let root = tempfile::tempdir().unwrap();
        let (dir, _, _) = paired_dir(root.path());
        // mestra, autoridade ou certificado de autoridade no mesmo diretório
        for name in ["identity.key", AUTHORITY_KEY_FILE, AUTHORITY_CERT_FILE] {
            std::fs::write(dir.join(name), b"x").unwrap();
            assert!(paired_load(&dir).is_err(), "aceitou com {name}");
            std::fs::remove_file(dir.join(name)).unwrap();
        }
        // senha errada
        assert!(load(&dir, Some(b"senha errada"), false, KdfParams::MIN).is_err());
        // ainda abre depois de tudo isso
        assert!(paired_load(&dir).is_ok());
    }

    #[test]
    fn paired_mode_without_a_device_key_fails_and_never_creates_one() {
        let root = tempfile::tempdir().unwrap();
        let (dir, _, _) = paired_dir(root.path());
        std::fs::remove_file(dev_path(&dir)).unwrap();
        assert!(paired_load(&dir).is_err());
        assert!(!dev_path(&dir).exists(), "criou device.key");
        assert!(!id_path(&dir).exists());
    }

    #[test]
    fn paired_mode_with_an_unreadable_primary_id_fails() {
        let root = tempfile::tempdir().unwrap();
        let (dir, _, _) = paired_dir(root.path());
        for bad in [
            "",
            "lixo",
            "12D3KooWnaoeumaidentidadevalidaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            std::fs::write(dir.join(crate::pair::PRIMARY_ID_FILE), bad).unwrap();
            assert!(paired_load(&dir).is_err(), "aceitou primary.id {bad:?}");
        }
        assert!(!id_path(&dir).exists());
    }

    #[test]
    fn a_paired_device_only_runs_with_a_certificate_issued_by_an_authority() {
        let root = tempfile::tempdir().unwrap();
        let (dir, identity, v4) = paired_dir(root.path());
        let (loaded, _) = paired_load(&dir).unwrap();
        // v4 (da autoridade): passa
        ensure_current_issuer(&loaded, &v4).unwrap();
        // v3 (da mestra, como a de um banco criado pelo modo antigo): recusa
        let master = StandaloneIdentity::generate();
        let device = DeviceKey::generate();
        let signing = DeviceKey::generate().public_key();
        let v3 = master
            .authorize_device(
                &signing,
                &device.public_key(),
                &device.nostr_public_key(),
                1,
            )
            .unwrap();
        assert!(matches!(v3.issuer, Issuer::Master(_)));
        assert!(ensure_current_issuer(&LocalIdentity::Paired(identity), &v3).is_err());
    }

    // ---- troca de senha de um diretório (P13, `kin passwd`) ----

    const NEW_PASS: &[u8] = b"outra senha";

    /// Bytes de cada arquivo de chave que existe: para provar que nada mudou.
    fn key_bytes(dir: &Path) -> Vec<(String, Vec<u8>)> {
        SEALED_KEY_FILES
            .iter()
            .filter(|name| dir.join(name).exists())
            .map(|name| (name.to_string(), std::fs::read(dir.join(name)).unwrap()))
            .collect()
    }

    fn db_key_bytes(dir: &Path, pass: &[u8]) -> [u8; 32] {
        *kin_identity::DbKey::load_with(&dir.join(DB_KEY_FILE), pass)
            .unwrap()
            .expose_secret()
    }

    /// Diretório Master com `identity.key`, `device.key` e `db.key`, todos na senha `PASS`.
    fn master_dir(dir: &Path) -> IdentityId {
        let (loaded, _) = load(dir, Some(PASS), false, KdfParams::MIN).unwrap();
        open_db_key(dir, Some(PASS), false, KdfParams::MIN)
            .unwrap()
            .unwrap();
        loaded.id()
    }

    fn change(dir: &Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        change_passphrase(dir, PASS, NEW_PASS, KdfParams::MIN)
    }

    #[test]
    fn changing_the_passphrase_of_a_master_dir_keeps_identity_device_and_db_key() {
        let dir = tempfile::tempdir().unwrap();
        let id = master_dir(dir.path());
        let (_, device_before) = load(dir.path(), Some(PASS), false, KdfParams::MIN).unwrap();
        let db_before = db_key_bytes(dir.path(), PASS);

        let changed = change(dir.path()).unwrap();

        assert_eq!(changed, ["identity.key", "device.key", DB_KEY_FILE]);
        let (loaded, device) = load(dir.path(), Some(NEW_PASS), false, KdfParams::MIN).unwrap();
        assert_eq!(loaded.id(), id);
        assert_eq!(device.peer_id(), device_before.peer_id());
        assert_eq!(db_key_bytes(dir.path(), NEW_PASS), db_before);
    }

    #[test]
    fn after_the_change_the_old_passphrase_opens_nothing() {
        let dir = tempfile::tempdir().unwrap();
        master_dir(dir.path());
        change(dir.path()).unwrap();
        assert!(load(dir.path(), Some(PASS), false, KdfParams::MIN).is_err());
        assert!(kin_identity::DbKey::load_with(&dir.path().join(DB_KEY_FILE), PASS).is_err());
    }

    #[test]
    fn changing_the_passphrase_of_a_primary_dir_keeps_the_identity() {
        let root = tempfile::tempdir().unwrap();
        let (primary, identity) = cold_init(root.path());
        primary_load(&primary).unwrap();
        open_db_key(&primary, Some(PASS), false, KdfParams::MIN)
            .unwrap()
            .unwrap();

        let changed = change(&primary).unwrap();

        assert_eq!(changed, [AUTHORITY_KEY_FILE, "device.key", DB_KEY_FILE]);
        let (loaded, _) = load(&primary, Some(NEW_PASS), false, KdfParams::MIN).unwrap();
        assert!(matches!(loaded, LocalIdentity::Primary(_)));
        assert_eq!(loaded.id(), identity);
    }

    #[test]
    fn changing_the_passphrase_of_a_paired_dir_keeps_the_identity() {
        let root = tempfile::tempdir().unwrap();
        let (dir, identity, _) = paired_dir(root.path());
        change(&dir).unwrap();
        let (loaded, _) = load(&dir, Some(NEW_PASS), false, KdfParams::MIN).unwrap();
        assert!(matches!(loaded, LocalIdentity::Paired(_)));
        assert_eq!(loaded.id(), identity);
        assert!(load(&dir, Some(PASS), false, KdfParams::MIN).is_err());
    }

    #[test]
    fn a_wrong_old_passphrase_changes_no_file() {
        let dir = tempfile::tempdir().unwrap();
        master_dir(dir.path());
        let before = key_bytes(dir.path());
        assert!(change_passphrase(dir.path(), b"errada", NEW_PASS, KdfParams::MIN).is_err());
        assert_eq!(key_bytes(dir.path()), before);
    }

    /// Um arquivo que não abre nem com a velha nem com a nova trava a troca **antes** de gravar o primeiro.
    #[test]
    fn one_unreadable_file_stops_the_change_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        master_dir(dir.path());
        let db_path = dir.path().join(DB_KEY_FILE);
        std::fs::remove_file(&db_path).unwrap();
        kin_identity::DbKey::generate()
            .save_sealed(&db_path, b"terceira senha", KdfParams::MIN)
            .unwrap();
        let before = key_bytes(dir.path());
        assert!(change(dir.path()).is_err());
        assert_eq!(key_bytes(dir.path()), before);
    }

    /// Queda no meio: o `device.key` já está na senha nova, o resto na velha. Repetir termina o serviço.
    #[test]
    fn repeating_the_change_after_an_interruption_finishes_it() {
        let dir = tempfile::tempdir().unwrap();
        let id = master_dir(dir.path());
        kin_identity::rekey_file(
            &dir.path().join("device.key"),
            PASS,
            NEW_PASS,
            KdfParams::MIN,
        )
        .unwrap();

        let changed = change(dir.path()).unwrap();

        assert_eq!(changed, ["identity.key", DB_KEY_FILE]);
        let (loaded, _) = load(dir.path(), Some(NEW_PASS), false, KdfParams::MIN).unwrap();
        assert_eq!(loaded.id(), id);
        assert_eq!(db_key_bytes(dir.path(), NEW_PASS).len(), 32);
        // Uma terceira vez não encontra nada para trocar nem grava.
        let before = key_bytes(dir.path());
        assert!(change(dir.path()).unwrap().is_empty());
        assert_eq!(key_bytes(dir.path()), before);
    }

    #[test]
    fn a_plain_key_file_refuses_the_change_and_points_to_protect() {
        let dir = tempfile::tempdir().unwrap();
        make_plain(dir.path());
        let before = key_bytes(dir.path());
        let err = change(dir.path()).unwrap_err().to_string();
        assert!(err.contains("--protect"), "{err}");
        assert_eq!(key_bytes(dir.path()), before);
    }

    #[test]
    fn an_empty_or_equal_new_passphrase_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        master_dir(dir.path());
        let before = key_bytes(dir.path());
        assert!(change_passphrase(dir.path(), PASS, b"", KdfParams::MIN).is_err());
        assert!(change_passphrase(dir.path(), PASS, PASS, KdfParams::MIN).is_err());
        assert_eq!(key_bytes(dir.path()), before);
    }

    #[test]
    fn a_directory_without_key_files_has_nothing_to_change() {
        let dir = tempfile::tempdir().unwrap();
        assert!(change(dir.path()).is_err());
    }
}
