//! `kin cold ...`: comandos **offline** da custódia da mestra (P30 fatia 1b, ADR 0011 opção D).
//!
//! A mestra fria só assina; quem trabalha no dia a dia é a chave de autoridade (ADR 0013). Nada aqui
//! abre rede nem `Chat`: só gera chaves e grava arquivos cifrados (ADR 0012).

use std::path::{Path, PathBuf};

use kin_identity::{AuthorityKey, IdentityId, IdentityProvider, KdfParams, StandaloneIdentity};
use zeroize::Zeroizing;

const AUTHORITY_KEY_FILE: &str = "authority.key";
const AUTHORITY_CERT_FILE: &str = "authority.cert";

/// Subcomando pedido, já com os caminhos.
#[derive(Debug, PartialEq, Eq)]
pub enum ColdCommand {
    /// `kin cold init --master-out ARQ --authority-dir DIR`
    Init {
        master_out: PathBuf,
        authority_dir: PathBuf,
        master_passphrase_file: Option<PathBuf>,
        passphrase_file: Option<PathBuf>,
    },
    /// `kin cold rotate-authority --master ARQ --authority-dir DIR`
    Rotate {
        master: PathBuf,
        authority_dir: PathBuf,
        master_passphrase_file: Option<PathBuf>,
        passphrase_file: Option<PathBuf>,
        /// `--expect-identity BASE58`: recusa se a mestra aberta for de outra identidade.
        expect_identity: Option<String>,
    },
}

/// Lê os argumentos **depois** da palavra `cold`.
pub fn parse(argv: impl IntoIterator<Item = String>) -> Result<ColdCommand, String> {
    let mut it = argv.into_iter();
    let sub = it.next().ok_or("uso: kin cold init|rotate-authority ...")?;
    let (master_flag, other_master_flag) = match sub.as_str() {
        "init" => ("--master-out", "--master"),
        "rotate-authority" => ("--master", "--master-out"),
        other => return Err(format!("subcomando desconhecido: cold {other}")),
    };
    let mut master = None;
    let mut authority_dir = None;
    let mut master_passphrase_file = None;
    let mut passphrase_file = None;
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .map(PathBuf::from)
                .ok_or(format!("{flag} precisa de um valor"))
        };
        match flag.as_str() {
            f if f == master_flag => master = Some(value()?),
            "--authority-dir" => authority_dir = Some(value()?),
            "--master-passphrase-file" => master_passphrase_file = Some(value()?),
            "--passphrase-file" => passphrase_file = Some(value()?),
            f if f == other_master_flag => {
                return Err(format!("{f} não existe em cold {sub}: use {master_flag}"));
            }
            other => return Err(format!("argumento desconhecido: {other}")),
        }
    }
    let master = master.ok_or(format!("cold {sub} exige {master_flag}"))?;
    let authority_dir = authority_dir.ok_or(format!("cold {sub} exige --authority-dir"))?;
    Ok(if sub == "init" {
        ColdCommand::Init {
            master_out: master,
            authority_dir,
            master_passphrase_file,
            passphrase_file,
        }
    } else {
        ColdCommand::Rotate {
            master,
            authority_dir,
            master_passphrase_file,
            passphrase_file,
            expect_identity: None,
        }
    })
}

/// O que o usuário precisa ver depois de rodar um comando.
#[derive(Debug)]
pub struct Outcome {
    pub identity: IdentityId,
}

/// Gera a mestra e uma autoridade nova; grava `master_out` (mestra, cifrada com `master_pass`) e,
/// em `authority_dir`, `authority.key` (cifrada com `authority_pass`) e `authority.cert`.
pub fn init(
    master_out: &Path,
    authority_dir: &Path,
    master_pass: &[u8],
    authority_pass: &[u8],
    params: KdfParams,
    now: u64,
) -> Result<Outcome, String> {
    // Tudo é conferido antes de criar qualquer arquivo ou diretório.
    check_targets(master_out, authority_dir, master_pass, authority_pass)?;
    if master_out.exists() {
        return Err(format!(
            "{} já existe: não sobrescrevo uma mestra",
            master_out.display()
        ));
    }
    let master = StandaloneIdentity::generate();
    if let Some(parent) = master_out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    master
        .save_sealed(master_out, master_pass, params)
        .map_err(|e| format!("gravar a mestra: {e}"))?;
    write_authority(&master, authority_dir, authority_pass, params, now)
}

/// A mestra fria existente certifica uma autoridade **nova** (mesmo `IdentityId`) em `authority_dir`.
pub fn rotate(
    master_file: &Path,
    authority_dir: &Path,
    master_pass: &[u8],
    authority_pass: &[u8],
    params: KdfParams,
    now: u64,
    _expect_identity: Option<&str>,
) -> Result<Outcome, String> {
    check_targets(master_file, authority_dir, master_pass, authority_pass)?;
    // Senha errada falha aqui, antes de criar qualquer coisa.
    let master = StandaloneIdentity::load_with(master_file, Some(master_pass))
        .map_err(|e| format!("abrir a mestra {}: {e}", master_file.display()))?;
    write_authority(&master, authority_dir, authority_pass, params, now)
}

/// Conferência só de **caminhos** (mestra dentro do diretório do primário, autoridade já existente),
/// para o `run` recusar antes de pedir as senhas. Resolve `.`, `..` e links (auditoria 1b, I1).
#[allow(dead_code)] // só no stub; a implementação usa
fn check_paths(_master_file: &Path, _authority_dir: &Path) -> Result<(), String> {
    todo!()
}

/// Recusas que não dependem de abrir nada: senhas iguais, mestra dentro do diretório do primário e
/// autoridade já existente. A conferência do "dentro de" é lexical (não resolve `..` nem links).
fn check_targets(
    master_file: &Path,
    authority_dir: &Path,
    master_pass: &[u8],
    authority_pass: &[u8],
) -> Result<(), String> {
    if master_pass == authority_pass {
        return Err(
            "a senha da autoridade deve ser diferente da senha da mestra fria: a do primário não pode abrir a mestra"
                .into(),
        );
    }
    if master_file.starts_with(authority_dir) {
        return Err(format!(
            "a mestra fria ({}) não pode ficar dentro do diretório do primário ({}): leve-a para fora dele",
            master_file.display(),
            authority_dir.display()
        ));
    }
    for name in [AUTHORITY_KEY_FILE, AUTHORITY_CERT_FILE] {
        let path = authority_dir.join(name);
        if path.exists() {
            return Err(format!("{} já existe: não sobrescrevo", path.display()));
        }
    }
    Ok(())
}

/// Gera uma autoridade nova, grava `authority.key` (cifrada) e `authority.cert` (assinado pela mestra).
fn write_authority(
    master: &StandaloneIdentity,
    authority_dir: &Path,
    authority_pass: &[u8],
    params: KdfParams,
    now: u64,
) -> Result<Outcome, String> {
    let authority = AuthorityKey::generate();
    let cert = master
        .certify_authority(&authority.public_key(), now)
        .map_err(|e| format!("certificar a autoridade: {e}"))?;
    std::fs::create_dir_all(authority_dir)
        .map_err(|e| format!("{}: {e}", authority_dir.display()))?;
    authority
        .save_sealed(
            &authority_dir.join(AUTHORITY_KEY_FILE),
            authority_pass,
            params,
        )
        .map_err(|e| format!("gravar a autoridade: {e}"))?;
    crate::write_private(&authority_dir.join(AUTHORITY_CERT_FILE), &cert.to_bytes())
        .map_err(|e| format!("gravar o certificado: {e}"))?;
    Ok(Outcome {
        identity: master.id(),
    })
}

/// Executa um subcomando: lê as senhas (arquivo ou prompt), roda e imprime o resultado.
pub fn run(command: ColdCommand) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let (outcome, master_path, authority_dir) = match command {
        ColdCommand::Init {
            master_out,
            authority_dir,
            master_passphrase_file,
            passphrase_file,
        } => {
            eprintln!(
                "aviso: rode isto numa máquina sem rede; a mestra existe em claro na memória durante o comando"
            );
            let master_pass = passphrase(master_passphrase_file, "senha da mestra fria", true)?;
            let authority_pass = passphrase(passphrase_file, "senha da autoridade", true)?;
            let outcome = init(
                &master_out,
                &authority_dir,
                &master_pass,
                &authority_pass,
                KdfParams::DEFAULT,
                now,
            )?;
            (outcome, master_out, authority_dir)
        }
        ColdCommand::Rotate {
            master,
            authority_dir,
            master_passphrase_file,
            passphrase_file,
            expect_identity: _,
        } => {
            let master_pass = passphrase(master_passphrase_file, "senha da mestra fria", false)?;
            let authority_pass = passphrase(passphrase_file, "senha da autoridade nova", true)?;
            let outcome = rotate(
                &master,
                &authority_dir,
                &master_pass,
                &authority_pass,
                KdfParams::DEFAULT,
                now,
                None,
            )?;
            (outcome, master, authority_dir)
        }
    };
    println!("identidade: {}", outcome.identity.to_base58());
    println!("mestra fria: {}", master_path.display());
    println!(
        "autoridade: {} e {}",
        authority_dir.join(AUTHORITY_KEY_FILE).display(),
        authority_dir.join(AUTHORITY_CERT_FILE).display()
    );
    println!(
        "guarde a mestra fria FORA do disco do primário (mídia offline). Senha da mestra perdida = identidade perdida: não há recuperação."
    );
    println!(
        "a CLI normal ainda não usa esses arquivos (o pareamento autenticado vem na fatia 2 do P30)."
    );
    Ok(())
}

fn passphrase(
    file: Option<PathBuf>,
    label: &str,
    confirm: bool,
) -> Result<Zeroizing<Vec<u8>>, String> {
    if let Some(path) = file {
        return crate::keys::read_passphrase_file(&path);
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Err(format!("sem terminal: informe a {label} por arquivo"));
    }
    let first = Zeroizing::new(
        rpassword::prompt_password(format!("{label}: ")).map_err(|e| e.to_string())?,
    );
    if first.is_empty() {
        return Err("a senha não pode ser vazia".into());
    }
    if confirm {
        let second = Zeroizing::new(
            rpassword::prompt_password(format!("repita a {label}: ")).map_err(|e| e.to_string())?,
        );
        if *first != *second {
            return Err("as senhas não conferem".into());
        }
    }
    Ok(kin_identity::normalize_passphrase(&first))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_identity::{AuthorityCertificate, AuthorityKey, StandaloneIdentity, is_sealed};

    const MASTER_PASS: &[u8] = b"senha da mestra fria";
    const AUTH_PASS: &[u8] = b"senha da autoridade";
    const NOW: u64 = 1_700_000_000;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn do_init(dir: &Path) -> Outcome {
        init(
            &dir.join("offline/cold-master.key"),
            &dir.join("primary"),
            MASTER_PASS,
            AUTH_PASS,
            KdfParams::MIN,
            NOW,
        )
        .unwrap()
    }

    fn read_cert(dir: &Path) -> AuthorityCertificate {
        AuthorityCertificate::from_bytes(
            &std::fs::read(dir.join("primary/authority.cert")).unwrap(),
        )
        .unwrap()
    }

    // ---- argumentos ----

    #[test]
    fn parses_init_and_rotate() {
        assert_eq!(
            parse(args(&[
                "init",
                "--master-out",
                "m.key",
                "--authority-dir",
                "d"
            ]))
            .unwrap(),
            ColdCommand::Init {
                master_out: "m.key".into(),
                authority_dir: "d".into(),
                master_passphrase_file: None,
                passphrase_file: None,
            }
        );
        assert_eq!(
            parse(args(&[
                "rotate-authority",
                "--master",
                "m.key",
                "--authority-dir",
                "d",
                "--master-passphrase-file",
                "mp",
                "--passphrase-file",
                "ap",
            ]))
            .unwrap(),
            ColdCommand::Rotate {
                master: "m.key".into(),
                authority_dir: "d".into(),
                master_passphrase_file: Some("mp".into()),
                passphrase_file: Some("ap".into()),
                expect_identity: None,
            }
        );
    }

    #[test]
    fn parses_expect_identity_only_for_rotate() {
        let rotate_args = [
            "rotate-authority",
            "--master",
            "m.key",
            "--authority-dir",
            "d",
            "--expect-identity",
            "12D3KooWabc",
        ];
        match parse(args(&rotate_args)).unwrap() {
            ColdCommand::Rotate {
                expect_identity, ..
            } => assert_eq!(expect_identity.as_deref(), Some("12D3KooWabc")),
            other => panic!("esperava Rotate, veio {other:?}"),
        }
        assert!(
            parse(args(&[
                "init",
                "--master-out",
                "m.key",
                "--authority-dir",
                "d",
                "--expect-identity",
                "x"
            ]))
            .is_err()
        );
    }

    #[test]
    fn an_argument_error_never_echoes_the_value_after_an_equals_sign() {
        let err = parse(args(&[
            "init",
            "--master-out",
            "m",
            "--authority-dir",
            "d",
            "--passphrase=segredo123",
        ]))
        .unwrap_err();
        assert!(!err.contains("segredo123"), "ecoou o segredo: {err}");
    }

    #[test]
    fn refuses_missing_unknown_and_misplaced_arguments() {
        for bad in [
            args(&[]),
            args(&["wipe"]),
            args(&["init", "--authority-dir", "d"]),
            args(&["init", "--master-out", "m"]),
            args(&["init", "--master-out", "m", "--authority-dir", "d", "--x"]),
            args(&["init", "--master", "m", "--authority-dir", "d"]),
            args(&[
                "rotate-authority",
                "--master-out",
                "m",
                "--authority-dir",
                "d",
            ]),
            args(&["rotate-authority", "--authority-dir", "d"]),
            args(&["init", "--master-out"]),
        ] {
            assert!(parse(bad.clone()).is_err(), "aceitou {bad:?}");
        }
    }

    // ---- init ----

    #[test]
    fn init_writes_a_sealed_master_and_a_sealed_authority_with_a_valid_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = do_init(dir.path());
        let master_file = dir.path().join("offline/cold-master.key");
        assert!(is_sealed(&std::fs::read(&master_file).unwrap()));
        assert!(is_sealed(
            &std::fs::read(dir.path().join("primary/authority.key")).unwrap()
        ));

        let master = StandaloneIdentity::load_with(&master_file, Some(MASTER_PASS)).unwrap();
        let cert = read_cert(dir.path());
        cert.verify().unwrap();
        assert_eq!(cert.identity, master_identity(&master));
        assert_eq!(cert.identity, outcome.identity);
        let authority =
            AuthorityKey::load_with(&dir.path().join("primary/authority.key"), Some(AUTH_PASS))
                .unwrap();
        assert_eq!(cert.authority_key, authority.public_key());
        assert_ne!(
            cert.authority_key,
            *outcome.identity.public_key(),
            "autoridade igual à mestra"
        );
    }

    fn master_identity(master: &StandaloneIdentity) -> IdentityId {
        use kin_identity::IdentityProvider;
        master.id()
    }

    #[test]
    fn the_primary_directory_never_gets_the_master() {
        let dir = tempfile::tempdir().unwrap();
        do_init(dir.path());
        let mut names: Vec<_> = std::fs::read_dir(dir.path().join("primary"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["authority.cert", "authority.key"]);
    }

    #[test]
    fn init_refuses_to_overwrite_and_leaves_existing_files_untouched() {
        let dir = tempfile::tempdir().unwrap();
        do_init(dir.path());
        let master_file = dir.path().join("offline/cold-master.key");
        let before_master = std::fs::read(&master_file).unwrap();
        let before_key = std::fs::read(dir.path().join("primary/authority.key")).unwrap();
        assert!(
            init(
                &master_file,
                &dir.path().join("other"),
                MASTER_PASS,
                AUTH_PASS,
                KdfParams::MIN,
                NOW
            )
            .is_err(),
            "sobrescreveu a mestra"
        );
        assert!(
            init(
                &dir.path().join("offline/second.key"),
                &dir.path().join("primary"),
                MASTER_PASS,
                AUTH_PASS,
                KdfParams::MIN,
                NOW
            )
            .is_err(),
            "sobrescreveu a autoridade"
        );
        assert_eq!(std::fs::read(&master_file).unwrap(), before_master);
        assert_eq!(
            std::fs::read(dir.path().join("primary/authority.key")).unwrap(),
            before_key
        );
        assert!(
            !dir.path().join("other").exists(),
            "criou diretório antes de recusar"
        );
    }

    #[test]
    fn init_refuses_the_same_passphrase_for_master_and_authority() {
        let dir = tempfile::tempdir().unwrap();
        let result = init(
            &dir.path().join("m.key"),
            &dir.path().join("primary"),
            MASTER_PASS,
            MASTER_PASS,
            KdfParams::MIN,
            NOW,
        );
        assert!(result.is_err());
        assert!(!dir.path().join("m.key").exists());
        assert!(!dir.path().join("primary").exists());
    }

    #[test]
    fn init_refuses_a_master_file_inside_the_authority_directory() {
        let dir = tempfile::tempdir().unwrap();
        let result = init(
            &dir.path().join("primary/cold-master.key"),
            &dir.path().join("primary"),
            MASTER_PASS,
            AUTH_PASS,
            KdfParams::MIN,
            NOW,
        );
        assert!(result.is_err());
        assert!(!dir.path().join("primary").exists());
    }

    // ---- rotate ----

    #[test]
    fn rotate_certifies_a_new_authority_for_the_same_identity() {
        let dir = tempfile::tempdir().unwrap();
        let first = do_init(dir.path());
        let old_cert = read_cert(dir.path());
        let outcome = rotate(
            &dir.path().join("offline/cold-master.key"),
            &dir.path().join("primary2"),
            MASTER_PASS,
            b"outra senha da autoridade",
            KdfParams::MIN,
            NOW + 1,
            None,
        )
        .unwrap();
        assert_eq!(outcome.identity, first.identity);
        let new_cert = AuthorityCertificate::from_bytes(
            &std::fs::read(dir.path().join("primary2/authority.cert")).unwrap(),
        )
        .unwrap();
        new_cert.verify().unwrap();
        assert_eq!(new_cert.identity, old_cert.identity);
        assert_ne!(new_cert.authority_key, old_cert.authority_key);
        let key = AuthorityKey::load_with(
            &dir.path().join("primary2/authority.key"),
            Some(b"outra senha da autoridade"),
        )
        .unwrap();
        assert_eq!(key.public_key(), new_cert.authority_key);
    }

    #[test]
    fn rotate_with_the_wrong_master_passphrase_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        do_init(dir.path());
        let result = rotate(
            &dir.path().join("offline/cold-master.key"),
            &dir.path().join("primary2"),
            b"errada",
            AUTH_PASS,
            KdfParams::MIN,
            NOW,
            None,
        );
        assert!(result.is_err());
        assert!(!dir.path().join("primary2").exists());
    }

    #[test]
    fn rotate_refuses_an_authority_directory_that_already_has_an_authority() {
        let dir = tempfile::tempdir().unwrap();
        do_init(dir.path());
        let before = std::fs::read(dir.path().join("primary/authority.key")).unwrap();
        let result = rotate(
            &dir.path().join("offline/cold-master.key"),
            &dir.path().join("primary"),
            MASTER_PASS,
            b"outra senha da autoridade",
            KdfParams::MIN,
            NOW,
            None,
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(dir.path().join("primary/authority.key")).unwrap(),
            before
        );
    }

    #[test]
    fn rotate_refuses_the_same_passphrase_for_master_and_authority() {
        let dir = tempfile::tempdir().unwrap();
        do_init(dir.path());
        let result = rotate(
            &dir.path().join("offline/cold-master.key"),
            &dir.path().join("primary2"),
            MASTER_PASS,
            MASTER_PASS,
            KdfParams::MIN,
            NOW,
            None,
        );
        assert!(result.is_err());
        assert!(!dir.path().join("primary2").exists());
    }

    // ---- correções da auditoria ----

    #[test]
    fn check_paths_resolves_dot_dot_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir(p.join("primary")).unwrap();
        // Direto.
        assert!(check_paths(&p.join("primary/m.key"), &p.join("primary")).is_err());
        // Com `..` no meio (o `starts_with` por componentes não via isto).
        assert!(check_paths(&p.join("x/../primary/m.key"), &p.join("primary")).is_err());
        assert!(check_paths(&p.join("primary/sub/../m.key"), &p.join("primary")).is_err());
        // Via symlink para o diretório do primário.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(p.join("primary"), p.join("link")).unwrap();
            assert!(check_paths(&p.join("link/m.key"), &p.join("primary")).is_err());
            assert!(check_paths(&p.join("primary/m.key"), &p.join("link")).is_err());
        }
        // Fora do diretório, mesmo com nome parecido: aceita.
        assert!(check_paths(&p.join("primary2/m.key"), &p.join("primary")).is_ok());
        assert!(check_paths(&p.join("offline/m.key"), &p.join("primary")).is_ok());
    }

    #[test]
    fn check_paths_refuses_an_existing_authority() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            check_paths(
                &dir.path().join("offline/m.key"),
                &dir.path().join("primary")
            )
            .is_ok()
        );
        do_init(dir.path());
        assert!(
            check_paths(
                &dir.path().join("offline/m2.key"),
                &dir.path().join("primary")
            )
            .is_err()
        );
    }

    #[test]
    fn init_refuses_empty_passphrases_before_writing_anything() {
        let dir = tempfile::tempdir().unwrap();
        for (master_pass, authority_pass) in [(MASTER_PASS, &b""[..]), (&b""[..], AUTH_PASS)] {
            let result = init(
                &dir.path().join("offline/m.key"),
                &dir.path().join("primary"),
                master_pass,
                authority_pass,
                KdfParams::MIN,
                NOW,
            );
            assert!(result.is_err());
            assert!(!dir.path().join("offline").exists());
            assert!(!dir.path().join("primary").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_failure_after_the_master_is_written_cleans_up_and_names_the_master() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("primary");
        std::fs::create_dir(&primary).unwrap();
        // Um symlink pendente no lugar do certificado: `exists()` diz que não existe, a gravação falha.
        std::os::unix::fs::symlink("/nonexistent-kin-dir/x", primary.join("authority.cert"))
            .unwrap();
        let master_file = dir.path().join("offline/m.key");
        let err = init(
            &master_file,
            &primary,
            MASTER_PASS,
            AUTH_PASS,
            KdfParams::MIN,
            NOW,
        )
        .unwrap_err();
        assert!(
            err.contains(&master_file.display().to_string()),
            "o erro não diz onde a mestra foi gravada: {err}"
        );
        assert!(master_file.exists(), "a mestra não deve ser apagada");
        assert!(
            !primary.join("authority.key").exists(),
            "sobrou authority.key sem certificado"
        );
    }

    #[test]
    fn rotate_refuses_a_master_of_another_identity() {
        let dir = tempfile::tempdir().unwrap();
        let first = do_init(dir.path());
        let master_file = dir.path().join("offline/cold-master.key");
        let other = StandaloneIdentity::generate();
        let wrong = {
            use kin_identity::IdentityProvider;
            other.id().to_base58()
        };
        let result = rotate(
            &master_file,
            &dir.path().join("primary2"),
            MASTER_PASS,
            b"outra senha da autoridade",
            KdfParams::MIN,
            NOW,
            Some(wrong.as_str()),
        );
        assert!(result.is_err());
        assert!(!dir.path().join("primary2").exists());
        let right = first.identity.to_base58();
        rotate(
            &master_file,
            &dir.path().join("primary2"),
            MASTER_PASS,
            b"outra senha da autoridade",
            KdfParams::MIN,
            NOW,
            Some(right.as_str()),
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn created_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        do_init(dir.path());
        for sub in ["offline", "primary"] {
            let mode = std::fs::metadata(dir.path().join(sub))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "{sub}");
        }
    }
}
