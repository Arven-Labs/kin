use kin_identity::{DeviceCertificate, IdentityId, NostrPublicKey, PeerId, ed25519_bytes};
use openmls::prelude::{BasicCredential, Credential};

use crate::provider::Provider;
use crate::{Error, Expected};

/// Quem uma credencial MLS diz ser, já com a assinatura do certificado conferida.
pub(crate) struct Verified {
    pub(crate) identity: IdentityId,
    pub(crate) peer: PeerId,
}

impl Expected<'_> {
    /// O membro satisfaz todas as exigências informadas?
    pub(crate) fn accepts(&self, who: &Verified) -> bool {
        self.identity.is_none_or(|id| id == &who.identity)
            && self.peer.is_none_or(|peer| peer == &who.peer)
    }
}

/// Confere a credencial MLS e devolve a identidade e o Peer ID de rede a quem ela pertence.
///
/// A credencial carrega um `DeviceCertificate`. Aceitamos se (1) a assinatura do certificado vale,
/// (2) o certificado cobre exatamente a chave de assinatura MLS do membro (`signature_key`) e
/// (3) o device do certificado não tem tombstone em `provider` e (4) identidade e Peer ID são os
/// esperados, nos campos que o chamador informou.
pub(crate) fn verify(
    credential: &Credential,
    signature_key: &[u8],
    expected: &Expected,
    provider: &Provider,
) -> Result<Verified, Error> {
    let cert = parse(credential)?;
    if ed25519_bytes(&cert.signing_key)? != signature_key {
        return Err(Error::InvalidCredential);
    }
    // Ponto único da consulta (ADR 0023 I2): só depois de `parse` ter conferido a assinatura do
    // certificado, porque `is_revoked` assume um certificado verificado. Falha fechada: erro de
    // leitura do store conta como revogado.
    if provider.is_revoked_or_unknown(&cert) {
        return Err(Error::Revoked);
    }
    let who = Verified {
        peer: cert.network_peer_id(),
        identity: cert.identity().clone(),
    };
    if expected.identity.is_some_and(|id| id != &who.identity) {
        return Err(Error::UnexpectedIdentity);
    }
    if expected.peer.is_some_and(|peer| peer != &who.peer) {
        return Err(Error::UnexpectedPeer);
    }
    Ok(who)
}

/// Lê a identidade de uma credencial já validada pelo MLS (sem rechecar a chave do membro).
pub(crate) fn identity_of(credential: &Credential) -> Result<IdentityId, Error> {
    Ok(parse(credential)?.identity().clone())
}

/// Lê o Peer ID de rede certificado numa credencial já validada pelo MLS.
pub(crate) fn peer_of(credential: &Credential) -> Result<PeerId, Error> {
    Ok(parse(credential)?.network_peer_id())
}

/// Lê a chave Nostr certificada numa credencial já validada pelo MLS.
pub(crate) fn nostr_of(credential: &Credential) -> Result<NostrPublicKey, Error> {
    Ok(parse(credential)?.nostr_key)
}

fn parse(credential: &Credential) -> Result<DeviceCertificate, Error> {
    let basic =
        BasicCredential::try_from(credential.clone()).map_err(|_| Error::InvalidCredential)?;
    let cert =
        DeviceCertificate::from_bytes(basic.identity()).map_err(|_| Error::InvalidCredential)?;
    cert.verify().map_err(|_| Error::InvalidCredential)?;
    Ok(cert)
}

#[cfg(test)]
mod tests {
    use kin_identity::{DeviceKey, IdentityProvider, PublicKey, StandaloneIdentity};

    use super::*;

    fn credential_for(cert: &DeviceCertificate) -> Credential {
        BasicCredential::new(cert.to_bytes()).into()
    }

    fn issue(
        id: &StandaloneIdentity,
        signing: &PublicKey,
        network: &PublicKey,
    ) -> DeviceCertificate {
        let nostr = DeviceKey::generate().nostr_public_key();
        id.authorize_device(signing, network, &nostr, 1).unwrap()
    }

    const ANY: Expected = Expected {
        identity: None,
        peer: None,
    };

    #[test]
    fn accepts_a_valid_certificate_that_covers_the_signing_key() {
        let id = StandaloneIdentity::generate();
        let (key, net) = (DeviceKey::generate(), DeviceKey::generate());
        let cert = issue(&id, &key.public_key(), &net.public_key());
        let raw = ed25519_bytes(&key.public_key()).unwrap();

        let who = verify(&credential_for(&cert), &raw, &ANY, &fresh()).unwrap();
        assert_eq!((who.identity, who.peer), (id.id(), net.peer_id()));
    }

    #[test]
    fn enforces_the_expected_network_peer() {
        let id = StandaloneIdentity::generate();
        let (key, net) = (DeviceKey::generate(), DeviceKey::generate());
        let cert = issue(&id, &key.public_key(), &net.public_key());
        let raw = ed25519_bytes(&key.public_key()).unwrap();
        let (cred, own_id) = (credential_for(&cert), id.id());

        let right = Expected {
            identity: Some(&own_id),
            peer: Some(&net.peer_id()),
        };
        assert!(verify(&cred, &raw, &right, &fresh()).is_ok());
        // Mesmo certificado válido, apresentado por outro Peer ID (o spoof do P15), é recusado.
        let other = DeviceKey::generate().peer_id();
        let wrong = Expected {
            identity: None,
            peer: Some(&other),
        };
        assert!(matches!(
            verify(&cred, &raw, &wrong, &fresh()),
            Err(Error::UnexpectedPeer)
        ));
    }

    #[test]
    fn rejects_a_certificate_that_covers_a_different_key() {
        let id = StandaloneIdentity::generate();
        let certified = DeviceKey::generate().public_key();
        let cert = issue(&id, &certified, &DeviceKey::generate().public_key());
        let other = ed25519_bytes(&DeviceKey::generate().public_key()).unwrap();

        assert!(matches!(
            verify(&credential_for(&cert), &other, &ANY, &fresh()),
            Err(Error::InvalidCredential)
        ));
    }

    #[test]
    fn rejects_a_forged_or_garbage_credential() {
        let id = StandaloneIdentity::generate();
        let key = DeviceKey::generate().public_key();
        let mut cert = issue(&id, &key, &DeviceKey::generate().public_key());
        // Alguém troca a identidade por outra, mantendo a assinatura original.
        cert.issuer = kin_identity::Issuer::Master(StandaloneIdentity::generate().id());
        let raw = ed25519_bytes(&key).unwrap();

        assert!(matches!(
            verify(&credential_for(&cert), &raw, &ANY, &fresh()),
            Err(Error::InvalidCredential)
        ));
        let garbage: Credential = BasicCredential::new(b"lixo".to_vec()).into();
        assert!(matches!(
            verify(&garbage, &raw, &ANY, &fresh()),
            Err(Error::InvalidCredential)
        ));
    }

    // ---- revogação (P30 3d-i) ----

    /// Banco em memória, sem nenhum tombstone.
    fn fresh() -> Provider {
        Provider::open(None).unwrap()
    }

    /// Um certificado da `id`, com as chaves de signing/rede/Nostr, e o signing em bytes crus.
    fn device_of(id: &StandaloneIdentity) -> (DeviceCertificate, [u8; 32]) {
        let (key, net) = (DeviceKey::generate(), DeviceKey::generate());
        let cert = issue(id, &key.public_key(), &net.public_key());
        let raw = ed25519_bytes(&key.public_key()).unwrap();
        (cert, raw)
    }

    #[test]
    fn rejects_a_device_with_a_stored_tombstone() {
        let id = StandaloneIdentity::generate();
        let (cert, raw) = device_of(&id);
        let provider = fresh();
        provider
            .store_revocation(&id.revoke_device(&cert, 2).unwrap())
            .unwrap();

        assert!(matches!(
            verify(&credential_for(&cert), &raw, &ANY, &provider),
            Err(Error::Revoked)
        ));
    }

    /// O tombstone é de UM device: outro device da mesma identidade segue aceito.
    #[test]
    fn a_tombstone_does_not_reject_another_device_of_the_same_identity() {
        let id = StandaloneIdentity::generate();
        let (revoked, _) = device_of(&id);
        let (other, raw) = device_of(&id);
        let provider = fresh();
        provider
            .store_revocation(&id.revoke_device(&revoked, 2).unwrap())
            .unwrap();

        assert!(verify(&credential_for(&other), &raw, &ANY, &provider).is_ok());
    }

    /// Ordem (auditoria I2 da 3b): a consulta vem DEPOIS da verificação do certificado. Um certificado
    /// com as chaves de um device revogado e a assinatura adulterada tem de ser `InvalidCredential`;
    /// consultar antes de verificar devolveria `Revoked` (e, com chaves sem tombstone, deixaria passar
    /// a consulta sem checar a assinatura).
    #[test]
    fn verifies_the_certificate_before_asking_about_revocation() {
        let id = StandaloneIdentity::generate();
        let (cert, raw) = device_of(&id);
        let provider = fresh();
        provider
            .store_revocation(&id.revoke_device(&cert, 2).unwrap())
            .unwrap();
        let mut forged = cert.clone();
        forged.signature[0] ^= 0xFF;

        assert!(matches!(
            verify(&credential_for(&forged), &raw, &ANY, &provider),
            Err(Error::InvalidCredential)
        ));
    }

    /// Falha fechada: se o store não pode ser lido, o device conta como revogado.
    #[test]
    fn rejects_when_the_revocation_store_cannot_be_read() {
        let id = StandaloneIdentity::generate();
        let (cert, raw) = device_of(&id);
        let provider = fresh();
        provider.break_revocation_store_for_test();

        assert!(matches!(
            verify(&credential_for(&cert), &raw, &ANY, &provider),
            Err(Error::Revoked)
        ));
    }
}
