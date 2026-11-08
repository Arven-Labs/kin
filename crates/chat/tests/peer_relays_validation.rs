//! P23 PR 2: validação da lista de relays que um contato anuncia. Entrada hostil: é o contato quem
//! escolhe a que host o NOSSO cliente conecta. Sem rede.

use kin_chat::nostr::{MAX_PEER_RELAYS, validate_peer_relays};

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

// 1
#[test]
fn up_to_the_limit_of_wss_relays_is_accepted_in_order() {
    let list = strings(&["wss://a.example", "wss://b.example", "wss://c.example"]);

    assert_eq!(validate_peer_relays(&list).unwrap(), list);
    assert_eq!(MAX_PEER_RELAYS, 3);
}

// 2
#[test]
fn one_more_than_the_limit_rejects_the_whole_list() {
    let list = strings(&[
        "wss://a.example",
        "wss://b.example",
        "wss://c.example",
        "wss://d.example",
    ]);

    assert!(validate_peer_relays(&list).is_err());
}

// 3
#[test]
fn an_empty_list_is_rejected() {
    assert!(validate_peer_relays(&[]).is_err());
}

// 4
#[test]
fn plain_ws_is_rejected_even_for_this_computer() {
    for url in [
        "ws://relay.example",
        "ws://localhost:7777",
        "ws://127.0.0.1",
    ] {
        assert!(
            validate_peer_relays(&strings(&[url])).is_err(),
            "aceitou {url}"
        );
    }
}

// 5
#[test]
fn local_and_literal_ip_hosts_are_rejected_even_over_wss() {
    for url in [
        "wss://localhost",
        "wss://127.0.0.1",
        "wss://10.0.0.5",
        "wss://192.168.1.1:8443",
        "wss://169.254.169.254",
        "wss://[::1]",
        "wss://8.8.8.8",
    ] {
        assert!(
            validate_peer_relays(&strings(&[url])).is_err(),
            "aceitou {url}"
        );
    }
}

// 6
#[test]
fn credentials_in_the_url_are_rejected() {
    assert!(validate_peer_relays(&strings(&["wss://user:senha@relay.example"])).is_err());
}

// 7
#[test]
fn a_url_over_200_bytes_is_rejected() {
    let long = format!("wss://{}.example", "a".repeat(200));

    assert!(validate_peer_relays(&[long]).is_err());
}

// 8
#[test]
fn text_that_is_not_a_url_is_rejected() {
    assert!(validate_peer_relays(&strings(&["isso nao e url"])).is_err());
}

// 9
#[test]
fn one_bad_item_rejects_the_whole_list() {
    let list = strings(&["wss://ok.example", "ws://localhost:7777"]);

    assert!(validate_peer_relays(&list).is_err());
}

// 10
#[test]
fn the_same_relay_written_twice_counts_once() {
    let list = strings(&["wss://a.example", "wss://A.example/", "wss://b.example"]);

    assert_eq!(
        validate_peer_relays(&list).unwrap(),
        ["wss://a.example", "wss://b.example"]
    );
}
