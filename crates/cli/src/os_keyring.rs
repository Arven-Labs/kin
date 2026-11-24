//! Keyring do SO como fonte da senha das chaves (P13, ADR 0047): `kin --keyring` lê a senha, e
//! `kin keyring save|forget` a grava e a apaga. O keyring guarda só a **senha** (como segredo binário,
//! byte a byte); os arquivos selados e o formato do ADR 0012 não mudam. A entrada é `("kin", caminho
//! canônico do --data-dir)`: não há identidade legível antes de abrir as chaves, e mover o diretório
//! exige um novo `save`.
//!
//! Quem lê o keyring abre as chaves: qualquer programa da mesma sessão, com o keyring desbloqueado, lê a
//! senha. A cifra em repouso continua valendo contra disco ou backup roubado, não contra isso.

use std::path::{Path, PathBuf};

use keyring_core::Error as KeyringError;
use zeroize::Zeroizing;

const SERVICE: &str = "kin";

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

/// `kin keyring save [--data-dir DIR] [--passphrase-file ARQ]` e `kin keyring forget [--data-dir DIR]`.
/// Os erros citam só o nome do argumento: a senha digitada no lugar errado nunca volta no terminal.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<KeyringCommand, String> {
    let mut it = args.into_iter();
    let action = match it.next().as_deref() {
        Some("save") => Action::Save,
        Some("forget") => Action::Forget,
        _ => return Err("subcomando esperado: save ou forget".to_string()),
    };
    let mut command = KeyringCommand {
        action,
        data_dir: PathBuf::from(".kin"),
        passphrase_file: None,
    };
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} precisa de um valor"));
        match flag.as_str() {
            "--data-dir" => command.data_dir = value()?.into(),
            // Esquecer não lê senha nenhuma: aqui a flag cai em "desconhecido".
            "--passphrase-file" if command.action == Action::Save => {
                command.passphrase_file = Some(value()?.into());
            }
            other if other.starts_with("--") => {
                let name = other.split('=').next().unwrap_or(other);
                return Err(format!("argumento desconhecido: {name}"));
            }
            _ => return Err("argumento inesperado (a senha vai por arquivo ou prompt)".to_string()),
        }
    }
    Ok(command)
}

pub fn run(command: KeyringCommand) -> Result<(), String> {
    match command.action {
        Action::Save => {
            let pass = crate::cold::passphrase(command.passphrase_file, "senha das chaves", false)?;
            eprintln!(
                "conferindo a senha contra as chaves (Argon2id em cada arquivo; pode levar alguns segundos)..."
            );
            save_verified(&command.data_dir, &pass)?;
            println!(
                "senha gravada no keyring do SO para {}",
                command.data_dir.display()
            );
            eprintln!(
                "aviso: quem lê o keyring abre as chaves (qualquer programa desta sessão com o keyring desbloqueado lê a senha); a cifra em repouso segue valendo contra disco ou backup roubado. Depois de `kin passwd` ou de mover o --data-dir, rode `kin keyring save` de novo"
            );
        }
        Action::Forget => {
            if forget(&command.data_dir)? {
                println!("senha removida do keyring do SO");
            } else {
                println!("não havia senha gravada para este --data-dir");
            }
        }
    }
    Ok(())
}

/// Inicia o store padrão do SO uma vez (o `keyring` v1 escolhe Secret Service, Keychain ou Credential
/// Manager). Se já há um store padrão (os testes ligam o `mock`), não mexe nele.
fn init_store() -> Result<(), String> {
    if keyring_core::get_default_store().is_some() {
        return Ok(());
    }
    match keyring::Entry::store_status() {
        Ok(()) => Ok(()),
        Err(e) => Err(format!(
            "keyring do SO indisponível ({e}): sem Secret Service, Keychain ou Credential Manager nesta sessão; use --passphrase-file ou o prompt"
        )),
    }
}

fn entry(dir: &Path) -> Result<keyring_core::Entry, String> {
    init_store()?;
    let path = std::fs::canonicalize(dir)
        .or_else(|_| std::path::absolute(dir))
        .map_err(|e| format!("caminho do --data-dir inválido: {e}"))?;
    keyring_core::Entry::new(SERVICE, &path.to_string_lossy()).map_err(|e| format!("keyring: {e}"))
}

/// A senha gravada para `dir`, ou o erro que diz como gravá-la.
pub fn load(dir: &Path) -> Result<Zeroizing<Vec<u8>>, String> {
    match entry(dir)?.get_secret() {
        Ok(secret) => Ok(Zeroizing::new(secret)),
        Err(KeyringError::NoEntry) => Err(
            "não há senha gravada no keyring para este --data-dir: rode `kin keyring save --data-dir DIR` (uma vez, com a senha)"
                .to_string(),
        ),
        Err(e) => Err(format!("keyring: {e}")),
    }
}

/// Grava `pass` como a senha de `dir`, sem conferir nada.
pub fn save(dir: &Path, pass: &[u8]) -> Result<(), String> {
    entry(dir)?
        .set_secret(pass)
        .map_err(|e| format!("keyring: {e}"))
}

/// Confere que `pass` abre as chaves de `dir` e só então a grava: uma senha errada no keyring só
/// apareceria na partida seguinte.
pub fn save_verified(dir: &Path, pass: &[u8]) -> Result<(), String> {
    crate::keys::check_passphrase(dir, pass).map_err(|e| e.to_string())?;
    save(dir, pass)
}

/// Apaga a senha gravada de `dir`; `false` se não havia.
pub fn forget(dir: &Path) -> Result<bool, String> {
    match entry(dir)?.delete_credential() {
        Ok(()) => Ok(true),
        Err(KeyringError::NoEntry) => Ok(false),
        Err(e) => Err(format!("keyring: {e}")),
    }
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
