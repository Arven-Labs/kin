//! P13 fatia 2 (ADR 0045): `DbKey`, a chave de 32 bytes dos bancos SQLCipher. Ela nunca pode sair em `Debug`.

use kin_identity::DbKey;

const BYTES: [u8; 32] = [0xAB; 32];

#[test]
fn debug_does_not_show_the_key() {
    let key = DbKey::from_bytes(BYTES);
    let shown = format!("{key:?}");
    // Nem em hex, nem como lista de bytes (171 = 0xAB).
    assert!(!shown.to_lowercase().contains("ab"));
    assert!(!shown.contains("171"));
}

#[test]
fn the_bytes_come_back_as_given() {
    assert_eq!(DbKey::from_bytes(BYTES).expose_secret(), &BYTES);
}
