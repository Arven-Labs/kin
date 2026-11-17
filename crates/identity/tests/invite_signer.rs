//! `InviteSigner` e `DeviceCertificate::verify_invite` (P30, assinatura do link de convite, ADR 0033):
//! só a chave de rede, com o domínio `kin/invite/v2` imposto dentro do crate.

use kin_identity::{DeviceCertificate, DeviceKey, IdentityProvider, StandaloneIdentity};

const DOMAIN: &[u8] = b"kin/invite/v2";
const BODY: &[u8] = b"corpo do link: certificado, enderecos e validade";

fn certified(device: &DeviceKey) -> DeviceCertificate {
    StandaloneIdentity::generate()
        .authorize_device(
            &DeviceKey::generate().public_key(),
            &device.public_key(),
            &device.nostr_public_key(),
            1_000,
        )
        .unwrap()
}

#[test]
fn a_signature_from_the_device_signer_verifies_against_its_own_certificate() {
    let device = DeviceKey::generate();
    let cert = certified(&device);

    let signature = device.invite_signer().sign(BODY);

    assert!(cert.verify_invite(BODY, &signature));
}

#[test]
fn another_body_another_device_or_a_damaged_signature_do_not_verify() {
    let (device, other) = (DeviceKey::generate(), DeviceKey::generate());
    let cert = certified(&device);
    let signature = device.invite_signer().sign(BODY);

    assert!(!cert.verify_invite(b"outro corpo", &signature));
    assert!(!cert.verify_invite(BODY, &other.invite_signer().sign(BODY)));
    let mut damaged = signature.clone();
    damaged[0] ^= 1;
    assert!(!cert.verify_invite(BODY, &damaged));
    assert!(!cert.verify_invite(BODY, &signature[..signature.len() - 1]));
    assert!(!cert.verify_invite(BODY, &[]));
}

/// O domínio é imposto por dentro: o signer assina `kin/invite/v2 | corpo`, não o corpo cru.
#[test]
fn the_signer_enforces_the_invite_domain() {
    let device = DeviceKey::generate();
    let cert = certified(&device);

    // Assinatura do corpo cru (outro protocolo, ou um chamador esquecido): recusada.
    let raw = device.keypair().sign(BODY).unwrap();
    assert!(!cert.verify_invite(BODY, &raw));

    // O formato fixo: domínio + corpo, assinado à mão com a chave de rede, é aceito.
    let by_hand = device.keypair().sign(&[DOMAIN, BODY].concat()).unwrap();
    assert!(cert.verify_invite(BODY, &by_hand));

    // E o que o signer produz é exatamente isso (nada a mais na mensagem assinada).
    assert!(
        device
            .public_key()
            .verify(&[DOMAIN, BODY].concat(), &device.invite_signer().sign(BODY))
    );
    assert!(
        !device
            .public_key()
            .verify(BODY, &device.invite_signer().sign(BODY))
    );
}

/// Uma assinatura de outro domínio do Kin (ex.: o certificado) não vale como convite.
#[test]
fn a_signature_of_another_kin_domain_is_not_an_invite_signature() {
    let device = DeviceKey::generate();
    let cert = certified(&device);

    for domain in [&b"kin/device-cert/v4"[..], b"kin/invite/v1", b"kin/pair/v1"] {
        let other = device.keypair().sign(&[domain, BODY].concat()).unwrap();
        assert!(
            !cert.verify_invite(BODY, &other),
            "aceitou o domínio {domain:?}"
        );
    }
}
