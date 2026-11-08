//! Quais relays este dispositivo usa: `--nostr-relay` (flag) > `relays.txt` no diretório de dados >
//! padrão embutido. Sem rede; só lê e valida as URLs (mesma regra do `NostrMailbox`).

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;

use nostr::prelude::RelayUrlScheme;

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

/// Quantos relays de um contato se aceitam. Cada depósito abre uma conexão por relay, e quem escolhe
/// o host é o contato: o teto limita a amplificação.
pub const MAX_PEER_RELAYS: usize = 3;

/// Tamanho máximo de uma URL anunciada por um contato (bytes).
const MAX_PEER_URL: usize = 200;

/// Valida a lista de relays que um contato anunciou. É entrada hostil: o MLS prova quem mandou, não
/// que os hosts são seguros, e é o NOSSO cliente que vai conectar neles. Regras: de 1 a
/// [`MAX_PEER_RELAYS`] itens; só `wss://`; host é nome de domínio (sem IP literal nem `localhost`);
/// sem credenciais na URL; no máximo 200 bytes; repetidos contam uma vez. Qualquer item ruim recusa a
/// lista inteira (quem chama mantém a anterior).
pub fn validate_peer_relays(relays: &[String]) -> Result<Vec<String>, NostrError> {
    let bad = |why: String| NostrError::InvalidRelayUrl(why);
    if relays.is_empty() {
        return Err(bad("lista vazia".into()));
    }
    let mut seen = HashSet::new();
    let mut accepted = Vec::new();
    for url in relays {
        if url.len() > MAX_PEER_URL {
            return Err(bad("URL grande demais".into()));
        }
        let parsed = parse_relay(url)?;
        if parsed.scheme() != RelayUrlScheme::Wss {
            return Err(bad(format!("{url}: só wss:// vale")));
        }
        if parsed.domain().is_none() || parsed.is_localhost() {
            return Err(bad(format!("{url}: o host precisa ser um nome de domínio")));
        }
        let authority = url.split_once("://").map_or("", |(_, rest)| rest);
        let authority = authority.split(['/', '?', '#']).next().unwrap_or("");
        if authority.contains('@') {
            return Err(bad(format!("{url}: credenciais na URL")));
        }
        let key = parsed
            .to_string()
            .trim_end_matches('/')
            .to_ascii_lowercase();
        if seen.insert(key) {
            accepted.push(url.clone());
        }
    }
    if accepted.len() > MAX_PEER_RELAYS {
        return Err(bad(format!("mais de {MAX_PEER_RELAYS} relays")));
    }
    Ok(accepted)
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
