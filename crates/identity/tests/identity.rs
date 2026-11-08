use kin_identity::{DeviceKey, IdentityProvider, NostrPublicKey, StandaloneIdentity};

#[test]
fn identity_signature_verifies_and_rejects_tampering() {
    let id = StandaloneIdentity::generate();
    let sig = id.sign(b"ola").unwrap();
    assert!(id.id().public_key().verify(b"ola", &sig));
    assert!(!id.id().public_key().verify(b"outra", &sig));
}

#[test]
fn device_certificate_verifies() {
    let id = StandaloneIdentity::generate();
    let (mls, device) = (DeviceKey::generate(), DeviceKey::generate());
    let cert = id
        .authorize_device(
            &mls.public_key(),
            &device.public_key(),
            &device.nostr_public_key(),
            1_000,
        )
        .unwrap();
    cert.verify().unwrap();
    assert_eq!(cert.identity, id.id());
    assert_eq!(cert.signing_key, mls.public_key());
    assert_eq!(cert.network_peer_id(), device.peer_id());
}

#[test]
fn certificate_from_another_identity_or_tampered_fails() {
    let id = StandaloneIdentity::generate();
    let other = StandaloneIdentity::generate();
    let (mls, net) = (
        DeviceKey::generate().public_key(),
        DeviceKey::generate().public_key(),
    );
    let nostr = DeviceKey::generate().nostr_public_key();
    let issue = || id.authorize_device(&mls, &net, &nostr, 1_000).unwrap();

    let mut cert = issue();
    cert.created_at += 1;
    assert!(cert.verify().is_err());

    let mut forged = issue();
    forged.identity = other.id();
    assert!(forged.verify().is_err());

    let mut swapped = issue();
    swapped.signing_key = DeviceKey::generate().public_key();
    assert!(swapped.verify().is_err());

    // Trocar só a chave de rede também invalida (é o que impede reaproveitar o certificado, P15).
    let mut renet = issue();
    renet.network_key = DeviceKey::generate().public_key();
    assert!(renet.verify().is_err());

    // Inverter as duas chaves também não passa: o payload preserva a ordem.
    let mut flipped = issue();
    std::mem::swap(&mut flipped.signing_key, &mut flipped.network_key);
    assert!(flipped.verify().is_err());
}

#[test]
fn peer_id_is_stable_across_serialization() {
    let device = DeviceKey::generate();
    let restored = DeviceKey::from_bytes(&device.to_bytes().unwrap()).unwrap();
    assert_eq!(device.peer_id(), restored.peer_id());
}

#[test]
fn identity_roundtrips_through_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("identity.key");
    let id = StandaloneIdentity::generate();
    id.save(&path).unwrap();

    let loaded = StandaloneIdentity::load(&path).unwrap();
    assert_eq!(loaded.id(), id.id());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn rejects_non_ed25519_or_garbage_bytes() {
    assert!(StandaloneIdentity::from_bytes(b"lixo").is_err());
    assert!(DeviceKey::from_bytes(&[]).is_err());
}

#[test]
fn certificate_roundtrips_through_bytes_and_still_verifies() {
    let id = StandaloneIdentity::generate();
    let cert = id
        .authorize_device(
            &DeviceKey::generate().public_key(),
            &DeviceKey::generate().public_key(),
            &DeviceKey::generate().nostr_public_key(),
            42,
        )
        .unwrap();

    let restored = kin_identity::DeviceCertificate::from_bytes(&cert.to_bytes()).unwrap();
    assert_eq!(restored, cert);
    restored.verify().unwrap();
}

#[test]
fn certificate_from_bytes_rejects_truncated_and_trailing_data() {
    let id = StandaloneIdentity::generate();
    let cert = id
        .authorize_device(
            &DeviceKey::generate().public_key(),
            &DeviceKey::generate().public_key(),
            &DeviceKey::generate().nostr_public_key(),
            1,
        )
        .unwrap();
    let bytes = cert.to_bytes();

    assert!(kin_identity::DeviceCertificate::from_bytes(&bytes[..bytes.len() - 1]).is_err());
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(kin_identity::DeviceCertificate::from_bytes(&extra).is_err());
    assert!(kin_identity::DeviceCertificate::from_bytes(&[]).is_err());
}

#[test]
fn ed25519_key_helpers_roundtrip() {
    let key = DeviceKey::generate().public_key();
    let raw = kin_identity::ed25519_bytes(&key).unwrap();
    assert_eq!(kin_identity::public_key_from_ed25519(&raw).unwrap(), key);
    assert!(kin_identity::public_key_from_ed25519(&[1, 2, 3]).is_err());
}

// --- Certificado v3: chave Nostr (secp256k1, ADR 0004) ---

fn issue(id: &StandaloneIdentity, nostr: &NostrPublicKey) -> kin_identity::DeviceCertificate {
    id.authorize_device(
        &DeviceKey::generate().public_key(),
        &DeviceKey::generate().public_key(),
        nostr,
        1_000,
    )
    .unwrap()
}

#[test]
fn each_device_gets_its_own_independent_nostr_key() {
    let (a, b) = (DeviceKey::generate(), DeviceKey::generate());
    assert_ne!(a.nostr_public_key(), b.nostr_public_key());
    assert_eq!(a.nostr_public_key().to_bytes().len(), 32);
}

#[test]
fn certificate_carries_the_nostr_key_and_it_survives_serialization() {
    let id = StandaloneIdentity::generate();
    let nostr = DeviceKey::generate().nostr_public_key();
    let cert = issue(&id, &nostr);
    cert.verify().unwrap();
    assert_eq!(cert.nostr_key, nostr);

    let restored = kin_identity::DeviceCertificate::from_bytes(&cert.to_bytes()).unwrap();
    assert_eq!(restored.nostr_key, nostr);
    restored.verify().unwrap();
}

#[test]
fn swapping_the_nostr_key_of_a_signed_certificate_invalidates_it() {
    let id = StandaloneIdentity::generate();
    let mut cert = issue(&id, &DeviceKey::generate().nostr_public_key());
    cert.nostr_key = DeviceKey::generate().nostr_public_key();
    assert!(cert.verify().is_err());
}

#[test]
fn certificate_with_a_nostr_key_that_is_not_a_curve_point_is_rejected_on_read() {
    let id = StandaloneIdentity::generate();
    let nostr = DeviceKey::generate().nostr_public_key();
    let bytes = issue(&id, &nostr).to_bytes();
    let raw = nostr.to_bytes();
    let at = bytes
        .windows(32)
        .position(|w| w == raw)
        .expect("a chave Nostr está no certificado serializado");

    // x = 0xFF..FF é maior que o primo do corpo: não existe ponto com essa coordenada.
    let mut forged = bytes.clone();
    forged[at..at + 32].fill(0xFF);
    assert!(kin_identity::DeviceCertificate::from_bytes(&forged).is_err());
    assert!(NostrPublicKey::from_bytes(&[0xFF; 32]).is_err());
    assert!(NostrPublicKey::from_bytes(&[1, 2, 3]).is_err());
}

#[test]
fn device_key_keeps_both_keys_across_serialization() {
    let device = DeviceKey::generate();
    let restored = DeviceKey::from_bytes(&device.to_bytes().unwrap()).unwrap();
    assert_eq!(restored.peer_id(), device.peer_id());
    assert_eq!(restored.nostr_public_key(), device.nostr_public_key());
}

#[test]
fn device_key_file_without_the_nostr_key_is_rejected() {
    // Formato antigo: só o keypair Ed25519 em protobuf, sem versão nem chave Nostr.
    let old = libp2p_identity::Keypair::generate_ed25519()
        .to_protobuf_encoding()
        .unwrap();
    assert!(DeviceKey::from_bytes(&old).is_err());
}

mod pair_link {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use kin_identity::{IdentityProvider, StandaloneIdentity};

    fn link_of(bytes: &[u8]) -> String {
        format!("kin://pair/{}", URL_SAFE_NO_PAD.encode(bytes))
    }

    #[test]
    fn a_pair_link_restores_the_same_identity() {
        let original = StandaloneIdentity::generate();

        let restored =
            StandaloneIdentity::from_pair_link(&original.to_pair_link().unwrap()).unwrap();

        assert_eq!(restored.id(), original.id());
        let signature = restored.sign(b"ola").unwrap();
        assert!(original.id().public_key().verify(b"ola", &signature));
    }

    #[test]
    fn whitespace_around_the_link_is_accepted() {
        let original = StandaloneIdentity::generate();
        let link = format!("  {}\r\n", original.to_pair_link().unwrap());

        assert_eq!(
            StandaloneIdentity::from_pair_link(&link).unwrap().id(),
            original.id()
        );
    }

    #[test]
    fn malformed_links_are_rejected_without_echoing_their_content() {
        let key = StandaloneIdentity::generate().to_bytes().unwrap();
        let body = URL_SAFE_NO_PAD.encode(&key);
        let secret_looking = "SEGREDOSEGREDOSEGREDO";
        let cases = [
            ("", "vazio"),
            ("kin://pair/", "sem corpo"),
            (&format!("kin://invite/{body}"), "prefixo de convite"),
            (&format!("kin://PAIR/{body}"), "prefixo em maiúsculas"),
            (&body, "sem prefixo"),
            (&format!("kin://pair/{secret_looking}!!"), "base64 inválido"),
            (&link_of(b"nao e uma chave"), "bytes que não são chave"),
            (&link_of(&key[..key.len() - 1]), "chave truncada"),
        ];

        for (link, why) in cases {
            let error = StandaloneIdentity::from_pair_link(link)
                .err()
                .unwrap_or_else(|| panic!("devia recusar: {why}"));
            let text = format!("{error} {error:?}");
            assert!(
                !text.contains(secret_looking) && !text.contains(&body),
                "vazou: {why}"
            );
        }
    }

    #[test]
    fn a_link_never_holds_a_device_key() {
        // O arquivo do device tem outro formato (versão + Nostr): não pode virar identidade.
        let device = kin_identity::DeviceKey::generate().to_bytes().unwrap();

        assert!(StandaloneIdentity::from_pair_link(&link_of(&device)).is_err());
    }
}
