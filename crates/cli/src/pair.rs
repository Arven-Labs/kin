//! `kin pair request | answer | accept`: pareamento autenticado de um device novo com o primário
//! (P30 fatia 2d-ii-a2, ADR 0020), **sem rede e sem `Chat`**, como o `kin cold`.
//!
//! Fluxo (links de texto; o transporte entre as máquinas é manual):
//! 1. primário: já mostra a sua identidade (base58) ao abrir; ela vai ao device novo **antes** do pedido;
//! 2. device novo: `kin pair request --identity BASE58` grava `primary.id`, gera as chaves e mostra o
//!    link do pedido e o código de comparação (SAS);
//! 3. primário: `kin pair answer --request ARQ` pede o SAS (digitado, lido da tela do device novo) e
//!    só então imprime o link do certificado v4;
//! 4. device novo: `kin pair accept --certificate ARQ|-` confere e instala o certificado.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use kin_crypto::PendingDevice;
use kin_identity::{
    AuthorityIdentity, DeviceCertificate, DeviceKey, Error as IdentityError, IdentityId, KdfParams,
    PairRequest,
};

/// Arquivo (texto, 0600) com a identidade do primário, recebida fora de banda **antes** do pedido.
/// Fica fora do `mls.sqlite` (ADR 0017: quem escreve no banco não troca a identidade esperada).
pub(crate) const PRIMARY_ID_FILE: &str = "primary.id";

pub(crate) const REQUEST_PREFIX: &str = "kin://pair-request/";
pub(crate) const CERTIFICATE_PREFIX: &str = "kin://pair-cert/";

/// Subcomando pedido, já com os caminhos.
#[derive(Debug, PartialEq, Eq)]
pub enum PairCommand {
    /// `kin pair request --data-dir DIR --identity BASE58`
    Request {
        data_dir: PathBuf,
        identity: String,
        passphrase_file: Option<PathBuf>,
    },
    /// `kin pair answer --data-dir DIR --request ARQUIVO` (o SAS é pedido no terminal)
    Answer {
        data_dir: PathBuf,
        request: PathBuf,
        passphrase_file: Option<PathBuf>,
    },
    /// `kin pair accept --data-dir DIR --certificate ARQUIVO|-`
    Accept {
        data_dir: PathBuf,
        certificate: String,
        passphrase_file: Option<PathBuf>,
    },
}

/// Lê os argumentos **depois** da palavra `pair`.
pub fn parse(argv: impl IntoIterator<Item = String>) -> Result<PairCommand, String> {
    let mut it = argv.into_iter();
    let sub = it.next().ok_or("uso: kin pair request|answer|accept ...")?;
    if !matches!(sub.as_str(), "request" | "answer" | "accept") {
        return Err(format!("subcomando desconhecido: pair {sub}"));
    }
    // A flag que carrega o dado de cada subcomando; as outras duas só valem nos seus.
    let own_flag = match sub.as_str() {
        "request" => "--identity",
        "answer" => "--request",
        _ => "--certificate",
    };
    let mut data_dir = None;
    let mut input = None;
    let mut passphrase_file = None;
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} precisa de um valor"));
        match flag.as_str() {
            "--data-dir" => data_dir = Some(PathBuf::from(value()?)),
            "--passphrase-file" => passphrase_file = Some(PathBuf::from(value()?)),
            f if f == own_flag => input = Some(value()?),
            other => {
                // Só o nome: `--passphrase=segredo` não pode ser ecoado no terminal.
                let name = other.split('=').next().unwrap_or(other);
                return Err(format!("argumento desconhecido em pair {sub}: {name}"));
            }
        }
    }
    let data_dir = data_dir.ok_or(format!("pair {sub} exige --data-dir"))?;
    let input = input.ok_or(format!("pair {sub} exige {own_flag}"))?;
    Ok(match sub.as_str() {
        "request" => PairCommand::Request {
            data_dir,
            identity: input,
            passphrase_file,
        },
        "answer" => {
            if input == "-" {
                return Err(
                    "pair answer: --request não aceita `-` (o stdin é do prompt do código); use um arquivo"
                        .into(),
                );
            }
            PairCommand::Answer {
                data_dir,
                request: PathBuf::from(input),
                passphrase_file,
            }
        }
        _ => PairCommand::Accept {
            data_dir,
            certificate: input,
            passphrase_file,
        },
    })
}

/// O que o device novo mostra depois de `request`.
#[derive(Debug, PartialEq, Eq)]
pub struct RequestShown {
    pub link: String,
    /// Código de comparação (24 dígitos), a ser digitado no primário.
    pub sas: String,
}

/// Teto do texto depois do prefixo (o pedido tem ≤ 1 KiB = ~1,4 mil caracteres; o certificado, bem menos).
const LINK_BODY_LIMIT: usize = 8192;

fn to_link(prefix: &str, bytes: &[u8]) -> String {
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// Tira o prefixo e decodifica. Sem detalhe no erro além do tipo de link: o conteúdo veio de fora.
fn link_bytes(prefix: &str, what: &str, link: &str) -> Result<Vec<u8>, String> {
    let invalid = || format!("{what} inválido (esperado {prefix}...)");
    let body = link.trim().strip_prefix(prefix).ok_or_else(invalid)?;
    if body.is_empty() || body.len() > LINK_BODY_LIMIT {
        return Err(invalid());
    }
    URL_SAFE_NO_PAD.decode(body).map_err(|_| invalid())
}

pub fn request_to_link(request: &PairRequest) -> String {
    to_link(REQUEST_PREFIX, &request.to_bytes())
}

pub fn request_from_link(link: &str) -> Result<PairRequest, String> {
    let bytes = link_bytes(REQUEST_PREFIX, "link do pedido de pareamento", link)?;
    PairRequest::from_bytes(&bytes).map_err(|e| e.to_string())
}

pub fn certificate_to_link(certificate: &DeviceCertificate) -> String {
    to_link(CERTIFICATE_PREFIX, &certificate.to_bytes())
}

pub fn certificate_from_link(link: &str) -> Result<DeviceCertificate, String> {
    let bytes = link_bytes(CERTIFICATE_PREFIX, "link do certificado", link)?;
    DeviceCertificate::from_bytes(&bytes).map_err(|e| e.to_string())
}

/// Lê `primary.id` (texto pequeno) e o converte; erro sem ecoar o conteúdo.
fn read_primary_id(dir: &Path) -> Result<Option<IdentityId>, String> {
    let path = dir.join(PRIMARY_ID_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("ler {}: {e}", path.display())),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| format!("{} corrompido: peça o pareamento de novo", path.display()))?;
    IdentityId::from_base58(text)
        .map(Some)
        .map_err(|_| format!("{} corrompido: peça o pareamento de novo", path.display()))
}

/// Device novo: grava `primary.id`, abre (ou cria) o `device.key` e o `mls.sqlite` pendente e devolve o
/// pedido guardado. Repetir devolve o mesmo pedido até vencer (ADR 0019).
pub fn request(
    dir: &Path,
    identity: &IdentityId,
    passphrase: Option<&[u8]>,
    params: KdfParams,
    now: u64,
) -> Result<RequestShown, String> {
    // Diretório de primário ou de mestra nunca vira device pareado (e aqui não se cria nada nele).
    for name in ["authority.key", "authority.cert", "identity.key"] {
        if dir
            .join(name)
            .symlink_metadata()
            .map_or_else(|e| e.kind() != std::io::ErrorKind::NotFound, |_| true)
        {
            return Err(format!(
                "este --data-dir tem {name}: use um diretório novo para o device a parear"
            ));
        }
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // A identidade esperada vem de fora de banda e é gravada ANTES do pedido; trocar de primário num
    // diretório que já tem uma é erro (apague o diretório), nunca sobrescrita silenciosa.
    match read_primary_id(dir)? {
        Some(recorded) if &recorded != identity => {
            return Err(format!(
                "este --data-dir já está ligado à identidade {}: use um diretório novo para outro primário",
                recorded.to_base58()
            ));
        }
        Some(_) => {}
        None => {}
    }
    let device = crate::keys::load_or_create_device(&dir.join("device.key"), passphrase, params)
        .map_err(|e| e.to_string())?;
    let pending = PendingDevice::open(&dir.join("mls.sqlite")).map_err(|e| e.to_string())?;
    let request = pending
        .request_at(&device, now)
        .map_err(|e| e.to_string())?;
    Ok(RequestShown {
        link: request_to_link(&request),
        sas: request.sas_code(identity),
    })
}

/// Primário: confere o pedido e só certifica se `typed_sas` for o código do pedido. Devolve o link do
/// certificado.
pub fn answer(
    primary: &AuthorityIdentity,
    request_link: &str,
    typed_sas: &str,
    now: u64,
) -> Result<String, String> {
    let request = request_from_link(request_link)?;
    request.verify().map_err(|e| e.to_string())?;
    let certificate = primary
        .answer_pair_request(&request, typed_sas.trim(), now)
        .map_err(|e| match e {
            // O pedido já foi verificado acima: sobra o código digitado.
            IdentityError::InvalidPairRequest => {
                "o código de comparação não confere com o do pedido".to_string()
            }
            other => other.to_string(),
        })?;
    Ok(certificate_to_link(&certificate))
}

/// Device novo: instala o certificado, conferindo a identidade de `primary.id` (nunca a do certificado).
pub fn accept(
    dir: &Path,
    certificate_link: &str,
    passphrase: Option<&[u8]>,
    now: u64,
) -> Result<(), String> {
    // Tudo o que não precisa do banco é conferido antes de abri-lo (abrir cria o arquivo se faltar).
    let expected = read_primary_id(dir)?.ok_or(format!(
        "falta {PRIMARY_ID_FILE} neste --data-dir: rode `kin pair request` antes"
    ))?;
    let certificate = certificate_from_link(certificate_link)?;
    let device =
        DeviceKey::load_with(&dir.join("device.key"), passphrase).map_err(|e| e.to_string())?;
    let pending = PendingDevice::open(&dir.join("mls.sqlite")).map_err(|e| e.to_string())?;
    // `expected` vem do `primary.id`, nunca do certificado (ADR 0016, item 4).
    pending
        .install_certificate_at(&device, &certificate, &expected, now)
        .map(drop)
        .map_err(|e| e.to_string())
}

fn now_secs() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| e.to_string())
}

/// Senha do arquivo da flag, senão de `KIN_PASSPHRASE_FILE`, senão do terminal.
fn secret(
    flag: Option<PathBuf>,
    label: &str,
    confirm: bool,
) -> Result<zeroize::Zeroizing<Vec<u8>>, String> {
    let file = flag.or_else(|| {
        std::env::var("KIN_PASSPHRASE_FILE")
            .ok()
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    });
    crate::cold::passphrase(file, label, confirm)
}

/// Executa o subcomando (senhas e terminal ficam aqui; a lógica está nas funções acima).
pub fn run(command: PairCommand) -> Result<(), String> {
    let read_link = |source: &str| crate::read_pair_link(source).map_err(|e| e.to_string());
    match command {
        PairCommand::Request {
            data_dir,
            identity,
            passphrase_file,
        } => {
            let identity = IdentityId::from_base58(&identity).map_err(|e| e.to_string())?;
            let confirm = !data_dir.join("device.key").exists();
            let pass = secret(passphrase_file, "senha do device", confirm)?;
            let shown = request(
                &data_dir,
                &identity,
                Some(pass.as_slice()),
                KdfParams::DEFAULT,
                now_secs()?,
            )?;
            println!(
                "Pedido de pareamento (válido por 15 minutos):\n{}",
                shown.link
            );
            println!(
                "\nCódigo de comparação: {}\nNo primário, rode `kin pair answer` com este link num arquivo e digite o código acima.",
                shown.sas
            );
        }
        PairCommand::Answer {
            data_dir,
            request,
            passphrase_file,
        } => {
            let link = read_link(&request.to_string_lossy())?;
            let pass = secret(passphrase_file, "senha da autoridade", false)?;
            let primary = crate::keys::load_primary_identity(&data_dir, Some(pass.as_slice()))
                .map_err(|e| e.to_string())?;
            println!(
                "Primário da identidade {}.\nDigite o código de comparação que o device novo mostra (compare com a tela dele):",
                kin_identity::IdentityProvider::id(&primary).to_base58()
            );
            let mut typed = String::new();
            std::io::stdin()
                .read_line(&mut typed)
                .map_err(|e| e.to_string())?;
            let certificate = answer(&primary, &link, &typed, now_secs()?)?;
            println!("Certificado (leve ao device novo e rode `kin pair accept`):\n{certificate}");
        }
        PairCommand::Accept {
            data_dir,
            certificate,
            passphrase_file,
        } => {
            let link = read_link(&certificate)?;
            let pass = secret(passphrase_file, "senha do device", false)?;
            accept(&data_dir, &link, Some(pass.as_slice()), now_secs()?)?;
            println!(
                "Certificado instalado: este device agora pertence à identidade registrada em {PRIMARY_ID_FILE}.\n(O chat em modo pareado vem na próxima fatia.)"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cold;
    use kin_crypto::CryptoDevice;
    use kin_identity::{AuthorityCertificate, AuthorityKey, DeviceKey, IdentityProvider};

    const PASS: &[u8] = b"senha do device";
    const MASTER_PASS: &[u8] = b"senha da mestra fria";
    const AUTH_PASS: &[u8] = b"senha da autoridade";
    const NOW: u64 = 1_700_000_000;
    /// Prazo do pedido: 900 s (ADR 0019).
    const TTL: u64 = kin_crypto::PAIR_REQUEST_TTL_SECS;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// Um primário de verdade (mestra fria + autoridade), pelo mesmo `cold::init` da CLI.
    fn primary_in(dir: &Path) -> (AuthorityIdentity, IdentityId) {
        let outcome = cold::init(
            &dir.join("offline/cold-master.key"),
            &dir.join("primary"),
            MASTER_PASS,
            AUTH_PASS,
            KdfParams::MIN,
            NOW,
        )
        .unwrap();
        let key =
            AuthorityKey::load_with(&dir.join("primary/authority.key"), Some(AUTH_PASS)).unwrap();
        let cert = AuthorityCertificate::from_bytes(
            &std::fs::read(dir.join("primary/authority.cert")).unwrap(),
        )
        .unwrap();
        (AuthorityIdentity::new(key, cert).unwrap(), outcome.identity)
    }

    fn ask(dir: &Path, identity: &IdentityId, now: u64) -> Result<RequestShown, String> {
        request(dir, identity, Some(PASS), KdfParams::MIN, now)
    }

    /// O intruso certifica o pedido interceptado digitando o SAS calculado com a **identidade dele**
    /// (com o SAS da identidade verdadeira o próprio `answer` recusa: o SAS cobre a identidade).
    fn forge(intruder: &AuthorityIdentity, shown: &RequestShown) -> String {
        let req = request_from_link(&shown.link).unwrap();
        answer(intruder, &shown.link, &req.sas_code(&intruder.id()), NOW).unwrap()
    }

    #[test]
    fn answer_with_the_sas_of_another_identity_is_refused() {
        // O SAS cobre a identidade: o código mostrado no device novo (da identidade esperada) não
        // serve a um primário de outra identidade.
        let dir = tempfile::tempdir().unwrap();
        let (_, identity) = primary_in(dir.path());
        let other_dir = tempfile::tempdir().unwrap();
        let (intruder, _) = primary_in(other_dir.path());
        let shown = ask(&dir.path().join("new"), &identity, NOW).unwrap();
        assert!(answer(&intruder, &shown.link, &shown.sas, NOW).is_err());
    }

    // ---- parse ----

    #[test]
    fn parse_request_answer_and_accept() {
        let request = parse(args(&[
            "request",
            "--data-dir",
            "d",
            "--identity",
            "12D3KooWabc",
            "--passphrase-file",
            "p",
        ]))
        .unwrap();
        assert_eq!(
            request,
            PairCommand::Request {
                data_dir: "d".into(),
                identity: "12D3KooWabc".into(),
                passphrase_file: Some("p".into()),
            }
        );
        let answer = parse(args(&["answer", "--data-dir", "d", "--request", "r.txt"])).unwrap();
        assert_eq!(
            answer,
            PairCommand::Answer {
                data_dir: "d".into(),
                request: "r.txt".into(),
                passphrase_file: None,
            }
        );
        let accept = parse(args(&["accept", "--data-dir", "d", "--certificate", "-"])).unwrap();
        assert_eq!(
            accept,
            PairCommand::Accept {
                data_dir: "d".into(),
                certificate: "-".into(),
                passphrase_file: None,
            }
        );
    }

    #[test]
    fn parse_refuses_missing_required_flags_and_unknown_input() {
        for bad in [
            vec![],
            vec!["bogus"],
            vec!["request", "--data-dir", "d"],
            vec!["request", "--identity", "x"],
            vec!["answer", "--data-dir", "d"],
            vec!["accept", "--data-dir", "d"],
            vec!["accept", "--data-dir", "d", "--certificate"],
            vec![
                "accept",
                "--data-dir",
                "d",
                "--certificate",
                "c",
                "--surprise",
            ],
            // cada subcomando só aceita as suas flags
            vec![
                "answer",
                "--data-dir",
                "d",
                "--request",
                "r",
                "--identity",
                "x",
            ],
            vec![
                "request",
                "--data-dir",
                "d",
                "--identity",
                "x",
                "--certificate",
                "c",
            ],
        ] {
            assert!(parse(args(&bad)).is_err(), "aceitou {bad:?}");
        }
    }

    #[test]
    fn answer_refuses_stdin_for_the_request_because_the_sas_prompt_needs_it() {
        assert!(parse(args(&["answer", "--data-dir", "d", "--request", "-"])).is_err());
    }

    #[test]
    fn a_parse_error_never_echoes_a_value_after_an_equals_sign() {
        let err = parse(args(&["accept", "--passphrase=segredo123"])).unwrap_err();
        assert!(!err.contains("segredo123"), "ecoou o valor: {err}");
    }

    // ---- links ----

    #[test]
    fn request_and_certificate_links_round_trip_with_their_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let (primary, identity) = primary_in(dir.path());
        let shown = ask(&dir.path().join("new"), &identity, NOW).unwrap();
        assert!(shown.link.starts_with(REQUEST_PREFIX));
        let req = request_from_link(&shown.link).unwrap();
        assert_eq!(request_to_link(&req), shown.link);

        let cert_link = answer(&primary, &shown.link, &shown.sas, NOW).unwrap();
        assert!(cert_link.starts_with(CERTIFICATE_PREFIX));
        let cert = certificate_from_link(&cert_link).unwrap();
        assert_eq!(certificate_to_link(&cert), cert_link);
    }

    #[test]
    fn links_with_the_wrong_prefix_garbage_or_trailing_bytes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (_, identity) = primary_in(dir.path());
        let shown = ask(&dir.path().join("new"), &identity, NOW).unwrap();
        let body = shown.link.strip_prefix(REQUEST_PREFIX).unwrap();
        let bad_links = [
            String::new(),
            "kin://pair-request/".to_string(),
            "kin://pair-request/!!!não é base64!!!".to_string(),
            "kin://pair-request/AAAA".to_string(),
            format!("kin://pair-cert/{body}"),
            format!("kin://invite/{body}"),
            format!("{}{}", shown.link, "A"),
            format!("kin://pair-request/{}", "A".repeat(100_000)),
        ];
        for bad in &bad_links {
            assert!(request_from_link(bad).is_err(), "aceitou {bad:.40}");
        }
        assert!(certificate_from_link(&shown.link).is_err());
        assert!(certificate_from_link("kin://pair-cert/AAAA").is_err());
    }

    #[test]
    fn a_link_read_tolerates_surrounding_whitespace_and_newline() {
        let dir = tempfile::tempdir().unwrap();
        let (_, identity) = primary_in(dir.path());
        let shown = ask(&dir.path().join("new"), &identity, NOW).unwrap();
        let padded = format!("  {}\n", shown.link);
        assert!(request_from_link(&padded).is_ok());
    }

    // ---- request (device novo) ----

    #[test]
    fn request_writes_primary_id_device_key_and_a_pending_database() {
        let dir = tempfile::tempdir().unwrap();
        let (_, identity) = primary_in(dir.path());
        let new = dir.path().join("new");
        let shown = ask(&new, &identity, NOW).unwrap();

        assert_eq!(
            std::fs::read_to_string(new.join(PRIMARY_ID_FILE))
                .unwrap()
                .trim(),
            identity.to_base58()
        );
        assert!(new.join("device.key").exists());
        assert!(new.join("mls.sqlite").exists());
        // o código mostrado é o do pedido, calculado com a identidade recebida fora de banda
        let req = request_from_link(&shown.link).unwrap();
        req.verify().unwrap();
        assert_eq!(shown.sas, req.sas_code(&identity));
        assert_eq!(shown.sas.len(), 27, "24 dígitos em 4 grupos de 6 com hífen");
    }

    #[test]
    fn request_files_are_private_and_the_device_key_is_sealed() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let (_, identity) = primary_in(dir.path());
        let new = dir.path().join("new");
        ask(&new, &identity, NOW).unwrap();
        let mode = |name: &str| {
            std::fs::metadata(new.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(PRIMARY_ID_FILE), 0o600);
        assert_eq!(mode("device.key"), 0o600);
        assert!(kin_identity::is_sealed(
            &std::fs::read(new.join("device.key")).unwrap()
        ));
    }

    #[test]
    fn repeating_request_shows_the_same_link_and_code_until_it_expires() {
        let dir = tempfile::tempdir().unwrap();
        let (_, identity) = primary_in(dir.path());
        let new = dir.path().join("new");
        let first = ask(&new, &identity, NOW).unwrap();
        assert_eq!(
            ask(&new, &identity, NOW + TTL).unwrap(),
            first,
            "900 s ainda vale"
        );
        let renewed = ask(&new, &identity, NOW + TTL + 1).unwrap();
        assert_ne!(renewed.link, first.link, "901 s: pedido novo");
        assert_ne!(renewed.sas, first.sas);
    }

    #[test]
    fn request_refuses_a_different_identity_than_the_one_already_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let (_, identity) = primary_in(dir.path());
        let new = dir.path().join("new");
        ask(&new, &identity, NOW).unwrap();
        let other = kin_identity::StandaloneIdentity::generate().id();
        assert!(ask(&new, &other, NOW).is_err());
        assert_eq!(
            std::fs::read_to_string(new.join(PRIMARY_ID_FILE))
                .unwrap()
                .trim(),
            identity.to_base58(),
            "primary.id foi sobrescrito"
        );
    }

    #[test]
    fn request_refuses_primary_master_and_already_paired_directories() {
        let dir = tempfile::tempdir().unwrap();
        let (primary, identity) = primary_in(dir.path());
        // diretório do primário (authority.key)
        assert!(ask(&dir.path().join("primary"), &identity, NOW).is_err());
        assert!(!dir.path().join("primary/device.key").exists());
        // diretório com a mestra (identity.key)
        let master_dir = dir.path().join("master");
        std::fs::create_dir_all(&master_dir).unwrap();
        kin_identity::StandaloneIdentity::generate()
            .save(&master_dir.join("identity.key"))
            .unwrap();
        assert!(ask(&master_dir, &identity, NOW).is_err());
        // diretório já pareado
        let new = dir.path().join("new");
        let shown = ask(&new, &identity, NOW).unwrap();
        let link = answer(&primary, &shown.link, &shown.sas, NOW).unwrap();
        accept(&new, &link, Some(PASS), NOW).unwrap();
        assert!(ask(&new, &identity, NOW).is_err());
    }

    // ---- answer (primário) ----

    #[test]
    fn answer_certifies_only_with_the_right_code() {
        let dir = tempfile::tempdir().unwrap();
        let (primary, identity) = primary_in(dir.path());
        let shown = ask(&dir.path().join("new"), &identity, NOW).unwrap();

        let wrong = "000000-000000-000000-000000";
        for typed in [
            "",
            wrong,
            &shown.sas.replace('-', ""),
            &format!("{}0", shown.sas),
        ] {
            assert!(
                answer(&primary, &shown.link, typed, NOW).is_err(),
                "emitiu com o código {typed:?}"
            );
        }
        // espaço e quebra de linha em volta do que foi digitado não contam
        assert!(answer(&primary, &shown.link, &format!(" {}\n", shown.sas), NOW).is_ok());
    }

    #[test]
    fn answer_refuses_a_request_whose_proofs_do_not_verify() {
        let dir = tempfile::tempdir().unwrap();
        let (primary, identity) = primary_in(dir.path());
        let shown = ask(&dir.path().join("new"), &identity, NOW).unwrap();
        let mut bytes = request_from_link(&shown.link).unwrap().to_bytes();
        let last = bytes.len() - 1;
        bytes[last] ^= 1; // estraga a prova Nostr
        let tampered = request_to_link(&PairRequest::from_bytes(&bytes).unwrap());
        assert!(answer(&primary, &tampered, &shown.sas, NOW).is_err());
        assert!(answer(&primary, "kin://pair-request/AAAA", &shown.sas, NOW).is_err());
    }

    #[test]
    fn answer_issues_a_v4_certificate_of_the_primary_identity_for_the_request_keys() {
        let dir = tempfile::tempdir().unwrap();
        let (primary, identity) = primary_in(dir.path());
        let shown = ask(&dir.path().join("new"), &identity, NOW).unwrap();
        let link = answer(&primary, &shown.link, &shown.sas, NOW).unwrap();
        let cert = certificate_from_link(&link).unwrap();
        cert.verify().unwrap();
        assert_eq!(cert.identity(), &identity);
        assert_eq!(cert.identity(), &primary.id());
        let req = request_from_link(&shown.link).unwrap();
        assert_eq!(cert.signing_key, req.signing_key);
        assert_eq!(cert.network_key, req.network_key);
        assert_eq!(cert.nostr_key, req.nostr_key);
        assert!(matches!(cert.issuer, kin_identity::Issuer::Authority(_)));
    }

    // ---- accept (device novo) ----

    #[test]
    fn the_full_flow_ends_with_a_device_that_reopens_as_paired() {
        let dir = tempfile::tempdir().unwrap();
        let (primary, identity) = primary_in(dir.path());
        let new = dir.path().join("new");
        let shown = ask(&new, &identity, NOW).unwrap();
        let link = answer(&primary, &shown.link, &shown.sas, NOW + 60).unwrap();
        accept(&new, &link, Some(PASS), NOW + 120).unwrap();

        let device = DeviceKey::load_with(&new.join("device.key"), Some(PASS)).unwrap();
        let paired = CryptoDevice::open_paired(
            &identity,
            &device.public_key(),
            &device.nostr_public_key(),
            &new.join("mls.sqlite"),
        )
        .unwrap();
        assert_eq!(paired.certificate().identity(), &identity);
    }

    #[test]
    fn accept_uses_primary_id_never_the_identity_of_the_returned_certificate() {
        // Um intruso devolve um certificado válido, das chaves certas, mas de OUTRA identidade.
        let dir = tempfile::tempdir().unwrap();
        let (_, identity) = primary_in(dir.path());
        let other_dir = tempfile::tempdir().unwrap();
        let (intruder, _) = primary_in(other_dir.path());
        let new = dir.path().join("new");
        let shown = ask(&new, &identity, NOW).unwrap();
        let forged = forge(&intruder, &shown);
        assert!(accept(&new, &forged, Some(PASS), NOW).is_err());
    }

    #[test]
    fn a_refused_accept_leaves_the_device_pending_and_the_right_certificate_still_installs() {
        let dir = tempfile::tempdir().unwrap();
        let (primary, identity) = primary_in(dir.path());
        let other_dir = tempfile::tempdir().unwrap();
        let (intruder, _) = primary_in(other_dir.path());
        let new = dir.path().join("new");
        let shown = ask(&new, &identity, NOW).unwrap();
        let forged = forge(&intruder, &shown);
        let real = answer(&primary, &shown.link, &shown.sas, NOW).unwrap();
        assert!(accept(&new, &forged, Some(PASS), NOW).is_err());
        assert!(accept(&new, "kin://pair-cert/AAAA", Some(PASS), NOW).is_err());
        accept(&new, &real, Some(PASS), NOW).unwrap();
    }

    #[test]
    fn accept_refuses_a_certificate_for_another_devices_request() {
        let dir = tempfile::tempdir().unwrap();
        let (primary, identity) = primary_in(dir.path());
        let a = ask(&dir.path().join("a"), &identity, NOW).unwrap();
        let b_dir = dir.path().join("b");
        ask(&b_dir, &identity, NOW).unwrap();
        let for_a = answer(&primary, &a.link, &a.sas, NOW).unwrap();
        assert!(accept(&b_dir, &for_a, Some(PASS), NOW).is_err());
    }

    #[test]
    fn accept_refuses_after_the_request_expired_and_the_old_certificate_needs_a_new_request() {
        let dir = tempfile::tempdir().unwrap();
        let (primary, identity) = primary_in(dir.path());
        let new = dir.path().join("new");
        let shown = ask(&new, &identity, NOW).unwrap();
        let link = answer(&primary, &shown.link, &shown.sas, NOW).unwrap();
        assert!(
            accept(&new, &link, Some(PASS), NOW + TTL + 1).is_err(),
            "901 s"
        );
        // 900 s ainda vale (limite inclusivo, como no ADR 0019)
        accept(&new, &link, Some(PASS), NOW + TTL).unwrap();
    }

    #[test]
    fn accept_without_primary_id_or_a_request_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (primary, identity) = primary_in(dir.path());
        let shown = ask(&dir.path().join("new"), &identity, NOW).unwrap();
        let link = answer(&primary, &shown.link, &shown.sas, NOW).unwrap();
        // diretório vazio
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(accept(&empty, &link, Some(PASS), NOW).is_err());
        // sem primary.id
        let new = dir.path().join("new");
        std::fs::remove_file(new.join(PRIMARY_ID_FILE)).unwrap();
        assert!(accept(&new, &link, Some(PASS), NOW).is_err());
    }

    #[test]
    fn accept_with_a_tampered_primary_id_refuses_the_real_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let (primary, identity) = primary_in(dir.path());
        let new = dir.path().join("new");
        let shown = ask(&new, &identity, NOW).unwrap();
        let link = answer(&primary, &shown.link, &shown.sas, NOW).unwrap();
        let other = kin_identity::StandaloneIdentity::generate().id();
        std::fs::write(new.join(PRIMARY_ID_FILE), other.to_base58()).unwrap();
        assert!(accept(&new, &link, Some(PASS), NOW).is_err());
    }

    #[test]
    fn accept_with_a_wrong_device_passphrase_fails_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (primary, identity) = primary_in(dir.path());
        let new = dir.path().join("new");
        let shown = ask(&new, &identity, NOW).unwrap();
        let link = answer(&primary, &shown.link, &shown.sas, NOW).unwrap();
        assert!(accept(&new, &link, Some(b"senha errada"), NOW).is_err());
        accept(&new, &link, Some(PASS), NOW).unwrap();
    }
}
