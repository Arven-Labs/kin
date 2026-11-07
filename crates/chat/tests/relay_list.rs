//! Relays de entrada do próprio dispositivo: leitura do `relays.txt`, precedência flag > arquivo >
//! padrão e validação das URLs. Sem rede.

use kin_chat::nostr::{DEFAULT_RELAYS, NostrError, RelaySource, parse_relay_list, resolve_relays};

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

fn dir_with(file: Option<&str>) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relays.txt");
    if let Some(text) = file {
        std::fs::write(&path, text).unwrap();
    }
    (dir, path)
}

// 1
#[test]
fn comments_blank_lines_and_crlf_are_ignored_and_the_order_is_kept() {
    let text = "# meus relays\r\n\r\n  wss://a.example  \r\n# outro\nwss://b.example\n";

    let relays = parse_relay_list(text).unwrap();

    assert_eq!(relays, ["wss://a.example", "wss://b.example"]);
}

// 2
#[test]
fn a_remote_plain_ws_url_is_refused_with_the_line_number() {
    let err = parse_relay_list("wss://ok.example\nws://remoto.example\n").unwrap_err();

    assert!(matches!(err, NostrError::InvalidRelayUrl(_)));
    assert!(err.to_string().contains("linha 2"), "erro: {err}");
}

// 3
#[test]
fn a_plain_ws_url_on_this_computer_is_accepted() {
    let relays = parse_relay_list("ws://127.0.0.1:7777\n").unwrap();

    assert_eq!(relays, ["ws://127.0.0.1:7777"]);
}

// 4
#[test]
fn a_line_that_is_not_a_url_is_an_error_and_not_silently_skipped() {
    let err = parse_relay_list("wss://ok.example\nisso nao e url\n").unwrap_err();

    assert!(err.to_string().contains("linha 2"), "erro: {err}");
}

// 5
#[test]
fn the_same_relay_written_twice_counts_once() {
    let relays = parse_relay_list("wss://a.example\nwss://a.example/\nwss://b.example\n").unwrap();

    assert_eq!(relays.len(), 2);
}

// 6
#[test]
fn flags_win_over_the_file() {
    let (_dir, path) = dir_with(Some("wss://do-arquivo.example\n"));

    let choice = resolve_relays(&strings(&["wss://da-flag.example"]), &path).unwrap();

    assert_eq!(choice.source, RelaySource::Flag);
    assert_eq!(choice.relays, ["wss://da-flag.example"]);
}

// 7
#[test]
fn without_flags_the_file_is_used() {
    let (_dir, path) = dir_with(Some("# so um\nwss://do-arquivo.example\n"));

    let choice = resolve_relays(&[], &path).unwrap();

    assert_eq!(choice.source, RelaySource::File);
    assert_eq!(choice.relays, ["wss://do-arquivo.example"]);
}

// 8
#[test]
fn without_flags_or_file_the_built_in_default_is_used() {
    let (_dir, path) = dir_with(None);

    let choice = resolve_relays(&[], &path).unwrap();

    assert_eq!(choice.source, RelaySource::Default);
    assert_eq!(choice.relays, DEFAULT_RELAYS);
}

// 9
#[test]
fn a_file_with_no_relay_in_it_is_an_error_and_does_not_fall_back_to_the_default() {
    let (_dir, path) = dir_with(Some("# nada aqui\n\n"));

    let err = resolve_relays(&[], &path).unwrap_err();

    assert!(matches!(err, NostrError::InvalidRelayUrl(_)));
}

// 10
#[test]
fn a_bad_flag_is_an_error() {
    let (_dir, path) = dir_with(None);

    let err = resolve_relays(&strings(&["ws://remoto.example"]), &path).unwrap_err();

    assert!(matches!(err, NostrError::InvalidRelayUrl(_)));
}

// 11
#[test]
fn a_broken_file_does_not_matter_when_flags_are_given() {
    let (_dir, path) = dir_with(Some("isso nao e url\n"));

    let choice = resolve_relays(&strings(&["wss://da-flag.example"]), &path).unwrap();

    assert_eq!(choice.source, RelaySource::Flag);
}

// 12
#[test]
fn a_broken_file_is_an_error_when_it_would_be_used() {
    let (_dir, path) = dir_with(Some("isso nao e url\n"));

    assert!(resolve_relays(&[], &path).is_err());
}

// 13
#[test]
fn the_built_in_defaults_are_valid_secure_relays() {
    assert!(!DEFAULT_RELAYS.is_empty());
    for url in DEFAULT_RELAYS {
        assert!(url.starts_with("wss://"), "{url}");
    }
    assert_eq!(
        parse_relay_list(&DEFAULT_RELAYS.join("\n")).unwrap(),
        DEFAULT_RELAYS
    );
}
