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

    let sas = request.sas_code(&primary.id());
    let cert = primary.answer_pair_request(&request, &sas, 2_000).unwrap();

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
    let sas = request.sas_code(&primary.id());
    request.nostr_proof[0] ^= 1;

    assert!(matches!(
        primary.answer_pair_request(&request, &sas, 2_000),
        Err(Error::InvalidPairRequest)
    ));
}

/// Auditoria 2c-i, achado 3: a API não emite certificado sem a confirmação do código.
#[test]
fn the_primary_does_not_certify_without_the_right_confirmation_code() {
    let primary = primary_of(&StandaloneIdentity::generate());
    let request = request_of(&new_device(), NONCE);
    let right = request.sas_code(&primary.id());

    let other_identity = request.sas_code(&StandaloneIdentity::generate().id());
    let other_request = request_of(&new_device(), NONCE).sas_code(&primary.id());
    let off_by_one_digit = {
        let mut chars: Vec<char> = right.chars().collect();
        chars[0] = if chars[0] == '0' { '1' } else { '0' };
        chars.into_iter().collect::<String>()
    };
    for wrong in [
        "",
        "garbage",
        &other_identity,
        &other_request,
        &off_by_one_digit,
    ] {
        assert!(
            matches!(
                primary.answer_pair_request(&request, wrong, 2_000),
                Err(Error::InvalidPairRequest)
            ),
            "certificou com o código {wrong:?}"
        );
    }
    // o certo continua valendo
    primary
        .answer_pair_request(&request, &right, 2_000)
        .unwrap();
}

// ---- o device novo aceita só a identidade esperada (auditoria 2c-i, achado 1) ----

#[test]
fn the_new_device_accepts_the_certificate_of_the_expected_identity() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let request = request_of(&new_device(), NONCE);
    let sas = request.sas_code(&primary.id());
    let cert = primary.answer_pair_request(&request, &sas, 2_000).unwrap();

    request.accept_certificate(&cert, &master.id()).unwrap();
}

/// Ataque do aniversário: quem intercepta devolve um certificado de uma identidade **dele**. O device
/// novo, que sabe a identidade esperada, recusa antes de qualquer comparação de código.
#[test]
fn the_new_device_refuses_a_certificate_of_another_identity() {
    let expected = StandaloneIdentity::generate();
    let intruder = StandaloneIdentity::generate();
    let d = new_device();
    let request = request_of(&d, NONCE);
    let cert = intruder
        .authorize_device(
            &request.signing_key,
            &request.network_key,
            &request.nostr_key,
            2_000,
        )
        .unwrap();
    cert.verify().unwrap(); // o certificado é válido; só não é da identidade esperada

    assert!(matches!(
        request.accept_certificate(&cert, &expected.id()),
        Err(Error::PairCertificateMismatch)
    ));
}

#[test]
fn the_new_device_refuses_a_certificate_for_other_keys() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let mine = request_of(&new_device(), NONCE);
    let theirs = request_of(&new_device(), NONCE);
    let sas = theirs.sas_code(&primary.id());
    let cert_for_theirs = primary.answer_pair_request(&theirs, &sas, 2_000).unwrap();

    assert!(matches!(
        mine.accept_certificate(&cert_for_theirs, &master.id()),
        Err(Error::PairCertificateMismatch)
    ));
}

#[test]
fn the_new_device_refuses_a_certificate_covering_only_some_of_its_keys() {
    let master = StandaloneIdentity::generate();
    let d = new_device();
    let request = request_of(&d, NONCE);
    let other = DeviceKey::generate();
    // MLS e rede certas, Nostr trocada; e depois a chave MLS trocada.
    for (signing, nostr) in [
        (request.signing_key.clone(), other.nostr_public_key()),
        (other.public_key(), request.nostr_key),
    ] {
        let cert = master
            .authorize_device(&signing, &request.network_key, &nostr, 2_000)
            .unwrap();
        assert!(matches!(
            request.accept_certificate(&cert, &master.id()),
            Err(Error::PairCertificateMismatch)
        ));
    }
}

#[test]
fn the_new_device_refuses_a_certificate_with_a_broken_signature() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let request = request_of(&new_device(), NONCE);
    let sas = request.sas_code(&primary.id());
    let mut cert = primary.answer_pair_request(&request, &sas, 2_000).unwrap();
    cert.signature[0] ^= 1;

    assert!(request.accept_certificate(&cert, &master.id()).is_err());
}

// ---- código de comparação (SAS) ----

fn is_sas_format(code: &str) -> bool {
    let groups: Vec<&str> = code.split('-').collect();
    groups.len() == 4
        && groups
            .iter()
            .all(|g| g.len() == 6 && g.bytes().all(|b| b.is_ascii_digit()))
}

#[test]
fn the_sas_code_has_twenty_four_digits_in_four_groups() {
    let request = request_of(&new_device(), NONCE);
    let identity = StandaloneIdentity::generate().id();
    let code = request.sas_code(&identity);
    assert!(is_sas_format(&code), "formato inesperado: {code}");
}

/// O primário calcula sobre o pedido que recebeu (bytes do canal); o device novo, sobre o que gerou, com
/// a identidade que recebeu fora de banda **antes** (nunca a do certificado que voltou: achado 1).
#[test]
fn both_screens_show_the_same_code() {
    let primary = primary_of(&StandaloneIdentity::generate());
    let identity_received_out_of_band = primary.id();
    let request = request_of(&new_device(), NONCE);

    let on_primary = PairRequest::from_bytes(&request.to_bytes())
        .unwrap()
        .sas_code(&primary.id());
    let on_new_device = request.sas_code(&identity_received_out_of_band);

    assert_eq!(on_primary, on_new_device);
}

/// Cada uma das 3 chaves entra no código: esquecer uma deixaria um atacante trocá-la sem mudar o que a
/// pessoa compara.
#[test]
fn the_code_covers_each_of_the_three_keys() {
    let identity = StandaloneIdentity::generate().id();
    let base = request_of(&new_device(), NONCE);
    let before = base.sas_code(&identity);
    let other = DeviceKey::generate();

    let mut a = base.clone();
    a.signing_key = other.public_key();
    let mut b = base.clone();
    b.network_key = other.public_key();
    let mut c = base;
    c.nostr_key = other.nostr_public_key();

    assert_ne!(a.sas_code(&identity), before, "sem a chave MLS");
    assert_ne!(b.sas_code(&identity), before, "sem a chave de rede");
    assert_ne!(c.sas_code(&identity), before, "sem a chave Nostr");
}

// ---- reforços da auditoria 2c-i ----

/// Achado 4: cada chave tem um papel; a mesma chave nos dois slots (MLS = rede) não vale.
#[test]
fn a_request_using_the_same_key_for_two_roles_is_refused() {
    let d = new_device();
    let request = PairRequest::new(&d.net, &d.net.public_key(), NONCE, |payload| {
        Ok(d.net.keypair().sign(payload)?)
    })
    .unwrap();
    assert!(matches!(request.verify(), Err(Error::InvalidPairRequest)));
}

/// Achado 8: o caminho normal sorteia o nonce; dois pedidos do mesmo device nunca repetem.
#[test]
fn generate_draws_a_fresh_nonce_each_time() {
    let d = new_device();
    let sign = |payload: &[u8]| Ok(d.mls.keypair().sign(payload)?);
    let a = PairRequest::generate(&d.net, &d.mls.public_key(), sign).unwrap();
    let b = PairRequest::generate(&d.net, &d.mls.public_key(), sign).unwrap();

    a.verify().unwrap();
    b.verify().unwrap();
    assert_ne!(a.nonce, b.nonce);
    assert_ne!(a.nonce, [0; PAIR_NONCE_LEN]);
}

#[test]
fn an_error_from_the_mls_signer_is_propagated() {
    let d = new_device();
    let result = PairRequest::new(&d.net, &d.mls.public_key(), NONCE, |_| {
        Err(Error::InvalidPairRequest)
    });
    assert!(result.is_err());
}

/// Achado 7: uma prova absurda não pode virar panic no `to_bytes`; `new` a recusa.
#[test]
fn an_oversized_mls_proof_is_refused_when_building() {
    let d = new_device();
    let result = PairRequest::new(&d.net, &d.mls.public_key(), NONCE, |_| Ok(vec![0; 70_000]));
    assert!(matches!(result, Err(Error::InvalidPairRequest)));
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
