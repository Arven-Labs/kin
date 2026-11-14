//! Pedido de pareamento (P30 fatia 2c, ADR 0016): o device novo mostra as três chaves públicas e prova
//! que tem as três privadas; o primário confere e responde com o certificado v4.
//!
//! Transporte manual (texto ou QR), sem rede. A confirmação humana (comparar o [`PairRequest::sas_code`]
//! nas duas telas) é de quem chama: sem ela, quem intercepta o canal troca o pedido por um seu.

use libp2p_identity::PublicKey;
use sha2::{Digest, Sha256};

use crate::device::{put, take};
use crate::{DeviceKey, Error, IdentityId, NostrPublicKey};

/// Tamanho do nonce do pedido, em bytes.
pub const PAIR_NONCE_LEN: usize = 16;

/// Domínio da prova de posse. Nenhum domínio do Kin é prefixo de outro.
const PROOF_DOMAIN: &[u8] = b"kin/pair-request/v1";
/// Domínio do código de comparação.
const SAS_DOMAIN: &[u8] = b"kin/pair-sas/v1";
const MAGIC: &[u8; 3] = b"KPR";
const VERSION: u8 = 1;
/// Teto do pedido serializado (o real tem ~330 bytes): recusa lixo grande antes de ler.
const MAX_REQUEST_LEN: usize = 1024;
/// 10^18: o código tem 18 dígitos (~59,8 bits).
const SAS_MODULUS: u64 = 1_000_000_000_000_000_000;

/// Pedido autoassinado de um device novo. Cada chave assina (com domínio próprio) o conjunto das três
/// chaves e o nonce: prova de posse, sem a qual alguém poderia pôr a chave de outro no pedido.
///
/// Os campos são públicos como os do `DeviceCertificate`: [`Self::verify`] é quem garante a coerência.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairRequest {
    /// Chave de assinatura MLS do device.
    pub signing_key: PublicKey,
    /// Chave de rede do device.
    pub network_key: PublicKey,
    /// Chave Nostr do device.
    pub nostr_key: NostrPublicKey,
    pub nonce: [u8; PAIR_NONCE_LEN],
    /// Assinatura da chave MLS sobre o conjunto.
    pub signing_proof: Vec<u8>,
    /// Assinatura da chave de rede sobre o conjunto.
    pub network_proof: Vec<u8>,
    /// Assinatura Schnorr da chave Nostr sobre o conjunto.
    pub nostr_proof: Vec<u8>,
}

/// `domínio | (len32 | campo)*`: o prefixo de tamanho impede que dois conjuntos diferentes de campos
/// produzam os mesmos bytes.
fn framed(domain: &[u8], fields: &[&[u8]]) -> Vec<u8> {
    let mut out = domain.to_vec();
    for field in fields {
        out.extend_from_slice(&(field.len() as u32).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

fn proof_payload(
    signing_key: &PublicKey,
    network_key: &PublicKey,
    nostr_key: &NostrPublicKey,
    nonce: &[u8; PAIR_NONCE_LEN],
) -> Vec<u8> {
    framed(
        PROOF_DOMAIN,
        &[
            &signing_key.encode_protobuf(),
            &network_key.encode_protobuf(),
            &nostr_key.to_bytes(),
            nonce,
        ],
    )
}

impl PairRequest {
    /// Monta o pedido de `device` (rede e Nostr) com a chave MLS `signing_key`, cuja assinatura vem de
    /// `sign_mls` (a chave MLS é do openmls e não passa por aqui). `sign_mls` recebe os bytes com domínio.
    pub fn new(
        device: &DeviceKey,
        signing_key: &PublicKey,
        nonce: [u8; PAIR_NONCE_LEN],
        sign_mls: impl FnOnce(&[u8]) -> Result<Vec<u8>, Error>,
    ) -> Result<Self, Error> {
        let network_key = device.public_key();
        let nostr_key = device.nostr_public_key();
        let payload = proof_payload(signing_key, &network_key, &nostr_key, &nonce);
        Ok(Self {
            signing_key: signing_key.clone(),
            signing_proof: sign_mls(&payload)?,
            network_proof: device.keypair().sign(&payload)?,
            nostr_proof: device.sign_nostr(&payload),
            network_key,
            nostr_key,
            nonce,
        })
    }

    /// Confere as três provas de posse. Não diz qual falhou.
    pub fn verify(&self) -> Result<(), Error> {
        let payload = proof_payload(
            &self.signing_key,
            &self.network_key,
            &self.nostr_key,
            &self.nonce,
        );
        let signing = self.signing_key.verify(&payload, &self.signing_proof);
        let network = self.network_key.verify(&payload, &self.network_proof);
        let nostr = self.nostr_key.verify(&payload, &self.nostr_proof);
        if signing && network && nostr {
            Ok(())
        } else {
            Err(Error::InvalidPairRequest)
        }
    }

    /// `"KPR" | 1 | len16|chave MLS | len16|chave de rede | len16|chave Nostr | nonce 16 B |
    /// len16|prova MLS | len16|prova de rede | len16|prova Nostr`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.push(VERSION);
        put(&mut out, &self.signing_key.encode_protobuf());
        put(&mut out, &self.network_key.encode_protobuf());
        put(&mut out, &self.nostr_key.to_bytes());
        out.extend_from_slice(&self.nonce);
        put(&mut out, &self.signing_proof);
        put(&mut out, &self.network_proof);
        put(&mut out, &self.nostr_proof);
        out
    }

    /// Lê um pedido serializado. Não confere as provas: use [`Self::verify`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        // Genérica: serve ao erro do `take` e ao do decodificador de chave, sem expor o detalhe.
        fn invalid<E>(_: E) -> Error {
            Error::InvalidPairRequest
        }
        if bytes.len() > MAX_REQUEST_LEN {
            return Err(Error::InvalidPairRequest);
        }
        let mut cur = bytes
            .strip_prefix(MAGIC.as_slice())
            .and_then(|rest| rest.strip_prefix(&[VERSION]))
            .ok_or(Error::InvalidPairRequest)?;
        let signing_key =
            PublicKey::try_decode_protobuf(take(&mut cur).map_err(invalid)?).map_err(invalid)?;
        let network_key =
            PublicKey::try_decode_protobuf(take(&mut cur).map_err(invalid)?).map_err(invalid)?;
        let nostr_key =
            NostrPublicKey::from_bytes(take(&mut cur).map_err(invalid)?).map_err(invalid)?;
        let nonce: [u8; PAIR_NONCE_LEN] = cur
            .get(..PAIR_NONCE_LEN)
            .and_then(|n| n.try_into().ok())
            .ok_or(Error::InvalidPairRequest)?;
        cur = &cur[PAIR_NONCE_LEN..];
        let signing_proof = take(&mut cur).map_err(invalid)?.to_vec();
        let network_proof = take(&mut cur).map_err(invalid)?.to_vec();
        let nostr_proof = take(&mut cur).map_err(invalid)?.to_vec();
        if !cur.is_empty() {
            return Err(Error::InvalidPairRequest);
        }
        Ok(Self {
            signing_key,
            network_key,
            nostr_key,
            nonce,
            signing_proof,
            network_proof,
            nostr_proof,
        })
    }

    /// Código de comparação que as duas telas mostram: 18 dígitos em três grupos de seis
    /// (`123456-123456-123456`, ~60 bits), derivados das chaves, do nonce e da identidade para a qual o
    /// primário vai certificar. **Não** cobre as provas (a Schnorr pode não ser determinística).
    pub fn sas_code(&self, identity: &IdentityId) -> String {
        let hash = Sha256::digest(framed(
            SAS_DOMAIN,
            &[
                &identity.public_key().encode_protobuf()[..0],
                &self.signing_key.encode_protobuf(),
                &self.network_key.encode_protobuf(),
                &self.nostr_key.to_bytes(),
                &self.nonce,
            ],
        ));
        let head: [u8; 8] = hash.as_slice()[..8]
            .try_into()
            .expect("SHA-256 tem 32 bytes");
        let digits = u64::from_be_bytes(head) % SAS_MODULUS;
        let text = format!("{digits:018}");
        format!("{}-{}-{}", &text[..6], &text[6..12], &text[12..])
    }
}
