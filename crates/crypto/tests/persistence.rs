use kin_crypto::{Conversation, CryptoDevice, Decrypted, Error, Expected};
use kin_identity::{DeviceKey, IdentityProvider, NostrPublicKey, PublicKey, StandaloneIdentity};

fn text(d: Decrypted) -> Vec<u8> {
    match d {
        Decrypted::Message { plaintext, .. } => plaintext,
        other => panic!("esperava mensagem, veio {other:?}"),
    }
}

#[test]
fn conversation_survives_a_restart_on_both_sides() {
    let (net_ana, net_bia) = (key(), key());
    let (nostr_ana, nostr_bia) = (nostr(), nostr());
    let dir = tempfile::tempdir().unwrap();
    let (db_ana, db_bia) = (dir.path().join("ana.db"), dir.path().join("bia.db"));
    let (ana, bia) = (
        StandaloneIdentity::generate(),
        StandaloneIdentity::generate(),
    );

    let (id, in_flight) = {
        let (dev_ana, dev_bia) = (
            CryptoDevice::open(&ana, &net_ana, &nostr_ana, &db_ana, 1).unwrap(),
            CryptoDevice::open(&bia, &net_bia, &nostr_bia, &db_bia, 1).unwrap(),
        );
        let mut c_ana = Conversation::create(&dev_ana).unwrap();
        let invite = c_ana
            .invite(&dev_bia.key_package().unwrap(), &Expected::default())
            .unwrap();
        let mut c_bia =
            Conversation::join(&dev_bia, &invite.welcome, &Expected::default()).unwrap();
        dev_ana.remember("bia", &c_ana.id()).unwrap();
        dev_bia.remember("ana", &c_bia.id()).unwrap();

        let first = c_ana.encrypt(b"antes").unwrap();
        assert_eq!(text(c_bia.decrypt(&first).unwrap()), b"antes");
        // Cifrada antes do restart, entregue só depois.
        (c_ana.id(), c_ana.encrypt(b"em voo").unwrap())
    };

    let dev_ana = CryptoDevice::open(&ana, &net_ana, &nostr_ana, &db_ana, 2).unwrap();
    let dev_bia = CryptoDevice::open(&bia, &net_bia, &nostr_bia, &db_bia, 2).unwrap();
    assert_eq!(
        dev_ana.remembered().unwrap(),
        vec![("bia".into(), id.clone())]
    );
    let mut c_ana = Conversation::load(&dev_ana, &id)
        .unwrap()
        .expect("conversa da ana");
    let mut c_bia = Conversation::load(&dev_bia, &id)
        .unwrap()
        .expect("conversa da bia");
    assert_eq!(c_ana.peers().unwrap(), vec![bia.id()]);

    assert_eq!(text(c_bia.decrypt(&in_flight).unwrap()), b"em voo");
    let back = c_bia.encrypt(b"depois").unwrap();
    assert_eq!(text(c_ana.decrypt(&back).unwrap()), b"depois");
    // Estado persistido também lembra o que já foi consumido.
    assert!(matches!(c_bia.decrypt(&in_flight), Err(Error::Duplicate)));
}

#[test]
fn reopening_keeps_the_device_key_and_rejects_another_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kin.db");
    let ana = StandaloneIdentity::generate();
    let net = key();
    let nostr = nostr();
    let first = CryptoDevice::open(&ana, &net, &nostr, &path, 1).unwrap();
    // O KeyPackage carrega a chave do device: recarregar não pode trocá-la.
    let kp_before = Conversation::create(&first).unwrap().id();
    drop(first);
    let second = CryptoDevice::open(&ana, &net, &nostr, &path, 99).unwrap();
    assert!(Conversation::load(&second, &kp_before).unwrap().is_some());

    let other = StandaloneIdentity::generate();
    assert!(matches!(
        CryptoDevice::open(&other, &net, &nostr, &path, 1),
        Err(Error::IdentityMismatch)
    ));
    // P15: o certificado salvo cobre a chave de rede antiga; outra chave de rede não serve.
    assert!(matches!(
        CryptoDevice::open(&ana, &key(), &nostr, &path, 1),
        Err(Error::DeviceMismatch)
    ));
    assert!(
        Conversation::load(&second, b"nao existe")
            .unwrap()
            .is_none()
    );
}

#[test]
fn the_nostr_key_is_certified_and_reopening_with_another_one_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kin.db");
    let ana = StandaloneIdentity::generate();
    let device = DeviceKey::generate();
    let (net, nostr) = (device.public_key(), device.nostr_public_key());

    let first = CryptoDevice::open(&ana, &net, &nostr, &path, 1).unwrap();
    assert_eq!(first.certificate().nostr_key, nostr);
    first.certificate().verify().unwrap();
    drop(first);

    // Mesma chave: reabre, e o certificado salvo continua cobrindo a chave Nostr.
    let again = CryptoDevice::open(&ana, &net, &nostr, &path, 99).unwrap();
    assert_eq!(again.certificate().nostr_key, nostr);
    drop(again);

    // ADR 0004: outra chave Nostr não é a que o certificado salvo cobre.
    let other = DeviceKey::generate().nostr_public_key();
    assert!(matches!(
        CryptoDevice::open(&ana, &net, &other, &path, 1),
        Err(Error::DeviceMismatch)
    ));
}

fn nostr() -> NostrPublicKey {
    DeviceKey::generate().nostr_public_key()
}

fn key() -> PublicKey {
    DeviceKey::generate().public_key()
}
