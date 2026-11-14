//! Tombstone de device (P30 fatia 3a, ADR 0022): a declaração assinada de que um certificado de device
//! deixou de valer. É só o **formato**: assinar, verificar sem estado, serializar e dizer "este
//! tombstone revoga este certificado". Armazenar, propagar e derrubar o par MLS são as fatias 3b a 3d.
//!
//! Uma declaração por certificado, individual e cumulativa (ADR 0011): sem lista substitutiva e sem
//! "maior sequência vale", porque um replay de lista antiga des-revogaria e um `seq` forjado travaria a
//! correção. Quem assina é a **autoridade** (a cadeia viaja dentro do tombstone, como no certificado
//! v4) ou a **mestra**; `revoked_at` é informativo e nenhuma decisão de confiança o lê.

use libp2p_identity::PublicKey;
use sha2::{Digest, Sha256};

use crate::device::{put, take};
use crate::{AuthorityCertificate, DeviceCertificate, Error, IdentityId, Issuer, NostrPublicKey};

/// Domínio de assinatura do tombstone: diferente do certificado de device e do de autoridade, para uma
/// assinatura de um nunca valer como a de outro.
pub(crate) const REVOCATION_DOMAIN: &[u8] = b"";

/// Domínio do hash do certificado revogado (separado do de assinatura).
const CERT_HASH_DOMAIN: &[u8] = b"kin/device-revocation-cert-hash/v1";

/// Magic e versão do formato de fio (não entram na assinatura, que tem o seu domínio).
const MAGIC: &[u8; 3] = b"KRV";
const VERSION: u8 = 1;
/// Tipo do emissor na serialização.
const KIND_MASTER: u8 = 0;
const KIND_AUTHORITY: u8 = 1;
/// Teto do tombstone serializado (o real tem ~450 bytes com a cadeia). Barra entrada gigante antes do parse.
const MAX_LEN: usize = 2048;

/// `(len32 | campo)` acrescentado a `out`: o prefixo de tamanho impede que dois conjuntos diferentes de
/// campos produzam os mesmos bytes.
fn push(out: &mut Vec<u8>, field: &[u8]) {
    out.extend_from_slice(&(field.len() as u32).to_be_bytes());
    out.extend_from_slice(field);
}

/// Bytes que o emissor assina: domínio, emissor (no v4 a identidade, a chave de autoridade e o
/// `created_at` dela, como no certificado de device, para amarrar **qual** emissão), hash, as três
/// chaves e `revoked_at`.
fn signing_payload(
    issuer: &Issuer,
    cert_hash: &[u8; 32],
    signing_key: &PublicKey,
    network_key: &PublicKey,
    nostr_key: &NostrPublicKey,
    revoked_at: u64,
) -> Vec<u8> {
    let mut payload = REVOCATION_DOMAIN.to_vec();
    match issuer {
        Issuer::Master(identity) => {
            push(&mut payload, &[KIND_MASTER]);
            push(&mut payload, &identity.public_key().encode_protobuf());
        }
        Issuer::Authority(authority) => {
            push(&mut payload, &[KIND_AUTHORITY]);
            push(
                &mut payload,
                &authority.identity.public_key().encode_protobuf(),
            );
            push(&mut payload, &authority.authority_key.encode_protobuf());
            push(&mut payload, &authority.created_at.to_be_bytes());
        }
    }
    push(&mut payload, cert_hash);
    push(&mut payload, &signing_key.encode_protobuf());
    push(&mut payload, &network_key.encode_protobuf());
    push(&mut payload, &nostr_key.to_bytes());
    push(&mut payload, &revoked_at.to_be_bytes());
    payload
}

/// Declaração assinada de que o certificado `cert_hash` (e as chaves citadas) não vale mais.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceRevocation {
    /// Quem assinou: a mestra ou a autoridade certificada por ela. Define a identidade.
    pub issuer: Issuer,
    /// SHA-256 (com domínio) dos bytes do certificado revogado: registra **qual** certificado foi
    /// revogado. Quem decide o que é revogado são as chaves (ver [`Self::revokes`]).
    pub cert_hash: [u8; 32],
    pub signing_key: PublicKey,
    pub network_key: PublicKey,
    pub nostr_key: NostrPublicKey,
    /// Instante informativo (segundos Unix). Não é validade nem ordem.
    pub revoked_at: u64,
    pub signature: Vec<u8>,
}

impl DeviceRevocation {
    /// A identidade dona do device revogado: a da mestra ou a da cadeia de autoridade.
    pub fn identity(&self) -> &IdentityId {
        match &self.issuer {
            Issuer::Master(identity) => identity,
            Issuer::Authority(authority) => &authority.identity,
        }
    }

    /// SHA-256 com domínio dos bytes do certificado.
    pub(crate) fn certificate_hash(certificate: &DeviceCertificate) -> [u8; 32] {
        let mut input = CERT_HASH_DOMAIN.to_vec();
        push(&mut input, &certificate.to_bytes());
        let digest = Sha256::digest(&input);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(digest.as_slice());
        hash
    }

    /// Monta e assina o tombstone de `certificate`. Recusa certificado de **outra identidade** que a do
    /// emissor ([`Error::RevocationIdentityMismatch`]). `sign` assina os bytes com o domínio já imposto
    /// aqui (a chave da identidade nunca assina bytes livres, ADR 0013 S1).
    pub(crate) fn build(
        issuer: Issuer,
        certificate: &DeviceCertificate,
        revoked_at: u64,
        sign: impl FnOnce(&[u8]) -> Result<Vec<u8>, Error>,
    ) -> Result<Self, Error> {
        let identity = match &issuer {
            Issuer::Master(identity) => identity,
            Issuer::Authority(authority) => &authority.identity,
        };
        if certificate.identity() != identity {
            return Err(Error::RevocationIdentityMismatch);
        }
        let cert_hash = Self::certificate_hash(certificate);
        let payload = signing_payload(
            &issuer,
            &cert_hash,
            &certificate.signing_key,
            &certificate.network_key,
            &certificate.nostr_key,
            revoked_at,
        );
        let signature = sign(&payload)?;
        Ok(Self {
            issuer,
            cert_hash,
            signing_key: certificate.signing_key.clone(),
            network_key: certificate.network_key.clone(),
            nostr_key: certificate.nostr_key,
            revoked_at,
            signature,
        })
    }

    /// Confere a assinatura (e, no caso da autoridade, a cadeia inteira). Não diz se a identidade é a
    /// que o chamador espera, nem se o tombstone se aplica a algum certificado: isso é com quem chama.
    pub fn verify(&self) -> Result<(), Error> {
        let payload = signing_payload(
            &self.issuer,
            &self.cert_hash,
            &self.signing_key,
            &self.network_key,
            &self.nostr_key,
            self.revoked_at,
        );
        let signed = match &self.issuer {
            Issuer::Master(identity) => identity.public_key().verify(&payload, &self.signature),
            Issuer::Authority(authority) => {
                // A cadeia inteira: a mestra assinou a autoridade, e a autoridade assinou o tombstone.
                authority.verify().map_err(|_| Error::InvalidRevocation)?;
                authority.authority_key.verify(&payload, &self.signature)
            }
        };
        if signed {
            Ok(())
        } else {
            Err(Error::InvalidRevocation)
        }
    }

    /// Este tombstone revoga `certificate`? Verdadeiro se for da **mesma identidade e das mesmas três
    /// chaves** (MLS, rede e Nostr), mesmo que o certificado seja outro (reemitido com outro
    /// `created_at`). Não confere a assinatura de nenhum dos dois: use [`Self::verify`].
    pub fn revokes(&self, certificate: &DeviceCertificate) -> bool {
        certificate.identity() == self.identity()
            && certificate.signing_key == self.signing_key
            && certificate.network_key == self.network_key
            && certificate.nostr_key == self.nostr_key
    }

    /// `"KRV" | 1 | tipo u8 (0 mestra, 1 autoridade) | len16|emissor | len16|hash | len16|chave MLS |
    /// len16|chave de rede | len16|chave Nostr | revoked_at u64 BE | len16|assinatura`. O emissor é a
    /// identidade (mestra) ou o certificado de autoridade (`KAUT`), como no certificado de device.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.push(VERSION);
        match &self.issuer {
            Issuer::Master(identity) => {
                out.push(KIND_MASTER);
                put(&mut out, &identity.public_key().encode_protobuf());
            }
            Issuer::Authority(authority) => {
                out.push(KIND_AUTHORITY);
                put(&mut out, &authority.to_bytes());
            }
        }
        put(&mut out, &self.cert_hash);
        put(&mut out, &self.signing_key.encode_protobuf());
        put(&mut out, &self.network_key.encode_protobuf());
        put(&mut out, &self.nostr_key.to_bytes());
        out.extend_from_slice(&self.revoked_at.to_be_bytes());
        put(&mut out, &self.signature);
        out
    }

    /// Lê um tombstone serializado. Não confere a assinatura: use [`Self::verify`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        // Genérica: serve ao erro do `take` e ao dos decodificadores de chave, sem expor o detalhe.
        fn invalid<E>(_: E) -> Error {
            Error::InvalidRevocation
        }
        if bytes.len() > MAX_LEN {
            return Err(Error::InvalidRevocation);
        }
        let mut cur = bytes
            .strip_prefix(MAGIC.as_slice())
            .and_then(|rest| rest.strip_prefix(&[VERSION]))
            .ok_or(Error::InvalidRevocation)?;
        let (&kind, rest) = cur.split_first().ok_or(Error::InvalidRevocation)?;
        cur = rest;
        let issuer = match kind {
            KIND_MASTER => {
                let key = PublicKey::try_decode_protobuf(take(&mut cur).map_err(invalid)?)
                    .map_err(invalid)?;
                Issuer::Master(IdentityId::new(key))
            }
            KIND_AUTHORITY => {
                let authority = AuthorityCertificate::from_bytes(take(&mut cur).map_err(invalid)?)
                    .map_err(invalid)?;
                Issuer::Authority(Box::new(authority))
            }
            _ => return Err(Error::InvalidRevocation),
        };
        let cert_hash: [u8; 32] = take(&mut cur)
            .map_err(invalid)?
            .try_into()
            .map_err(invalid)?;
        let signing_key =
            PublicKey::try_decode_protobuf(take(&mut cur).map_err(invalid)?).map_err(invalid)?;
        let network_key =
            PublicKey::try_decode_protobuf(take(&mut cur).map_err(invalid)?).map_err(invalid)?;
        let nostr_key =
            NostrPublicKey::from_bytes(take(&mut cur).map_err(invalid)?).map_err(invalid)?;
        let revoked_at = u64::from_be_bytes(
            cur.get(..8)
                .ok_or(Error::InvalidRevocation)?
                .try_into()
                .map_err(invalid)?,
        );
        cur = &cur[8..];
        let signature = take(&mut cur).map_err(invalid)?.to_vec();
        if !cur.is_empty() {
            return Err(Error::InvalidRevocation);
        }
        Ok(Self {
            issuer,
            cert_hash,
            signing_key,
            network_key,
            nostr_key,
            revoked_at,
            signature,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuthorityIdentity, AuthorityKey, DeviceKey, IdentityProvider, StandaloneIdentity};

    const NOW: u64 = 1_700_000_000;

    fn authority_of(master: &StandaloneIdentity) -> AuthorityIdentity {
        let key = AuthorityKey::generate();
        let cert = master.certify_authority(&key.public_key(), 1_000).unwrap();
        AuthorityIdentity::new(key, cert).unwrap()
    }

    /// As três chaves de um device (a de assinatura MLS é uma chave Ed25519 qualquer).
    struct Dev {
        key: DeviceKey,
        signing: PublicKey,
    }

    fn dev() -> Dev {
        Dev {
            key: DeviceKey::generate(),
            signing: DeviceKey::generate().public_key(),
        }
    }

    fn cert_for(issuer: &dyn IdentityProvider, d: &Dev, created_at: u64) -> DeviceCertificate {
        issuer
            .authorize_device(
                &d.signing,
                &d.key.public_key(),
                &d.key.nostr_public_key(),
                created_at,
            )
            .unwrap()
    }

    /// Um tombstone da autoridade para um device v4 da identidade `master`.
    fn by_authority() -> (DeviceRevocation, DeviceCertificate, StandaloneIdentity) {
        let master = StandaloneIdentity::generate();
        let authority = authority_of(&master);
        let cert = cert_for(&authority, &dev(), NOW);
        let revocation = authority.revoke_device(&cert, NOW + 60).unwrap();
        (revocation, cert, master)
    }

    /// Um tombstone da mestra para um device v3.
    fn by_master() -> (DeviceRevocation, DeviceCertificate, StandaloneIdentity) {
        let master = StandaloneIdentity::generate();
        let cert = cert_for(&master, &dev(), NOW);
        let revocation = master.revoke_device(&cert, NOW + 60).unwrap();
        (revocation, cert, master)
    }

    // ---- assinar, verificar, ida e volta ----

    #[test]
    fn a_revocation_from_the_authority_verifies_and_round_trips() {
        let (revocation, cert, master) = by_authority();
        revocation.verify().unwrap();
        assert_eq!(revocation.identity(), &master.id());
        assert_eq!(revocation.revoked_at, NOW + 60);
        assert_eq!(
            revocation.cert_hash,
            DeviceRevocation::certificate_hash(&cert)
        );
        let back = DeviceRevocation::from_bytes(&revocation.to_bytes()).unwrap();
        assert_eq!(back, revocation);
        back.verify().unwrap();
    }

    #[test]
    fn a_revocation_from_the_master_verifies_and_round_trips() {
        let (revocation, _, master) = by_master();
        revocation.verify().unwrap();
        assert_eq!(revocation.identity(), &master.id());
        let back = DeviceRevocation::from_bytes(&revocation.to_bytes()).unwrap();
        assert_eq!(back, revocation);
        back.verify().unwrap();
    }

    #[test]
    fn either_issuer_can_revoke_a_device_of_either_kind() {
        // A mestra revoga um device v4 e a autoridade revoga um device v3 da mesma identidade.
        let master = StandaloneIdentity::generate();
        let authority = authority_of(&master);
        let v3 = cert_for(&master, &dev(), NOW);
        let v4 = cert_for(&authority, &dev(), NOW);
        for (revocation, cert) in [
            (master.revoke_device(&v4, NOW).unwrap(), &v4),
            (authority.revoke_device(&v3, NOW).unwrap(), &v3),
        ] {
            revocation.verify().unwrap();
            assert!(revocation.revokes(cert));
        }
    }

    #[test]
    fn revoke_device_refuses_a_certificate_of_another_identity() {
        let ours = StandaloneIdentity::generate();
        let theirs = StandaloneIdentity::generate();
        let our_authority = authority_of(&ours);
        let their_cert = cert_for(&theirs, &dev(), NOW);
        let their_v4 = cert_for(&authority_of(&theirs), &dev(), NOW);
        for cert in [&their_cert, &their_v4] {
            assert!(matches!(
                ours.revoke_device(cert, NOW),
                Err(Error::RevocationIdentityMismatch)
            ));
            assert!(matches!(
                our_authority.revoke_device(cert, NOW),
                Err(Error::RevocationIdentityMismatch)
            ));
        }
    }

    // ---- o que o tombstone revoga ----

    #[test]
    fn it_revokes_the_certificate_and_any_other_certificate_with_the_same_keys() {
        // Quem ainda tem a mestra reemite um certificado para as mesmas chaves (outro `created_at`,
        // outro hash): o tombstone tem de continuar valendo, senão não adianta nada.
        let master = StandaloneIdentity::generate();
        let d = dev();
        let original = cert_for(&master, &d, NOW);
        let reissued = cert_for(&master, &d, NOW + 999);
        let revocation = master.revoke_device(&original, NOW + 1).unwrap();

        assert!(revocation.revokes(&original));
        assert!(revocation.revokes(&reissued));
        assert_ne!(
            DeviceRevocation::certificate_hash(&original),
            DeviceRevocation::certificate_hash(&reissued)
        );
        // o hash registra qual foi revogado: o do original
        assert_eq!(
            revocation.cert_hash,
            DeviceRevocation::certificate_hash(&original)
        );
    }

    #[test]
    fn it_does_not_revoke_another_device_of_the_same_identity() {
        let master = StandaloneIdentity::generate();
        let d = dev();
        let revocation = master
            .revoke_device(&cert_for(&master, &d, NOW), NOW)
            .unwrap();

        // Cada chave sozinha diferente basta para não ser o mesmo device.
        let other = dev();
        let only_signing = Dev {
            key: DeviceKey::from_bytes(&d.key.to_bytes().unwrap()).unwrap(),
            signing: other.signing.clone(),
        };
        let only_network_and_nostr = Dev {
            key: DeviceKey::generate(),
            signing: d.signing.clone(),
        };
        for different in [&only_signing, &only_network_and_nostr, &other] {
            assert!(!revocation.revokes(&cert_for(&master, different, NOW)));
        }
        // E cada uma das três chaves sozinha (as outras duas iguais): nenhuma pode ser ignorada.
        let (net, nostr) = (other.key.public_key(), other.key.nostr_public_key());
        let issue = |s: &PublicKey, n: &PublicKey, x: &NostrPublicKey| {
            master.authorize_device(s, n, x, NOW).unwrap()
        };
        let (s0, n0, x0) = (&d.signing, &d.key.public_key(), &d.key.nostr_public_key());
        assert!(revocation.revokes(&issue(s0, n0, x0)), "controle positivo");
        assert!(
            !revocation.revokes(&issue(&other.signing, n0, x0)),
            "só MLS"
        );
        assert!(!revocation.revokes(&issue(s0, &net, x0)), "só rede");
        assert!(!revocation.revokes(&issue(s0, n0, &nostr)), "só Nostr");
    }

    #[test]
    fn it_does_not_revoke_the_same_keys_under_another_identity() {
        let (revocation, cert, _) = by_master();
        let intruder = StandaloneIdentity::generate();
        let same_keys = intruder
            .authorize_device(
                &cert.signing_key,
                &cert.network_key,
                &cert.nostr_key,
                cert.created_at,
            )
            .unwrap();
        assert!(!revocation.revokes(&same_keys));
        let same_keys_v4 = authority_of(&intruder)
            .authorize_device(
                &cert.signing_key,
                &cert.network_key,
                &cert.nostr_key,
                cert.created_at,
            )
            .unwrap();
        assert!(!revocation.revokes(&same_keys_v4));
    }

    #[test]
    fn revoked_at_is_informational_it_never_changes_what_is_revoked() {
        let master = StandaloneIdentity::generate();
        let cert = cert_for(&master, &dev(), NOW);
        for at in [0, 1, NOW, u64::MAX] {
            let revocation = master.revoke_device(&cert, at).unwrap();
            revocation.verify().unwrap();
            assert!(revocation.revokes(&cert));
        }
    }

    // ---- adulterar campo por campo ----

    /// Cada campo público alterado faz o `verify` falhar. `variants` devolve o tombstone com um campo
    /// trocado, com o nome do campo para a mensagem.
    fn tampered(revocation: &DeviceRevocation) -> Vec<(&'static str, DeviceRevocation)> {
        let other = dev();
        let mut cases = Vec::new();
        let mut with = |name: &'static str, change: &dyn Fn(&mut DeviceRevocation)| {
            let mut copy = revocation.clone();
            change(&mut copy);
            cases.push((name, copy));
        };
        with("cert_hash", &|r| r.cert_hash[0] ^= 1);
        with("signing_key", &|r| r.signing_key = other.signing.clone());
        with("network_key", &|r| r.network_key = other.key.public_key());
        with("nostr_key", &|r| r.nostr_key = other.key.nostr_public_key());
        with("revoked_at", &|r| r.revoked_at += 1);
        with("signature", &|r| r.signature[0] ^= 1);
        with("issuer.identity", &|r| match &mut r.issuer {
            Issuer::Master(identity) => {
                *identity = StandaloneIdentity::generate().id();
            }
            Issuer::Authority(authority) => {
                authority.identity = StandaloneIdentity::generate().id();
            }
        });
        with("issuer.authority_key", &|r| {
            if let Issuer::Authority(authority) = &mut r.issuer {
                authority.authority_key = AuthorityKey::generate().public_key();
            }
        });
        with("issuer.authority.created_at", &|r| {
            if let Issuer::Authority(authority) = &mut r.issuer {
                authority.created_at += 1;
            }
        });
        with("issuer.authority.signature", &|r| {
            if let Issuer::Authority(authority) = &mut r.issuer {
                authority.signature[0] ^= 1;
            }
        });
        cases
    }

    #[test]
    fn tampering_any_field_of_an_authority_revocation_breaks_it() {
        let (revocation, _, _) = by_authority();
        let mut changed = 0;
        for (field, copy) in tampered(&revocation) {
            if copy != revocation {
                changed += 1;
                assert!(copy.verify().is_err(), "aceitou {field} adulterado");
            }
        }
        assert_eq!(changed, 10, "todos os campos deviam ter mudado");
    }

    #[test]
    fn tampering_any_field_of_a_master_revocation_breaks_it() {
        let (revocation, _, _) = by_master();
        let mut changed = 0;
        for (field, copy) in tampered(&revocation) {
            if copy != revocation {
                changed += 1;
                assert!(copy.verify().is_err(), "aceitou {field} adulterado");
            }
        }
        // `authority_key`, `authority.created_at` e `authority.signature` não existem no emissor mestra.
        assert_eq!(changed, 7);
    }

    #[test]
    fn flipping_any_bit_of_the_wire_form_is_never_accepted() {
        for (revocation, _, _) in [by_authority(), by_master()] {
            let bytes = revocation.to_bytes();
            for index in 0..bytes.len() {
                for mask in [0x01u8, 0x80] {
                    let mut flipped = bytes.clone();
                    flipped[index] ^= mask;
                    let accepted = DeviceRevocation::from_bytes(&flipped)
                        .map(|parsed| parsed.verify().is_ok())
                        .unwrap_or(false);
                    assert!(!accepted, "aceitou o byte {index} com a máscara {mask:#x}");
                }
            }
        }
    }

    // ---- separação de domínio ----

    #[test]
    fn a_revocation_signature_is_not_a_device_certificate_and_vice_versa() {
        for (revocation, cert, _) in [by_authority(), by_master()] {
            // A assinatura do tombstone posta num certificado das mesmas chaves não verifica.
            let as_certificate = DeviceCertificate {
                issuer: revocation.issuer.clone(),
                signing_key: revocation.signing_key.clone(),
                network_key: revocation.network_key.clone(),
                nostr_key: revocation.nostr_key,
                created_at: revocation.revoked_at,
                signature: revocation.signature.clone(),
            };
            assert!(as_certificate.verify().is_err());
            // E a assinatura do certificado posta num tombstone do mesmo certificado não verifica.
            let mut as_revocation = revocation.clone();
            as_revocation.signature = cert.signature.clone();
            assert!(as_revocation.verify().is_err());
        }
    }

    // ---- parse ----

    #[test]
    fn parsing_refuses_garbage_truncation_trailing_bytes_and_oversized_input() {
        let (revocation, cert, _) = by_authority();
        let bytes = revocation.to_bytes();
        assert!(DeviceRevocation::from_bytes(&[]).is_err());
        assert!(DeviceRevocation::from_bytes(b"lixo").is_err());
        for n in 0..bytes.len() {
            assert!(
                DeviceRevocation::from_bytes(&bytes[..n]).is_err(),
                "aceitou truncado em {n}"
            );
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(DeviceRevocation::from_bytes(&trailing).is_err());
        assert!(DeviceRevocation::from_bytes(&vec![0u8; 1 << 20]).is_err());
        let mut wrong_magic = bytes.clone();
        wrong_magic[0] = b'X';
        assert!(DeviceRevocation::from_bytes(&wrong_magic).is_err());
        let mut wrong_version = bytes.clone();
        wrong_version[3] = 2;
        assert!(DeviceRevocation::from_bytes(&wrong_version).is_err());
        // um certificado de device não é um tombstone, nem o contrário
        assert!(DeviceRevocation::from_bytes(&cert.to_bytes()).is_err());
        assert!(DeviceCertificate::from_bytes(&bytes).is_err());
    }

    #[test]
    fn the_serialized_form_stays_small_enough_for_a_message() {
        for (revocation, _, _) in [by_authority(), by_master()] {
            assert!(revocation.to_bytes().len() < 1024);
        }
    }
}
