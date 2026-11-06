use kin_identity::{DeviceCertificate, IdentityId, PeerId, ed25519_bytes};
use openmls::prelude::{BasicCredential, Credential};

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
/// (3) identidade e Peer ID são os esperados, nos campos que o chamador informou.
pub(crate) fn verify(
    credential: &Credential,
    signature_key: &[u8],
    expected: &Expected,
) -> Result<Verified, Error> {
    let cert = parse(credential)?;
    if ed25519_bytes(&cert.signing_key)? != signature_key {
        return Err(Error::InvalidCredential);
    }
    let who = Verified {
        peer: cert.network_peer_id(),
        identity: cert.identity,
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
    Ok(parse(credential)?.identity)
}

/// Lê o Peer ID de rede certificado numa credencial já validada pelo MLS.
pub(crate) fn peer_of(credential: &Credential) -> Result<PeerId, Error> {
    Ok(parse(credential)?.network_peer_id())
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

        let who = verify(&credential_for(&cert), &raw, &ANY).unwrap();
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
        assert!(verify(&cred, &raw, &right).is_ok());
        // Mesmo certificado válido, apresentado por outro Peer ID (o spoof do P15), é recusado.
        let other = DeviceKey::generate().peer_id();
        let wrong = Expected {
            identity: None,
            peer: Some(&other),
        };
        assert!(matches!(
            verify(&cred, &raw, &wrong),
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
            verify(&credential_for(&cert), &other, &ANY),
            Err(Error::InvalidCredential)
        ));
    }

    #[test]
    fn rejects_a_forged_or_garbage_credential() {
        let id = StandaloneIdentity::generate();
        let key = DeviceKey::generate().public_key();
        let mut cert = issue(&id, &key, &DeviceKey::generate().public_key());
        // Alguém troca a identidade por outra, mantendo a assinatura original.
        cert.identity = StandaloneIdentity::generate().id();
        let raw = ed25519_bytes(&key).unwrap();

        assert!(matches!(
            verify(&credential_for(&cert), &raw, &ANY),
            Err(Error::InvalidCredential)
        ));
        let garbage: Credential = BasicCredential::new(b"lixo".to_vec()).into();
        assert!(matches!(
            verify(&garbage, &raw, &ANY),
            Err(Error::InvalidCredential)
        ));
    }
}
