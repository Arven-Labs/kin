//! `kin cold ...`: comandos **offline** da custódia da mestra (P30 fatia 1b, ADR 0011 opção D).
//!
//! A mestra fria só assina; quem trabalha no dia a dia é a chave de autoridade (ADR 0013). Nada aqui
//! abre rede nem `Chat`: só gera chaves e grava arquivos cifrados (ADR 0012).

// STUB (etapa 3 do plano): só as assinaturas, para os testes compilarem e falharem.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use kin_identity::{IdentityId, KdfParams};

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
    },
}

/// Lê os argumentos **depois** da palavra `cold`.
pub fn parse(_argv: impl IntoIterator<Item = String>) -> Result<ColdCommand, String> {
    todo!()
}

/// O que o usuário precisa ver depois de rodar um comando.
#[derive(Debug)]
pub struct Outcome {
    pub identity: IdentityId,
}

/// Gera a mestra e uma autoridade nova; grava `master_out` (mestra, cifrada com `master_pass`) e,
/// em `authority_dir`, `authority.key` (cifrada com `authority_pass`) e `authority.cert`.
pub fn init(
    _master_out: &Path,
    _authority_dir: &Path,
    _master_pass: &[u8],
    _authority_pass: &[u8],
    _params: KdfParams,
    _now: u64,
) -> Result<Outcome, String> {
    todo!()
}

/// A mestra fria existente certifica uma autoridade **nova** (mesmo `IdentityId`) em `authority_dir`.
pub fn rotate(
    _master_file: &Path,
    _authority_dir: &Path,
    _master_pass: &[u8],
    _authority_pass: &[u8],
    _params: KdfParams,
    _now: u64,
) -> Result<Outcome, String> {
    todo!()
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
            }
        );
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
        );
        assert!(result.is_err());
        assert!(!dir.path().join("primary2").exists());
    }
}
