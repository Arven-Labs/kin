//! P13 fatia 2 (ADR 0045): `Chat::open` com `Option<&DbKey>` cifra os **dois** bancos (`a.db` do crypto e
//! `a.outbox.db` do chat) com a mesma chave, e o histórico em texto puro some dos bytes do disco.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{Person, connect, until_delivered};
use kin_chat::{Chat, Error};
use kin_crypto::Error as CryptoError;
use kin_identity::{DbKey, IdentityProvider};
use kin_transport::NodeConfig;

const SECRET: &str = "texto-conhecido-do-teste-p13-chat";
const PLAIN_HEADER: &[u8; 16] = b"SQLite format 3\0";

fn key() -> DbKey {
    DbKey::from_bytes([0x33; 32])
}

fn open(person: &Person, key: Option<&DbKey>) -> Result<Chat, Error> {
    Chat::open(
        &person.identity,
        &person.device,
        NodeConfig {
            mdns: false,
            ..Default::default()
        },
        &person.db,
        key,
    )
}

fn outbox_of(db: &Path) -> PathBuf {
    db.with_extension("outbox.db")
}

fn wal_of(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", path.display()))
}

fn bytes_contain(path: &Path, needle: &[u8]) -> bool {
    fs::read(path)
        .map(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
        .unwrap_or(false)
}

fn files_of(person: &Person) -> Vec<PathBuf> {
    let outbox = outbox_of(&person.db);
    vec![
        person.db.clone(),
        wal_of(&person.db),
        outbox.clone(),
        wal_of(&outbox),
    ]
}

/// A e B conversam com a chave dada (`None` = em claro); A envia `SECRET`. Devolve o chat de A (aberto).
async fn talk(a: &Person, b: &Person, key: Option<&DbKey>) -> Chat {
    let (mut chat_a, mut chat_b) = (open(a, key).unwrap(), open(b, key).unwrap());
    connect(&mut chat_a, &mut chat_b).await;
    let peer = b.peer_id();
    chat_a.send(peer, SECRET, None).unwrap();
    until_delivered(&mut chat_a, &mut chat_b, 1, 1).await;
    chat_a
}

#[tokio::test(flavor = "multi_thread")]
async fn with_a_key_no_text_reaches_the_disk_of_either_database() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let k = key();
    // Fecha (o drop do `Chat` fecha as conexões) antes de ler os arquivos.
    drop(talk(&a, &b, Some(&k)).await);

    for file in files_of(&a) {
        assert!(
            !bytes_contain(&file, SECRET.as_bytes()),
            "{}",
            file.display()
        );
    }
    for file in [a.db.clone(), outbox_of(&a.db)] {
        assert!(!fs::read(&file).unwrap().starts_with(PLAIN_HEADER));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_key_the_text_is_on_disk_as_before() {
    // Controle: prova que os testes acima olham o lugar certo.
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    drop(talk(&a, &b, None).await);

    assert!(
        files_of(&a)
            .iter()
            .any(|f| bytes_contain(f, SECRET.as_bytes()))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_encrypted_chat_reopens_with_its_history() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let k = key();
    drop(talk(&a, &b, Some(&k)).await);

    let reopened = open(&a, Some(&k)).unwrap();
    let history = reopened.history(&b.identity.id(), None, 10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].message.text, SECRET);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_encrypted_chat_refuses_to_open_without_the_key_or_with_another() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    drop(talk(&a, &b, Some(&key())).await);

    assert!(matches!(
        open(&a, None),
        Err(Error::Crypto(CryptoError::DbKeyRequired))
    ));
    assert!(matches!(
        open(&a, Some(&DbKey::from_bytes([0x44; 32]))),
        Err(Error::Crypto(CryptoError::DbWrongKey))
    ));
}

/// O downgrade do segundo arquivo: o `mls.sqlite` continua cifrado, mas alguém troca só o `outbox.db` por um
/// banco em claro. Tem de falhar, e não abrir o histórico "limpo".
#[tokio::test(flavor = "multi_thread")]
async fn a_plain_outbox_next_to_an_encrypted_database_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (Person::new(dir.path(), "a"), Person::new(dir.path(), "b"));
    let k = key();
    drop(talk(&a, &b, Some(&k)).await);

    let outbox = outbox_of(&a.db);
    fs::remove_file(&outbox).unwrap();
    let _ = fs::remove_file(wal_of(&outbox));
    // Um banco em claro no lugar (aberto pelo caminho sem chave, o que a API antiga fazia).
    rusqlite::Connection::open(&outbox)
        .unwrap()
        .execute_batch("CREATE TABLE x (v INTEGER);")
        .unwrap();

    assert!(open(&a, Some(&k)).is_err());
}
