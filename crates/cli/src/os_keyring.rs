//! Keyring do SO como fonte da senha das chaves (P13, ADR 0047): `kin --keyring` lê a senha, e
//! `kin keyring save|forget` a grava e a apaga. Esqueleto do commit de testes: nada disto está
//! implementado ainda, e o `allow` abaixo sai junto com o esqueleto.
#![allow(dead_code, unused_variables)]

use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Save,
    Forget,
}

#[derive(Debug, PartialEq, Eq)]
pub struct KeyringCommand {
    pub action: Action,
    pub data_dir: PathBuf,
    pub passphrase_file: Option<PathBuf>,
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<KeyringCommand, String> {
    Err("não implementado".into())
}

pub fn run(command: KeyringCommand) -> Result<(), String> {
    Err("não implementado".into())
}

/// A senha gravada para `dir`, ou o erro que diz como gravá-la.
pub fn load(dir: &Path) -> Result<Zeroizing<Vec<u8>>, String> {
    Err("não implementado".into())
}

/// Grava `pass` como a senha de `dir`, sem conferir nada.
pub fn save(dir: &Path, pass: &[u8]) -> Result<(), String> {
    Err("não implementado".into())
}

/// Confere que `pass` abre as chaves de `dir` e só então a grava.
pub fn save_verified(dir: &Path, pass: &[u8]) -> Result<(), String> {
    Err("não implementado".into())
}

/// Apaga a senha gravada de `dir`; `false` se não havia.
pub fn forget(dir: &Path) -> Result<bool, String> {
    Err("não implementado".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_identity::KdfParams;

    const PASS: &[u8] = b"senha de teste";

    /// O store padrão do `keyring-core` é global ao processo: o mock entra uma vez só (trocá-lo no meio
    /// de outro teste apagaria as entradas dele), e cada teste usa o próprio diretório temporário, que
    /// é o `user` da entrada. Nenhum teste toca o Secret Service de verdade.
    fn use_mock_store() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
        });
    }

    fn sealed_dir() -> tempfile::TempDir {
        use_mock_store();
        let dir = tempfile::tempdir().unwrap();
        crate::keys::load(dir.path(), Some(PASS), false, KdfParams::MIN).unwrap();
        dir
    }

    fn parse_strs(args: &[&str]) -> Result<KeyringCommand, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    // ---- load / save / forget ----

    #[test]
    fn save_then_load_returns_the_same_bytes() {
        use_mock_store();
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), PASS).unwrap();
        assert_eq!(&*load(dir.path()).unwrap(), PASS);
    }

    #[test]
    fn load_without_a_saved_entry_points_to_keyring_save() {
        use_mock_store();
        let dir = tempfile::tempdir().unwrap();
        let err = load(dir.path()).unwrap_err();
        assert!(err.contains("kin keyring save"), "{err}");
    }

    #[test]
    fn forget_removes_the_entry_and_is_idempotent() {
        use_mock_store();
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), PASS).unwrap();
        assert_eq!(forget(dir.path()), Ok(true));
        assert!(load(dir.path()).is_err());
        assert_eq!(forget(dir.path()), Ok(false));
    }

    #[test]
    fn each_data_dir_has_its_own_entry() {
        use_mock_store();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        save(a.path(), b"senha do a").unwrap();
        save(b.path(), b"senha do b").unwrap();
        assert_eq!(&*load(a.path()).unwrap(), b"senha do a");
        assert_eq!(&*load(b.path()).unwrap(), b"senha do b");
        forget(a.path()).unwrap();
        assert_eq!(&*load(b.path()).unwrap(), b"senha do b");
    }

    /// `./dir` e `dir` são o mesmo diretório: a entrada é pelo caminho canônico.
    #[test]
    fn two_spellings_of_the_same_dir_share_the_entry() {
        use_mock_store();
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), PASS).unwrap();
        assert_eq!(&*load(&dir.path().join(".")).unwrap(), PASS);
    }

    /// Senha com acento e bytes que não são texto simples voltam idênticos (vai como segredo binário).
    #[test]
    fn the_secret_round_trips_byte_for_byte() {
        use_mock_store();
        let dir = tempfile::tempdir().unwrap();
        let pass = "senha com açaí e ✓".as_bytes();
        save(dir.path(), pass).unwrap();
        assert_eq!(&*load(dir.path()).unwrap(), pass);
    }

    // ---- save_verified ----

    #[test]
    fn save_verified_stores_only_a_passphrase_that_opens_the_keys() {
        let dir = sealed_dir();
        assert!(save_verified(dir.path(), b"errada").is_err());
        assert!(
            load(dir.path()).is_err(),
            "uma senha errada não pode ser gravada"
        );
        save_verified(dir.path(), PASS).unwrap();
        assert_eq!(&*load(dir.path()).unwrap(), PASS);
    }

    #[test]
    fn save_verified_refuses_plain_keys_and_points_to_protect() {
        use_mock_store();
        let dir = tempfile::tempdir().unwrap();
        crate::keys::load(dir.path(), None, false, KdfParams::MIN).unwrap();
        let err = save_verified(dir.path(), PASS).unwrap_err();
        assert!(err.contains("--protect"), "{err}");
        assert!(load(dir.path()).is_err());
    }

    /// De ponta a ponta: a senha que voltou do keyring abre as chaves do diretório.
    #[test]
    fn the_passphrase_from_the_keyring_opens_the_keys() {
        let dir = sealed_dir();
        save_verified(dir.path(), PASS).unwrap();
        let pass = load(dir.path()).unwrap();
        crate::keys::load(dir.path(), Some(pass.as_slice()), false, KdfParams::MIN).unwrap();
    }

    /// Depois do `kin passwd` a entrada fica velha: `--keyring` falha sem alterar nenhum arquivo.
    #[test]
    fn a_stale_entry_after_passwd_fails_without_changing_any_file() {
        let dir = sealed_dir();
        save_verified(dir.path(), PASS).unwrap();
        crate::keys::change_passphrase(dir.path(), PASS, b"senha nova", KdfParams::MIN).unwrap();
        let before = std::fs::read(dir.path().join("identity.key")).unwrap();
        let pass = load(dir.path()).unwrap();
        assert!(
            crate::keys::load(dir.path(), Some(pass.as_slice()), false, KdfParams::MIN).is_err()
        );
        assert_eq!(
            std::fs::read(dir.path().join("identity.key")).unwrap(),
            before
        );
        // Regravar com a senha nova conserta.
        save_verified(dir.path(), b"senha nova").unwrap();
        let pass = load(dir.path()).unwrap();
        crate::keys::load(dir.path(), Some(pass.as_slice()), false, KdfParams::MIN).unwrap();
    }

    // ---- linha de comando ----

    #[test]
    fn save_takes_the_data_dir_and_a_passphrase_file() {
        let command = parse_strs(&["save", "--data-dir", "/d", "--passphrase-file", "/p"]).unwrap();
        assert_eq!(
            command,
            KeyringCommand {
                action: Action::Save,
                data_dir: PathBuf::from("/d"),
                passphrase_file: Some(PathBuf::from("/p")),
            }
        );
    }

    #[test]
    fn forget_takes_only_the_data_dir_and_defaults_to_the_default_one() {
        let command = parse_strs(&["forget"]).unwrap();
        assert_eq!(command.action, Action::Forget);
        assert_eq!(command.data_dir, PathBuf::from(".kin"));
        assert_eq!(command.passphrase_file, None);
        // Não há senha a ler para esquecer.
        assert!(parse_strs(&["forget", "--passphrase-file", "/p"]).is_err());
    }

    #[test]
    fn a_missing_or_unknown_subcommand_is_refused() {
        assert!(
            parse_strs(&["save"]).is_ok(),
            "controle: o subcomando válido passa"
        );
        assert!(parse_strs(&[]).is_err());
        assert!(parse_strs(&["list"]).is_err());
    }

    /// A senha digitada no lugar errado (subcomando, flag com `=` ou argumento solto) nunca volta no erro.
    #[test]
    fn a_parse_error_never_echoes_a_passphrase() {
        assert!(
            parse_strs(&["save"]).is_ok(),
            "controle: sem argumento ruim, passa"
        );
        for bad in [
            &["hunter2"][..],
            &["save", "hunter2"],
            &["save", "--passphrase=hunter2"],
            &["forget", "--data-dir=hunter2"],
        ] {
            let err = parse_strs(bad).unwrap_err();
            assert!(!err.contains("hunter2"), "{bad:?} ecoou a senha: {err}");
        }
    }
}
