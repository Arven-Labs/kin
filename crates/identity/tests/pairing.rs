//! Pedido de pareamento (P30 fatia 2c, ADR 0016): prova de posse das três chaves, resposta do primário
//! com o certificado v4 e código de comparação (SAS).

use kin_identity::{
    AuthorityIdentity, AuthorityKey, DeviceKey, Error, IdentityId, IdentityProvider,
    PAIR_NONCE_LEN, PairRequest, StandaloneIdentity,
};

const NONCE: [u8; PAIR_NONCE_LEN] = [7; PAIR_NONCE_LEN];

/// Um device novo: `mls` faz o papel da chave MLS (Ed25519, que no app é do openmls) e `net` traz a
/// chave de rede e a Nostr.
struct NewDevice {
    mls: DeviceKey,
    net: DeviceKey,
}

fn new_device() -> NewDevice {
    NewDevice {
        mls: DeviceKey::generate(),
        net: DeviceKey::generate(),
    }
}

fn request_of(d: &NewDevice, nonce: [u8; PAIR_NONCE_LEN]) -> PairRequest {
    PairRequest::new(&d.net, &d.mls.public_key(), nonce, |payload| {
        Ok(d.mls.keypair().sign(payload)?)
    })
    .unwrap()
}

fn primary_of(master: &StandaloneIdentity) -> AuthorityIdentity {
    let key = AuthorityKey::generate();
    let cert = master.certify_authority(&key.public_key(), 1_000).unwrap();
    AuthorityIdentity::new(key, cert).unwrap()
}

// ---- o pedido ----

#[test]
fn a_request_carries_the_three_keys_and_verifies() {
    let d = new_device();
    let request = request_of(&d, NONCE);

    request.verify().unwrap();
    assert_eq!(request.signing_key, d.mls.public_key());
    assert_eq!(request.network_key, d.net.public_key());
    assert_eq!(request.nostr_key, d.net.nostr_public_key());
    assert_eq!(request.nonce, NONCE);
}

#[test]
fn a_request_survives_serialization() {
    let request = request_of(&new_device(), NONCE);

    let restored = PairRequest::from_bytes(&request.to_bytes()).unwrap();

    assert_eq!(restored, request);
    restored.verify().unwrap();
}

// ---- prova de posse: cada chave precisa assinar ----

#[test]
fn a_request_whose_mls_proof_is_garbage_is_refused() {
    let d = new_device();
    let mut request = request_of(&d, NONCE);
    request.signing_proof[0] ^= 1;
    assert!(matches!(request.verify(), Err(Error::InvalidPairRequest)));
}

#[test]
fn a_request_whose_network_proof_is_garbage_is_refused() {
    let mut request = request_of(&new_device(), NONCE);
    request.network_proof[0] ^= 1;
    assert!(matches!(request.verify(), Err(Error::InvalidPairRequest)));
}

#[test]
fn a_request_whose_nostr_proof_is_garbage_is_refused() {
    let mut request = request_of(&new_device(), NONCE);
    request.nostr_proof[0] ^= 1;
    assert!(matches!(request.verify(), Err(Error::InvalidPairRequest)));
}

/// Sem prova de posse: o atacante põe a chave MLS da vítima no pedido, mas só consegue assinar com a
/// dele. O pedido não pode valer.
#[test]
fn a_request_claiming_a_key_the_signer_does_not_hold_is_refused() {
    let d = new_device();
    let victim = DeviceKey::generate();
    let request = PairRequest::new(&d.net, &victim.public_key(), NONCE, |payload| {
        Ok(d.mls.keypair().sign(payload)?)
    })
    .unwrap();
    assert!(matches!(request.verify(), Err(Error::InvalidPairRequest)));
}

#[test]
fn swapping_the_network_key_of_a_valid_request_is_refused() {
    let mut request = request_of(&new_device(), NONCE);
    request.network_key = DeviceKey::generate().public_key();
    assert!(matches!(request.verify(), Err(Error::InvalidPairRequest)));
}

#[test]
fn swapping_the_nostr_key_of_a_valid_request_is_refused() {
    let mut request = request_of(&new_device(), NONCE);
    request.nostr_key = DeviceKey::generate().nostr_public_key();
    assert!(matches!(request.verify(), Err(Error::InvalidPairRequest)));
}

#[test]
fn changing_the_nonce_of_a_valid_request_is_refused() {
    let mut request = request_of(&new_device(), NONCE);
    request.nonce[0] ^= 1;
    assert!(matches!(request.verify(), Err(Error::InvalidPairRequest)));
}

/// A assinatura da chave de rede não vale como prova da chave MLS (e vice-versa): cada uma assina
/// por si, e a troca é pega.
#[test]
fn proofs_cannot_be_swapped_between_keys() {
    let mut request = request_of(&new_device(), NONCE);
    std::mem::swap(&mut request.signing_proof, &mut request.network_proof);
    assert!(matches!(request.verify(), Err(Error::InvalidPairRequest)));
}

// ---- leitura estrita ----

#[test]
fn a_truncated_request_is_refused() {
    let bytes = request_of(&new_device(), NONCE).to_bytes();
    for cut in [0, 1, 4, bytes.len() / 2, bytes.len() - 1] {
        assert!(
            PairRequest::from_bytes(&bytes[..cut]).is_err(),
            "aceitou truncado em {cut}"
        );
    }
}

#[test]
fn a_request_with_trailing_bytes_is_refused() {
    let mut bytes = request_of(&new_device(), NONCE).to_bytes();
    bytes.push(0);
    assert!(PairRequest::from_bytes(&bytes).is_err());
}

#[test]
fn a_request_with_another_magic_or_version_is_refused() {
    let bytes = request_of(&new_device(), NONCE).to_bytes();
    assert!(bytes.starts_with(b"KPR\x01"), "sem o magic e a versão 1");

    let mut wrong_magic = bytes.clone();
    wrong_magic[0] ^= 1;
    assert!(PairRequest::from_bytes(&wrong_magic).is_err());

    let mut wrong_version = bytes;
    wrong_version[3] = 2;
    assert!(PairRequest::from_bytes(&wrong_version).is_err());
}

#[test]
fn garbage_is_never_a_request() {
    assert!(PairRequest::from_bytes(&[]).is_err());
    assert!(PairRequest::from_bytes(&[0xff; 300]).is_err());
}

// ---- a resposta do primário ----

#[test]
fn the_primary_certifies_the_keys_of_a_valid_request_in_v4() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let d = new_device();
    let request = request_of(&d, NONCE);

    let cert = primary.answer_pair_request(&request, 2_000).unwrap();

    cert.verify().unwrap();
    assert_eq!(cert.identity(), &master.id());
    assert_eq!(cert.signing_key, request.signing_key);
    assert_eq!(cert.network_key, request.network_key);
    assert_eq!(cert.nostr_key, request.nostr_key);
    assert_eq!(cert.created_at, 2_000);
    assert!(cert.to_bytes().starts_with(b"KDC\x04"), "esperava v4");
}

#[test]
fn the_primary_refuses_a_request_without_proof_of_possession() {
    let primary = primary_of(&StandaloneIdentity::generate());
    let mut request = request_of(&new_device(), NONCE);
    request.nostr_proof[0] ^= 1;

    assert!(matches!(
        primary.answer_pair_request(&request, 2_000),
        Err(Error::InvalidPairRequest)
    ));
}

// ---- código de comparação (SAS) ----

fn is_sas_format(code: &str) -> bool {
    let groups: Vec<&str> = code.split('-').collect();
    groups.len() == 3
        && groups
            .iter()
            .all(|g| g.len() == 6 && g.bytes().all(|b| b.is_ascii_digit()))
}

#[test]
fn the_sas_code_has_eighteen_digits_in_three_groups() {
    let request = request_of(&new_device(), NONCE);
    let identity = StandaloneIdentity::generate().id();
    let code = request.sas_code(&identity);
    assert!(is_sas_format(&code), "formato inesperado: {code}");
}

/// O primário calcula sobre o pedido que recebeu; o device novo, sobre o que mostrou, com a identidade
/// do certificado que voltou. As duas telas só mostram o mesmo código se pedido e identidade batem.
#[test]
fn both_screens_show_the_same_code() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let request = request_of(&new_device(), NONCE);

    let on_primary = PairRequest::from_bytes(&request.to_bytes())
        .unwrap()
        .sas_code(&primary.id());
    let cert = primary.answer_pair_request(&request, 2_000).unwrap();
    let on_new_device = request.sas_code(cert.identity());

    assert_eq!(on_primary, on_new_device);
}

#[test]
fn the_code_changes_with_the_identity() {
    let request = request_of(&new_device(), NONCE);
    let a = StandaloneIdentity::generate().id();
    let b = StandaloneIdentity::generate().id();
    assert_ne!(a, b);
    assert_ne!(request.sas_code(&a), request.sas_code(&b));
}

#[test]
fn the_code_changes_with_the_request() {
    let identity: IdentityId = StandaloneIdentity::generate().id();
    let d = new_device();
    let base = request_of(&d, NONCE).sas_code(&identity);

    // outro nonce
    assert_ne!(
        request_of(&d, [8; PAIR_NONCE_LEN]).sas_code(&identity),
        base
    );
    // outro device
    assert_ne!(request_of(&new_device(), NONCE).sas_code(&identity), base);
}

/// Mesmo nonce e mesmas chaves: o código é determinístico (as assinaturas Ed25519 são, as Schnorr
/// podem não ser, então o código **não** pode depender das assinaturas).
#[test]
fn the_code_does_not_depend_on_the_signatures() {
    let identity = StandaloneIdentity::generate().id();
    let mut request = request_of(&new_device(), NONCE);
    let before = request.sas_code(&identity);

    request.nostr_proof = vec![0; 64];
    request.signing_proof = vec![0; 64];
    request.network_proof = vec![0; 64];

    assert_eq!(request.sas_code(&identity), before);
}
