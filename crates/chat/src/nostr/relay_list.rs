//! Quais relays este dispositivo usa: `--nostr-relay` (flag) > `relays.txt` no diretório de dados >
//! padrão embutido. Sem rede; só lê e valida as URLs (mesma regra do `NostrMailbox`).

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;

use super::mailbox::{NostrError, parse_relay};

/// Relays usados quando o usuário não configurou nada. Medidos na Sessão 11: depositam e devolvem em
/// ~1 s, sem NIP-42. Sempre substituíveis (flag ou `relays.txt`).
pub const DEFAULT_RELAYS: [&str; 2] = ["wss://relay.primal.net", "wss://offchain.pub"];

/// De onde veio a lista de relays em uso.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelaySource {
    Flag,
    File,
    Default,
}

/// A lista de relays escolhida e de onde ela veio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayChoice {
    pub relays: Vec<String>,
    pub source: RelaySource,
}

/// Lê uma lista de relays: uma URL por linha; linhas vazias e as que começam com `#` são ignoradas.
/// URL inválida é erro (com o número da linha), nunca é pulada em silêncio. Repetidos contam uma vez.
pub fn parse_relay_list(text: &str) -> Result<Vec<String>, NostrError> {
    let mut seen = HashSet::new();
    let mut relays = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parsed = parse_relay(line).map_err(|e| with_line(e, index + 1))?;
        // O `RelayUrl` não normaliza a barra final nem a caixa: comparamos sem elas.
        let key = parsed
            .to_string()
            .trim_end_matches('/')
            .to_ascii_lowercase();
        if seen.insert(key) {
            relays.push(line.to_string());
        }
    }
    Ok(relays)
}

fn with_line(error: NostrError, line: usize) -> NostrError {
    match error {
        NostrError::InvalidRelayUrl(why) => {
            NostrError::InvalidRelayUrl(format!("linha {line}: {why}"))
        }
        other => other,
    }
}

/// Escolhe os relays: `flags` (se houver) vencem o arquivo `file`, que vence o padrão. Com flags, o
/// arquivo nem é lido. Arquivo ausente cai no padrão; arquivo sem nenhum relay é erro (quem o esvaziou
/// provavelmente não quer o padrão).
pub fn resolve_relays(flags: &[String], file: &Path) -> Result<RelayChoice, NostrError> {
    if !flags.is_empty() {
        return Ok(RelayChoice {
            relays: parse_relay_list(&flags.join("\n"))?,
            source: RelaySource::Flag,
        });
    }
    match std::fs::read_to_string(file) {
        Ok(text) => {
            let relays = parse_relay_list(&text).map_err(|e| prefix(e, file))?;
            if relays.is_empty() {
                return Err(NostrError::InvalidRelayUrl(format!(
                    "{}: nenhum relay (apague o arquivo para usar o padrão)",
                    file.display()
                )));
            }
            Ok(RelayChoice {
                relays,
                source: RelaySource::File,
            })
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(RelayChoice {
            relays: DEFAULT_RELAYS.iter().map(|s| (*s).to_string()).collect(),
            source: RelaySource::Default,
        }),
        Err(e) => Err(NostrError::RelayFile(format!("{}: {e}", file.display()))),
    }
}

fn prefix(error: NostrError, file: &Path) -> NostrError {
    match error {
        NostrError::InvalidRelayUrl(why) => {
            NostrError::InvalidRelayUrl(format!("{}: {why}", file.display()))
        }
        other => other,
    }
}
