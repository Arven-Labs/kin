//! Certificado de device v4 (P30 fatia 2a, ADR 0014): assinado pela chave de autoridade, com a cadeia
//! mestra → autoridade embutida. O v3 (assinado pela mestra) continua valendo.

use kin_identity::{
    AuthorityCertificate, AuthorityIdentity, AuthorityKey, DeviceCertificate, DeviceKey, Error,
    IdentityProvider, Issuer, StandaloneIdentity,
};

/// Uma autoridade certificada por `master` em `created_at = 1_000`.
fn authority_of(master: &StandaloneIdentity) -> (AuthorityKey, AuthorityCertificate) {
    let key = AuthorityKey::generate();
    let cert = master.certify_authority(&key.public_key(), 1_000).unwrap();
    (key, cert)
}

fn primary_of(master: &StandaloneIdentity) -> AuthorityIdentity {
    let (key, cert) = authority_of(master);
    AuthorityIdentity::new(key, cert).unwrap()
}

/// As três chaves de um device novo.
struct Device {
    mls: DeviceKey,
    net: DeviceKey,
}

fn device() -> Device {
    Device {
        mls: DeviceKey::generate(),
        net: DeviceKey::generate(),
    }
}

fn certify(issuer: &dyn IdentityProvider, d: &Device) -> DeviceCertificate {
    issuer
        .authorize_device(
            &d.mls.public_key(),
            &d.net.public_key(),
            &d.net.nostr_public_key(),
            2_000,
        )
        .unwrap()
}

fn authority_cert_of(cert: &DeviceCertificate) -> &AuthorityCertificate {
    match &cert.issuer {
        Issuer::Authority(a) => a,
        Issuer::Master => panic!("esperava um certificado v4"),
    }
}

// ---- emissão e leitura ----

#[test]
fn a_device_certified_by_the_authority_belongs_to_the_master_identity() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let d = device();

    let cert = certify(&primary, &d);

    cert.verify().unwrap();
    assert_eq!(primary.id(), master.id());
    assert_eq!(cert.identity, master.id());
    assert_eq!(authority_cert_of(&cert), primary.certificate());
    assert_eq!(cert.signing_key, d.mls.public_key());
    assert_eq!(cert.network_peer_id(), d.net.peer_id());
    assert_eq!(cert.nostr_key, d.net.nostr_public_key());
    assert_eq!(cert.created_at, 2_000);
}

#[test]
fn a_v4_certificate_survives_serialization_and_starts_with_its_magic() {
    let master = StandaloneIdentity::generate();
    let cert = certify(&primary_of(&master), &device());

    let bytes = cert.to_bytes();
    assert!(bytes.starts_with(b"KDC\x04"), "sem o magic do v4");
    let restored = DeviceCertificate::from_bytes(&bytes).unwrap();

    assert_eq!(restored, cert);
    restored.verify().unwrap();
}

#[test]
fn a_v3_certificate_still_reads_and_verifies_next_to_v4() {
    let master = StandaloneIdentity::generate();
    let v3 = certify(&master, &device());

    assert_eq!(v3.issuer, Issuer::Master);
    // O v3 começa pelo tamanho da identidade (parte alta zero).
    assert_eq!(v3.to_bytes()[0], 0);
    let restored = DeviceCertificate::from_bytes(&v3.to_bytes()).unwrap();
    assert_eq!(restored, v3);
    restored.verify().unwrap();
}

// ---- a cadeia precisa fechar ----

#[test]
fn an_authority_certified_by_another_master_does_not_vouch_for_this_identity() {
    let master = StandaloneIdentity::generate();
    let intruder = StandaloneIdentity::generate();
    // A autoridade do intruso certifica o device, e ele diz que o device é da mestra.
    let mut cert = certify(&primary_of(&intruder), &device());
    cert.identity = master.id();

    assert!(cert.verify().is_err());
}

#[test]
fn a_forged_authority_certificate_breaks_the_chain() {
    let master = StandaloneIdentity::generate();
    let mut cert = certify(&primary_of(&master), &device());
    let Issuer::Authority(authority) = &mut cert.issuer else {
        unreachable!()
    };
    authority.signature[0] ^= 1;

    assert!(cert.verify().is_err());
}

#[test]
fn a_device_signed_by_a_key_other_than_the_certified_authority_is_refused() {
    let master = StandaloneIdentity::generate();
    let (a, b) = (primary_of(&master), primary_of(&master));
    let d = device();
    // Assinado pela autoridade B, mas apresentando a cadeia da autoridade A.
    let mut cert = certify(&b, &d);
    cert.issuer = Issuer::Authority(Box::new(a.certificate().clone()));

    assert!(cert.verify().is_err());
}

#[test]
fn a_master_signature_over_the_same_device_does_not_count_as_the_authority() {
    let master = StandaloneIdentity::generate();
    let primary = primary_of(&master);
    let d = device();
    // Mesmo device, mesmo instante: a assinatura da mestra (v3) com a cadeia do v4 ao lado.
    let mut cert = certify(&master, &d);
    cert.issuer = Issuer::Authority(Box::new(primary.certificate().clone()));

    assert!(cert.verify().is_err());
}

#[test]
fn a_v4_signature_does_not_verify_as_a_v3_certificate_of_the_authority_key() {
    let master = StandaloneIdentity::generate();
    let (key, authority) = authority_of(&master);
    let authority_id = kin_identity::IdentityId::new(key.public_key());
    let primary = AuthorityIdentity::new(key, authority).unwrap();
    // Quem tem a autoridade tenta fazer a assinatura v4 valer como v3 de uma "identidade" que é a
    // própria chave de autoridade: os domínios precisam ser diferentes.
    let mut cert = certify(&primary, &device());
    cert.issuer = Issuer::Master;
    cert.identity = authority_id;

    assert!(cert.verify().is_err());
}

#[test]
fn an_authority_shared_by_two_masters_does_not_move_a_device_between_them() {
    let m1 = StandaloneIdentity::generate();
    let m2 = StandaloneIdentity::generate();
    let key = AuthorityKey::generate();
    let by_m1 = m1.certify_authority(&key.public_key(), 1_000).unwrap();
    let by_m2 = m2.certify_authority(&key.public_key(), 1_000).unwrap();
    let primary = AuthorityIdentity::new(
        AuthorityKey::from_bytes(&key.to_bytes().unwrap()).unwrap(),
        by_m1,
    )
    .unwrap();
    // Device certificado para m1; trocar a cadeia por a de m2 não pode fazê-lo valer para m2.
    let mut cert = certify(&primary, &device());
    cert.issuer = Issuer::Authority(Box::new(by_m2));
    cert.identity = m2.id();

    assert!(cert.verify().is_err());
}

/// Auditoria da 2a, A1: o device amarra **qual** emissão do certificado de autoridade o certificou,
/// não só a chave. Assim a fatia 3/4 pode revogar ou expirar uma emissão sem que o device troque a sua.
#[test]
fn another_emission_for_the_same_authority_key_does_not_fit_the_device() {
    let master = StandaloneIdentity::generate();
    let key = AuthorityKey::generate();
    let first = master.certify_authority(&key.public_key(), 1_000).unwrap();
    let second = master.certify_authority(&key.public_key(), 1_001).unwrap();
    let primary = AuthorityIdentity::new(key, first).unwrap();
    let mut cert = certify(&primary, &device());
    cert.issuer = Issuer::Authority(Box::new(second));

    assert!(cert.verify().is_err());
}

#[test]
fn tampering_with_any_device_field_breaks_a_v4_certificate() {
    let master = StandaloneIdentity::generate();
    let good = certify(&primary_of(&master), &device());
    let other = device();

    let mut signing = good.clone();
    signing.signing_key = other.mls.public_key();
    let mut network = good.clone();
    network.network_key = other.net.public_key();
    let mut nostr = good.clone();
    nostr.nostr_key = other.net.nostr_public_key();
    let mut time = good.clone();
    time.created_at += 1;
    let mut signature = good.clone();
    signature.signature[0] ^= 1;
    let mut empty = good.clone();
    empty.signature.clear();
    let mut identity = good.clone();
    identity.identity = StandaloneIdentity::generate().id();

    for (name, bad) in [
        ("signing_key", signing),
        ("network_key", network),
        ("nostr_key", nostr),
        ("created_at", time),
        ("signature", signature),
        ("empty signature", empty),
        ("identity", identity),
    ] {
        assert!(bad.verify().is_err(), "{name} adulterado foi aceito");
    }
}

// ---- leitura de bytes hostis ----

#[test]
fn a_v4_certificate_cut_at_any_point_is_refused_without_panicking() {
    let master = StandaloneIdentity::generate();
    let bytes = certify(&primary_of(&master), &device()).to_bytes();
    for len in 0..bytes.len() {
        assert!(
            DeviceCertificate::from_bytes(&bytes[..len]).is_err(),
            "aceitou {len} de {} bytes",
            bytes.len()
        );
    }
}

#[test]
fn trailing_bytes_after_a_v4_certificate_are_refused() {
    let master = StandaloneIdentity::generate();
    let mut bytes = certify(&primary_of(&master), &device()).to_bytes();
    bytes.push(0);
    assert!(DeviceCertificate::from_bytes(&bytes).is_err());
}

#[test]
fn an_unknown_version_or_magic_is_refused() {
    let master = StandaloneIdentity::generate();
    let bytes = certify(&primary_of(&master), &device()).to_bytes();

    let mut version = bytes.clone();
    version[3] = 5;
    let mut magic = bytes.clone();
    magic[0] = b'X';

    assert!(DeviceCertificate::from_bytes(&version).is_err());
    assert!(DeviceCertificate::from_bytes(&magic).is_err());
    assert!(DeviceCertificate::from_bytes(b"KDC").is_err());
    assert!(DeviceCertificate::from_bytes(&[0xFF; 300]).is_err());
}

#[test]
fn a_damaged_authority_certificate_inside_a_v4_is_refused_on_read() {
    let master = StandaloneIdentity::generate();
    let cert = certify(&primary_of(&master), &device());
    let mut bytes = cert.to_bytes();
    // Logo depois de "KDC" 4 e do len16 vem o "KAUT" do certificado de autoridade.
    assert_eq!(&bytes[6..10], b"KAUT");
    bytes[6] = b'X';

    assert!(DeviceCertificate::from_bytes(&bytes).is_err());
}

// ---- o primário só monta com a chave certa ----

#[test]
fn the_primary_refuses_an_authority_certificate_for_another_key() {
    let master = StandaloneIdentity::generate();
    let (_, cert) = authority_of(&master);

    let result = AuthorityIdentity::new(AuthorityKey::generate(), cert);

    assert!(matches!(result, Err(Error::AuthorityKeyMismatch)));
}

/// P30 2b: a mestra certificando a si mesma como autoridade a deixaria quente no primário.
#[test]
fn the_primary_refuses_the_master_as_its_own_authority() {
    let master = StandaloneIdentity::generate();
    let as_authority = AuthorityKey::from_bytes(&master.to_bytes().unwrap()).unwrap();
    let cert = master
        .certify_authority(&as_authority.public_key(), 1_000)
        .unwrap();

    let result = AuthorityIdentity::new(as_authority, cert);

    assert!(matches!(result, Err(Error::AuthorityIsMaster)));
}

#[test]
fn the_primary_refuses_an_authority_certificate_with_a_bad_signature() {
    let master = StandaloneIdentity::generate();
    let (key, mut cert) = authority_of(&master);
    cert.signature[0] ^= 1;

    let result = AuthorityIdentity::new(key, cert);

    assert!(matches!(result, Err(Error::InvalidAuthorityCertificate)));
}
