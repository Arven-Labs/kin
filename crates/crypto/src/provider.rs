use std::path::Path;
use std::rc::Rc;

use kin_identity::{
    DeviceCertificate, DeviceRevocation, IdentityId, NostrPublicKey, PublicKey, ed25519_bytes,
};
use openmls_rust_crypto::RustCrypto;
use openmls_sqlite_storage::{Codec, Connection, SqliteStorageProvider};
use openmls_traits::OpenMlsProvider;
use rusqlite::{OptionalExtension as _, params};
use serde::Serialize;

use crate::Error;

/// Codec do storage MLS: JSON. Simples e estável; o custo de tamanho não importa neste volume.
#[derive(Default)]
pub(crate) struct JsonCodec;

impl Codec for JsonCodec {
    type Error = serde_json::Error;

    fn to_vec<T: Serialize>(value: &T) -> Result<Vec<u8>, Self::Error> {
        serde_json::to_vec(value)
    }

    fn from_slice<T: serde::de::DeserializeOwned>(slice: &[u8]) -> Result<T, Self::Error> {
        serde_json::from_slice(slice)
    }
}

/// Provider do `openmls`: criptografia em Rust + estado MLS no SQLite. Os metadados do Kin
/// (chave do device, conversas conhecidas) ficam no mesmo banco, em tabelas próprias.
pub(crate) struct Provider {
    crypto: RustCrypto,
    storage: SqliteStorageProvider<JsonCodec, Rc<Connection>>,
    conn: Rc<Connection>,
}

impl Provider {
    /// Abre (ou cria) o banco em `path`; sem caminho, usa memória (nada sobrevive ao processo).
    pub(crate) fn open(path: Option<&Path>) -> Result<Self, Error> {
        let mut conn = match path {
            Some(path) => Connection::open(path),
            None => Connection::open_in_memory(),
        }
        .map_err(Error::storage)?;
        if let Some(path) = path {
            // Só o dono lê o estado MLS (sem cifra em repouso, P13); -wal e -shm herdam o modo.
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(Error::storage)?;
        }
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS kin_meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS kin_conversations (
                 label TEXT PRIMARY KEY, group_id BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS kin_revocations (
                 identity TEXT NOT NULL, signing_key BLOB NOT NULL,
                 network_key BLOB NOT NULL, nostr_key BLOB NOT NULL, tombstone BLOB NOT NULL,
                 PRIMARY KEY (identity, signing_key, network_key, nostr_key));",
        )
        .map_err(Error::storage)?;
        SqliteStorageProvider::<JsonCodec, &mut Connection>::new(&mut conn)
            .run_migrations()
            .map_err(Error::storage)?;
        let conn = Rc::new(conn);
        Ok(Self {
            crypto: RustCrypto::default(),
            storage: SqliteStorageProvider::new(conn.clone()),
            conn,
        })
    }

    pub(crate) fn meta(&self, key: &str) -> Result<Option<Vec<u8>>, Error> {
        self.conn
            .query_row("SELECT value FROM kin_meta WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()
            .map_err(Error::storage)
    }

    pub(crate) fn set_meta(&self, key: &str, value: &[u8]) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO kin_meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .map_err(Error::storage)?;
        Ok(())
    }

    pub(crate) fn delete_meta(&self, key: &str) -> Result<(), Error> {
        self.conn
            .execute("DELETE FROM kin_meta WHERE key = ?1", [key])
            .map_err(Error::storage)?;
        Ok(())
    }

    /// Grava um tombstone de device (P30, fatia 3b, ADR 0023). Confere a assinatura (`verify`) e usa a
    /// identidade da **cadeia assinada**, nunca um parâmetro do chamador. Idempotente, cumulativo e **sem
    /// despejo**: não passa por teto nenhum.
    pub(crate) fn store_revocation(&self, revocation: &DeviceRevocation) -> Result<(), Error> {
        revocation.verify()?;
        let key = RevocationKey::new(
            revocation.identity(),
            &revocation.signing_key,
            &revocation.network_key,
            &revocation.nostr_key,
        )?;
        self.conn
            .execute(
                "INSERT INTO kin_revocations
                     (identity, signing_key, network_key, nostr_key, tombstone)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (identity, signing_key, network_key, nostr_key) DO NOTHING",
                params![
                    key.identity,
                    key.signing.as_slice(),
                    key.network.as_slice(),
                    key.nostr.as_slice(),
                    revocation.to_bytes()
                ],
            )
            .map_err(Error::storage)?;
        Ok(())
    }

    /// O device deste certificado está revogado? Chave `(identidade, 3 chaves)`; erro de leitura é erro.
    ///
    /// **Pré-condição:** o certificado já passou por `DeviceCertificate::verify` (em `credential::verify`).
    /// Um certificado não verificado traz identidade e cadeia à escolha do atacante, e a consulta
    /// devolveria "não revogado" para um device revogado (auditoria da 3b, I2).
    pub(crate) fn is_revoked(&self, certificate: &DeviceCertificate) -> Result<bool, Error> {
        let key = RevocationKey::new(
            certificate.identity(),
            &certificate.signing_key,
            &certificate.network_key,
            &certificate.nostr_key,
        )?;
        self.conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM kin_revocations
                     WHERE identity = ?1 AND signing_key = ?2 AND network_key = ?3 AND nostr_key = ?4)",
                params![
                    key.identity,
                    key.signing.as_slice(),
                    key.network.as_slice(),
                    key.nostr.as_slice()
                ],
                |r| r.get(0),
            )
            .map_err(Error::storage)
    }

    /// Falha fechada: na dúvida (erro de leitura), trata como revogado.
    #[allow(dead_code)]
    pub(crate) fn is_revoked_or_unknown(&self, certificate: &DeviceCertificate) -> bool {
        self.is_revoked(certificate).unwrap_or(true)
    }

    /// Conclui um pareamento: grava o certificado e apaga o pedido pendente **na mesma transação**, para
    /// um corte no meio não deixar o banco com os dois. É um compare-and-swap: o certificado entra com
    /// `INSERT` simples (falha se já existe um) e o pedido só é apagado se ainda for **exatamente** o
    /// `request` lido; senão a transação é desfeita ([`Error::NotPending`]). As chaves de `also_delete`
    /// (a hora do pedido) saem na mesma transação, para não ficar resíduo no banco já pareado.
    pub(crate) fn finish_pairing(
        &self,
        certificate: &[u8],
        cert_key: &str,
        request: &[u8],
        request_key: &str,
        also_delete: &[&str],
    ) -> Result<(), Error> {
        let tx = self.conn.unchecked_transaction().map_err(Error::storage)?;
        tx.execute(
            "INSERT INTO kin_meta (key, value) VALUES (?1, ?2)",
            params![cert_key, certificate],
        )
        .map_err(Error::storage)?;
        let removed = tx
            .execute(
                "DELETE FROM kin_meta WHERE key = ?1 AND value = ?2",
                params![request_key, request],
            )
            .map_err(Error::storage)?;
        if removed != 1 {
            return Err(Error::NotPending);
        }
        for key in also_delete {
            tx.execute("DELETE FROM kin_meta WHERE key = ?1", [*key])
                .map_err(Error::storage)?;
        }
        tx.commit().map_err(Error::storage)
    }

    pub(crate) fn remember(&self, label: &str, group_id: &[u8]) -> Result<(), Error> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO kin_conversations (label, group_id) VALUES (?1, ?2)",
                params![label, group_id],
            )
            .map_err(Error::storage)?;
        Ok(())
    }

    pub(crate) fn remembered(&self) -> Result<Vec<(String, Vec<u8>)>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT label, group_id FROM kin_conversations ORDER BY label")
            .map_err(Error::storage)?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(Error::storage)?
            .collect::<Result<_, _>>()
            .map_err(Error::storage)
    }
}

/// A chave de um tombstone: `(identidade, assinatura, rede, nostr)`. A identidade entra sempre: sem ela,
/// a autoridade de outra identidade revogaria um device alheio que tivesse as mesmas chaves públicas
/// (P31 af).
struct RevocationKey {
    identity: String,
    signing: [u8; 32],
    network: [u8; 32],
    nostr: [u8; 32],
}

impl RevocationKey {
    fn new(
        identity: &IdentityId,
        signing: &PublicKey,
        network: &PublicKey,
        nostr: &NostrPublicKey,
    ) -> Result<Self, Error> {
        Ok(Self {
            identity: identity.to_base58(),
            signing: ed25519_bytes(signing)?,
            network: ed25519_bytes(network)?,
            nostr: nostr.to_bytes(),
        })
    }
}

impl OpenMlsProvider for Provider {
    type CryptoProvider = RustCrypto;
    type RandProvider = RustCrypto;
    type StorageProvider = SqliteStorageProvider<JsonCodec, Rc<Connection>>;

    fn storage(&self) -> &Self::StorageProvider {
        &self.storage
    }

    fn crypto(&self) -> &Self::CryptoProvider {
        &self.crypto
    }

    fn rand(&self) -> &Self::RandProvider {
        &self.crypto
    }
}

#[cfg(test)]
mod tests {
    use kin_identity::{
        AuthorityIdentity, AuthorityKey, DeviceKey, IdentityProvider, NostrPublicKey, PublicKey,
        StandaloneIdentity,
    };

    use super::*;

    const NOW: u64 = 1_700_000_000;

    /// As três chaves de um device (a de assinatura MLS é uma chave Ed25519 qualquer).
    #[derive(Clone)]
    struct Keys {
        signing: PublicKey,
        network: PublicKey,
        nostr: NostrPublicKey,
    }

    fn keys() -> Keys {
        let net = DeviceKey::generate();
        Keys {
            signing: DeviceKey::generate().public_key(),
            network: net.public_key(),
            nostr: net.nostr_public_key(),
        }
    }

    fn cert_for(issuer: &dyn IdentityProvider, k: &Keys) -> DeviceCertificate {
        issuer
            .authorize_device(&k.signing, &k.network, &k.nostr, NOW)
            .unwrap()
    }

    fn authority_of(master: &StandaloneIdentity) -> AuthorityIdentity {
        let key = AuthorityKey::generate();
        let cert = master.certify_authority(&key.public_key(), 1_000).unwrap();
        AuthorityIdentity::new(key, cert).unwrap()
    }

    /// Banco em memória com um tombstone da autoridade já gravado para `k`.
    fn revoked(master: &StandaloneIdentity, k: &Keys) -> (Provider, DeviceCertificate) {
        let authority = authority_of(master);
        let cert = cert_for(&authority, k);
        let provider = Provider::open(None).unwrap();
        provider
            .store_revocation(&authority.revoke_device(&cert, NOW + 60).unwrap())
            .unwrap();
        (provider, cert)
    }

    fn rows(p: &Provider) -> i64 {
        p.conn
            .query_row("SELECT COUNT(*) FROM kin_revocations", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn a_stored_revocation_makes_the_device_revoked() {
        let master = StandaloneIdentity::generate();
        let (provider, cert) = revoked(&master, &keys());
        assert!(provider.is_revoked(&cert).unwrap());
        assert!(provider.is_revoked_or_unknown(&cert));
    }

    #[test]
    fn a_device_without_a_revocation_is_not_revoked() {
        let master = StandaloneIdentity::generate();
        let (provider, _) = revoked(&master, &keys());
        let other = cert_for(&master, &keys());
        assert!(!provider.is_revoked(&other).unwrap());
        assert!(!provider.is_revoked_or_unknown(&other));
    }

    #[test]
    fn a_revocation_from_the_master_revokes_a_v3_device() {
        let master = StandaloneIdentity::generate();
        let cert = cert_for(&master, &keys());
        let provider = Provider::open(None).unwrap();
        provider
            .store_revocation(&master.revoke_device(&cert, NOW + 60).unwrap())
            .unwrap();
        assert!(provider.is_revoked(&cert).unwrap());
    }

    /// Mesma identidade, mas com UMA das três chaves diferente: é outro device, não está revogado.
    #[test]
    fn changing_any_one_of_the_three_keys_is_another_device() {
        let master = StandaloneIdentity::generate();
        let k = keys();
        let (provider, _) = revoked(&master, &k);
        let fresh = keys();
        for (name, other) in [
            (
                "signing",
                Keys {
                    signing: fresh.signing.clone(),
                    ..k.clone()
                },
            ),
            (
                "network",
                Keys {
                    network: fresh.network.clone(),
                    ..k.clone()
                },
            ),
            (
                "nostr",
                Keys {
                    nostr: fresh.nostr,
                    ..k.clone()
                },
            ),
        ] {
            let cert = cert_for(&master, &other);
            assert!(!provider.is_revoked(&cert).unwrap(), "{name}");
        }
    }

    /// P31 (af): outra identidade com as MESMAS três chaves públicas não é atingida.
    #[test]
    fn the_same_three_keys_under_another_identity_are_not_revoked() {
        let victim = StandaloneIdentity::generate();
        let k = keys();
        let (provider, _) = revoked(&StandaloneIdentity::generate(), &k);
        let cert = cert_for(&victim, &k);
        assert!(!provider.is_revoked(&cert).unwrap());
        assert!(!provider.is_revoked_or_unknown(&cert));
    }

    /// Auditoria 3b, I3: a identidade faz parte da chave primária. Duas identidades revogando as MESMAS
    /// três chaves públicas convivem, em qualquer ordem de gravação (nenhuma é engolida pela outra).
    #[test]
    fn two_identities_revoking_the_same_three_keys_both_stay() {
        let k = keys();
        for victim_first in [true, false] {
            let (x, v) = (
                StandaloneIdentity::generate(),
                StandaloneIdentity::generate(),
            );
            let (ax, av) = (authority_of(&x), authority_of(&v));
            let (cx, cv) = (cert_for(&ax, &k), cert_for(&av, &k));
            let (rx, rv) = (
                ax.revoke_device(&cx, NOW + 60).unwrap(),
                av.revoke_device(&cv, NOW + 60).unwrap(),
            );
            let provider = Provider::open(None).unwrap();
            if victim_first {
                provider.store_revocation(&rv).unwrap();
                provider.store_revocation(&rx).unwrap();
            } else {
                provider.store_revocation(&rx).unwrap();
                provider.store_revocation(&rv).unwrap();
            }
            assert_eq!(rows(&provider), 2);
            assert!(provider.is_revoked(&cx).unwrap());
            assert!(provider.is_revoked(&cv).unwrap());
        }
    }

    #[test]
    fn storing_the_same_revocation_twice_is_idempotent() {
        let master = StandaloneIdentity::generate();
        let authority = authority_of(&master);
        let cert = cert_for(&authority, &keys());
        let revocation = authority.revoke_device(&cert, NOW + 60).unwrap();
        let provider = Provider::open(None).unwrap();
        provider.store_revocation(&revocation).unwrap();
        provider.store_revocation(&revocation).unwrap();
        assert_eq!(rows(&provider), 1);
        assert!(provider.is_revoked(&cert).unwrap());
    }

    /// Sem despejo: nenhum tombstone antigo sai quando entram muitos.
    #[test]
    fn many_revocations_are_all_kept() {
        let master = StandaloneIdentity::generate();
        let authority = authority_of(&master);
        let provider = Provider::open(None).unwrap();
        let certs: Vec<_> = (0..1200)
            .map(|_| {
                let cert = cert_for(&authority, &keys());
                provider
                    .store_revocation(&authority.revoke_device(&cert, NOW + 60).unwrap())
                    .unwrap();
                cert
            })
            .collect();
        assert_eq!(rows(&provider), 1200);
        assert!(certs.iter().all(|c| provider.is_revoked(c).unwrap()));
    }

    #[test]
    fn a_revocation_with_a_bad_signature_is_refused_and_not_stored() {
        let master = StandaloneIdentity::generate();
        let authority = authority_of(&master);
        let cert = cert_for(&authority, &keys());
        let mut revocation = authority.revoke_device(&cert, NOW + 60).unwrap();
        revocation.signature[0] ^= 1;
        let provider = Provider::open(None).unwrap();
        assert!(provider.store_revocation(&revocation).is_err());
        assert_eq!(rows(&provider), 0);
        assert!(!provider.is_revoked(&cert).unwrap());
    }

    /// Fail-closed: se não dá para ler o banco, o device conta como revogado.
    #[test]
    fn a_read_error_counts_as_revoked() {
        let master = StandaloneIdentity::generate();
        let (provider, _) = revoked(&master, &keys());
        let unrelated = cert_for(&master, &keys());
        provider
            .conn
            .execute("DROP TABLE kin_revocations", [])
            .unwrap();
        assert!(provider.is_revoked(&unrelated).is_err());
        assert!(provider.is_revoked_or_unknown(&unrelated));
    }

    #[test]
    fn revocations_survive_reopening_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mls.sqlite");
        let master = StandaloneIdentity::generate();
        let authority = authority_of(&master);
        let cert = cert_for(&authority, &keys());
        {
            let provider = Provider::open(Some(&path)).unwrap();
            provider
                .store_revocation(&authority.revoke_device(&cert, NOW + 60).unwrap())
                .unwrap();
        }
        let reopened = Provider::open(Some(&path)).unwrap();
        assert!(reopened.is_revoked(&cert).unwrap());
    }
}
