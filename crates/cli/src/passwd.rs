//! `kin passwd` (P13): troca a senha dos arquivos de chave de um `--data-dir` (sem rede nem chat).
//! A troca em si vive em `keys::change_passphrase`; aqui só a linha de comando.

use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub struct PasswdCommand {
    data_dir: PathBuf,
    /// Senha atual: arquivo ou, sem ele, prompt.
    old_file: Option<PathBuf>,
    /// Senha nova: arquivo ou, sem ele, prompt com confirmação.
    new_file: Option<PathBuf>,
}

/// `kin passwd [--data-dir DIR] [--passphrase-file ARQ] [--new-passphrase-file ARQ]`. Não lê
/// `KIN_PASSPHRASE_FILE`: com duas senhas em jogo, a variável deixaria ambíguo qual das duas ela é.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<PasswdCommand, String> {
    let mut command = PasswdCommand {
        data_dir: PathBuf::from(".kin"),
        old_file: None,
        new_file: None,
    };
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} precisa de um valor"));
        match flag.as_str() {
            "--data-dir" => command.data_dir = value()?.into(),
            "--passphrase-file" => command.old_file = Some(value()?.into()),
            "--new-passphrase-file" => command.new_file = Some(value()?.into()),
            other if other.starts_with("--") => {
                // Só o nome: `--new-passphrase=segredo` não pode ser ecoado no terminal.
                let name = other.split('=').next().unwrap_or(other);
                return Err(format!("argumento desconhecido: {name}"));
            }
            // Sem `--`: pode ser a senha digitada no lugar errado; não ecoa nada.
            _ => return Err("argumento inesperado (as senhas vão por arquivo ou prompt)".into()),
        }
    }
    Ok(command)
}

pub fn run(command: PasswdCommand) -> Result<(), String> {
    let old = crate::cold::passphrase(command.old_file, "senha atual", false)?;
    let new = crate::cold::passphrase(command.new_file, "senha nova", true)?;
    eprintln!(
        "trocando a senha (Argon2id em cada arquivo; pode levar alguns segundos). O kin que usa este diretório TEM de estar fechado (não há trava de diretório). Se interromper, repita o mesmo comando, com a mesma senha nova, para terminar a troca."
    );
    let changed = crate::keys::change_passphrase(
        &command.data_dir,
        &old,
        &new,
        kin_identity::KdfParams::DEFAULT,
    )
    .map_err(|e| e.to_string())?;
    if changed.is_empty() {
        println!("nada a trocar: todos os arquivos já estão na senha nova");
    } else {
        println!("senha trocada em: {}", changed.join(", "));
        eprintln!(
            "aviso: a troca re-sela as MESMAS chaves (a identidade, o device e o db.key não mudaram), e não mexe na mestra fria (que tem senha própria). Os arquivos com a senha antiga podem continuar recuperáveis no disco (SSD, snapshot, backup); se a senha antiga vazou, trate a chave como exposta: quem tem uma cópia antiga e a senha antiga abre a mesma identidade"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<PasswdCommand, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn the_defaults_are_the_default_dir_and_prompts() {
        let command = parse_strs(&[]).unwrap();
        assert_eq!(command.data_dir, PathBuf::from(".kin"));
        assert_eq!(command.old_file, None);
        assert_eq!(command.new_file, None);
    }

    #[test]
    fn the_two_passphrase_files_are_kept_apart() {
        let command = parse_strs(&[
            "--data-dir",
            "/d",
            "--passphrase-file",
            "/velha",
            "--new-passphrase-file",
            "/nova",
        ])
        .unwrap();
        assert_eq!(command.data_dir, PathBuf::from("/d"));
        assert_eq!(command.old_file, Some(PathBuf::from("/velha")));
        assert_eq!(command.new_file, Some(PathBuf::from("/nova")));
    }

    #[test]
    fn unknown_flags_and_missing_values_are_refused() {
        assert!(parse_strs(&["--protect"]).is_err());
        assert!(parse_strs(&["--no-passphrase"]).is_err());
        assert!(parse_strs(&["--new-passphrase-file"]).is_err());
    }

    /// A senha digitada no lugar errado (flag com `=` ou argumento solto) nunca volta no erro.
    #[test]
    fn a_parse_error_never_echoes_a_passphrase() {
        for bad in ["--new-passphrase=hunter2", "hunter2"] {
            let err = parse_strs(&[bad]).unwrap_err();
            assert!(!err.contains("hunter2"), "ecoou a senha: {err}");
        }
    }
}
