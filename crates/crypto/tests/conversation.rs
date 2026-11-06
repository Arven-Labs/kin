use kin_crypto::{Conversation, CryptoDevice, Decrypted, Error, Expected};
use kin_identity::{DeviceKey, IdentityId, IdentityProvider, PeerId, StandaloneIdentity};

struct Person {
    identity: StandaloneIdentity,
    id: IdentityId,
    peer: PeerId,
    device: CryptoDevice,
}

fn person() -> Person {
    let identity = StandaloneIdentity::generate();
    let network = DeviceKey::generate();
    let device = CryptoDevice::new(
        &identity,
        &network.public_key(),
        &network.nostr_public_key(),
        1_000,
    )
    .unwrap();
    Person {
        id: identity.id(),
        peer: network.peer_id(),
        identity,
        device,
    }
}

/// Ana convida Bia e as duas passam a ter a mesma conversa.
fn pair() -> (Person, Person, Conversation, Conversation) {
    let (ana, bia) = (person(), person());
    let mut conv_ana = Conversation::create(&ana.device).unwrap();
    let invite = conv_ana
        .invite(
            &bia.device.key_package().unwrap(),
            &Expected {
                identity: Some(&bia.id),
                peer: Some(&bia.peer),
            },
        )
        .unwrap();
    let conv_bia = Conversation::join(
        &bia.device,
        &invite.welcome,
        &Expected {
            identity: Some(&ana.id),
            peer: Some(&ana.peer),
        },
    )
    .unwrap();
    (ana, bia, conv_ana, conv_bia)
}

fn identity(p: &Person) -> Expected<'_> {
    Expected {
        identity: Some(&p.id),
        peer: None,
    }
}

fn from(p: &Person) -> Expected<'_> {
    Expected {
        identity: None,
        peer: Some(&p.peer),
    }
}

fn text(d: Decrypted) -> Vec<u8> {
    match d {
        Decrypted::Message { plaintext, .. } => plaintext,
        other => panic!("esperava mensagem, veio {other:?}"),
    }
}

#[test]
fn messages_flow_both_ways_and_carry_the_sender_identity() {
    let (ana, bia, mut c_ana, mut c_bia) = pair();
    assert_eq!(c_ana.id(), c_bia.id());
    assert_eq!(c_ana.epoch(), c_bia.epoch());
    assert_eq!(c_ana.peers().unwrap(), vec![bia.identity.id()]);
    assert_eq!(c_bia.peers().unwrap(), vec![ana.identity.id()]);
    assert_eq!(c_ana.peer_ids().unwrap(), vec![bia.peer]);
    assert_eq!(c_bia.peer_ids().unwrap(), vec![ana.peer]);

    let wire = c_ana.encrypt(b"oi bia").unwrap();
    assert!(
        !wire.windows(6).any(|w| w == b"oi bia"),
        "texto claro vazou"
    );
    match c_bia.decrypt(&wire).unwrap() {
        Decrypted::Message { sender, plaintext } => {
            assert_eq!(sender, ana.identity.id());
            assert_eq!(plaintext, b"oi bia");
        }
        other => panic!("{other:?}"),
    }

    let reply = c_bia.encrypt(b"oi ana").unwrap();
    assert_eq!(text(c_ana.decrypt(&reply).unwrap()), b"oi ana");
}

#[test]
fn tampered_ciphertext_is_rejected() {
    let (_, _, mut c_ana, mut c_bia) = pair();
    let mut wire = c_ana.encrypt(b"segredo").unwrap();
    let last = wire.len() - 1;
    wire[last] ^= 0xff;
    assert!(c_bia.decrypt(&wire).is_err());
    assert!(matches!(c_bia.decrypt(b"lixo"), Err(Error::Malformed)));
}

#[test]
fn redelivery_is_reported_as_duplicate_and_does_not_break_the_conversation() {
    let (_, _, mut c_ana, mut c_bia) = pair();
    let wire = c_ana.encrypt(b"uma vez so").unwrap();
    assert_eq!(text(c_bia.decrypt(&wire).unwrap()), b"uma vez so");
    assert!(matches!(c_bia.decrypt(&wire), Err(Error::Duplicate)));

    let next = c_ana.encrypt(b"seguinte").unwrap();
    assert_eq!(text(c_bia.decrypt(&next).unwrap()), b"seguinte");
}

#[test]
fn out_of_order_delivery_within_tolerance_still_decrypts() {
    let (_, _, mut c_ana, mut c_bia) = pair();
    let m1 = c_ana.encrypt(b"1").unwrap();
    let m2 = c_ana.encrypt(b"2").unwrap();
    let m3 = c_ana.encrypt(b"3").unwrap();
    assert_eq!(text(c_bia.decrypt(&m3).unwrap()), b"3");
    assert_eq!(text(c_bia.decrypt(&m1).unwrap()), b"1");
    assert_eq!(text(c_bia.decrypt(&m2).unwrap()), b"2");
}

#[test]
fn message_from_another_conversation_is_rejected() {
    let (_, _, _, mut c_bia) = pair();
    let (_, _, mut other_ana, _) = pair();
    let wire = other_ana.encrypt(b"nao e pra voce").unwrap();
    assert!(matches!(
        c_bia.decrypt(&wire),
        Err(Error::WrongConversation)
    ));
}

#[test]
fn key_rotation_advances_the_epoch_on_both_sides() {
    let (_, _, mut c_ana, mut c_bia) = pair();
    let before = c_ana.epoch();

    let commit = c_ana.rotate_keys().unwrap();
    assert_eq!(c_ana.epoch(), before + 1);
    assert_eq!(c_bia.decrypt(&commit).unwrap(), Decrypted::EpochChanged);
    assert_eq!(c_bia.epoch(), c_ana.epoch());

    let wire = c_bia.encrypt(b"depois da rotacao").unwrap();
    assert_eq!(text(c_ana.decrypt(&wire).unwrap()), b"depois da rotacao");
}

#[test]
fn late_message_from_the_previous_epoch_still_decrypts() {
    let (_, _, mut c_ana, mut c_bia) = pair();
    let late = c_ana.encrypt(b"enviada antes do commit").unwrap();
    let commit = c_ana.rotate_keys().unwrap();

    // O commit chega antes da mensagem (sem ordem garantida).
    c_bia.decrypt(&commit).unwrap();
    assert_eq!(
        text(c_bia.decrypt(&late).unwrap()),
        b"enviada antes do commit"
    );
}

#[test]
fn message_from_a_future_epoch_is_reported_until_the_commit_arrives() {
    let (_, _, mut c_ana, mut c_bia) = pair();
    let commit = c_ana.rotate_keys().unwrap();
    let early = c_ana.encrypt(b"nova epoca").unwrap();

    assert!(matches!(c_bia.decrypt(&early), Err(Error::UnknownEpoch)));
    c_bia.decrypt(&commit).unwrap();
    assert_eq!(text(c_bia.decrypt(&early).unwrap()), b"nova epoca");
}

#[test]
fn only_the_creator_of_the_conversation_is_the_committer() {
    let (_, _, c_ana, c_bia) = pair();
    assert!(c_ana.is_committer());
    assert!(!c_bia.is_committer());
}

#[test]
fn a_non_committer_cannot_issue_commits() {
    let (_, _, c_ana, mut c_bia) = pair();
    let before = c_bia.epoch();
    assert!(matches!(c_bia.rotate_keys(), Err(Error::NotCommitter)));
    assert!(matches!(c_bia.commit_pending(), Err(Error::NotCommitter)));
    assert_eq!(c_bia.epoch(), before);
    assert_eq!(c_ana.epoch(), before);
}

#[test]
fn a_non_committer_rotates_its_key_through_a_proposal_the_committer_commits() {
    let (_, _, mut c_ana, mut c_bia) = pair();
    let before = c_ana.epoch();

    let proposal = c_bia.propose_rotation().unwrap();
    assert_eq!(c_ana.decrypt(&proposal).unwrap(), Decrypted::ProposalQueued);
    assert_eq!(c_ana.epoch(), before, "proposta sozinha não muda a época");

    let commit = c_ana.commit_pending().unwrap();
    assert_eq!(c_bia.decrypt(&commit).unwrap(), Decrypted::EpochChanged);
    assert_eq!((c_ana.epoch(), c_bia.epoch()), (before + 1, before + 1));

    let wire = c_bia.encrypt(b"depois da proposta").unwrap();
    assert_eq!(text(c_ana.decrypt(&wire).unwrap()), b"depois da proposta");
    let wire = c_ana.encrypt(b"e de volta").unwrap();
    assert_eq!(text(c_bia.decrypt(&wire).unwrap()), b"e de volta");
}

#[test]
fn a_proposal_delivered_twice_still_yields_a_single_epoch_step() {
    let (_, _, mut c_ana, mut c_bia) = pair();
    let before = c_ana.epoch();

    let proposal = c_bia.propose_rotation().unwrap();
    c_ana.decrypt(&proposal).unwrap();
    // Reentrega (at-least-once): ou é ignorada ou vira Duplicate, mas não pode duplicar a proposta.
    let _ = c_ana.decrypt(&proposal);

    let commit = c_ana.commit_pending().unwrap();
    c_bia.decrypt(&commit).unwrap();
    assert_eq!((c_ana.epoch(), c_bia.epoch()), (before + 1, before + 1));
}

#[test]
fn welcome_is_useless_to_a_device_it_was_not_made_for() {
    let (ana, bia) = (person(), person());
    let intruder = person();
    let mut conv = Conversation::create(&ana.device).unwrap();
    let invite = conv
        .invite(&bia.device.key_package().unwrap(), &Expected::default())
        .unwrap();

    assert!(matches!(
        Conversation::join(&intruder.device, &invite.welcome, &Expected::default()),
        Err(Error::NotForThisDevice)
    ));
}

#[test]
fn identity_expectations_are_enforced_on_invite_and_join() {
    let (ana, bia) = (person(), person());
    let stranger = person();
    let mut conv = Conversation::create(&ana.device).unwrap();

    let kp = bia.device.key_package().unwrap();
    assert!(matches!(
        conv.invite(&kp, &identity(&stranger)),
        Err(Error::UnexpectedIdentity)
    ));
    assert!(matches!(
        conv.invite(b"lixo", &Expected::default()),
        Err(Error::Malformed)
    ));

    let invite = conv.invite(&kp, &identity(&bia)).unwrap();
    assert!(matches!(
        Conversation::join(&bia.device, &invite.welcome, &identity(&stranger)),
        Err(Error::UnexpectedIdentity)
    ));
}

/// P15: um KeyPackage legítimo de Bia apresentado por outro Peer ID (um peer M repassando o pacote
/// dela) é recusado, e o mesmo vale para o Welcome na volta.
#[test]
fn network_peer_expectations_are_enforced_on_invite_and_join() {
    let (ana, bia) = (person(), person());
    let relay = person(); // o peer que, na rede, entregou o pacote da Bia
    let mut conv = Conversation::create(&ana.device).unwrap();

    let kp = bia.device.key_package().unwrap();
    assert!(matches!(
        conv.invite(&kp, &from(&relay)),
        Err(Error::UnexpectedPeer)
    ));
    // Recusar não pode ter alterado a conversa: o convite certo ainda funciona.
    assert_eq!(conv.epoch(), 0);
    let invite = conv.invite(&kp, &from(&bia)).unwrap();
    Conversation::join(&bia.device, &invite.welcome, &from(&ana)).unwrap();

    // O openmls consome o KeyPackage ao ler o Welcome, então cada tentativa usa um convite novo.
    let welcome = || {
        let mut conv = Conversation::create(&ana.device).unwrap();
        let kp = bia.device.key_package().unwrap();
        conv.invite(&kp, &Expected::default()).unwrap().welcome
    };
    // Welcome entregue por outro Peer ID (ou "convidante" que seria a própria Bia): recusado.
    assert!(matches!(
        Conversation::join(&bia.device, &welcome(), &from(&relay)),
        Err(Error::UnexpectedPeer)
    ));
    assert!(matches!(
        Conversation::join(&bia.device, &welcome(), &from(&bia)),
        Err(Error::UnexpectedPeer)
    ));
    // Identidade certa mas Peer ID errado também.
    let mixed = Expected {
        identity: Some(&ana.id),
        peer: Some(&relay.peer),
    };
    assert!(matches!(
        Conversation::join(&bia.device, &welcome(), &mixed),
        Err(Error::UnexpectedPeer)
    ));
}
