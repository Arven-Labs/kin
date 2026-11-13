//! Cifra em repouso dos arquivos de chave (P13, ADR 0012): Argon2id deriva a chave da senha e
//! XChaCha20-Poly1305 cifra e autentica. Formato:
//!
//! `"KINK" | versão u8 | m_cost u32 BE (KiB) | t_cost u32 BE | lanes u8 | salt 16 | nonce 24 | cifra + tag 16`
//!
//! O cabeçalho inteiro entra como dado associado (AAD) do AEAD.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use secp256k1::rand::RngCore;
use zeroize::Zeroizing;

use crate::Error;

const MAGIC: &[u8; 4] = b"KINK";
const VERSION: u8 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const HEADER_LEN: usize = 4 + 1 + 4 + 4 + 1 + SALT_LEN + NONCE_LEN;
const TAG_LEN: usize = 16;

/// Custo do Argon2id gravado no cabeçalho. Na leitura os valores têm piso e teto: o arquivo é entrada
/// não confiável, e um `m_cost` de dezenas de GiB travaria o processo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    m_cost_kib: u32,
    t_cost: u32,
    lanes: u8,
}

impl KdfParams {
    /// Padrão de produção: 64 MiB, 3 passadas, 1 lane.
    pub const DEFAULT: Self = Self {
        m_cost_kib: 64 * 1024,
        t_cost: 3,
        lanes: 1,
    };
    /// O mínimo aceito (8 MiB, 1 passada): serve aos testes e é o piso na leitura.
    pub const MIN: Self = Self {
        m_cost_kib: 8 * 1024,
        t_cost: 1,
        lanes: 1,
    };
    pub(crate) const MAX_M_COST_KIB: u32 = 1024 * 1024;
    pub(crate) const MAX_T_COST: u32 = 10;
    pub(crate) const MAX_LANES: u8 = 4;

    /// Valida contra o piso e o teto.
    pub fn new(m_cost_kib: u32, t_cost: u32, lanes: u8) -> Result<Self, Error> {
        let in_bounds = (Self::MIN.m_cost_kib..=Self::MAX_M_COST_KIB).contains(&m_cost_kib)
            && (Self::MIN.t_cost..=Self::MAX_T_COST).contains(&t_cost)
            && (Self::MIN.lanes..=Self::MAX_LANES).contains(&lanes);
        if in_bounds {
            Ok(Self {
                m_cost_kib,
                t_cost,
                lanes,
            })
        } else {
            Err(Error::InvalidSealedFile)
        }
    }

    fn derive_key(
        self,
        passphrase: &[u8],
        salt: &[u8; SALT_LEN],
    ) -> Result<Zeroizing<[u8; 32]>, Error> {
        let params = Params::new(
            self.m_cost_kib,
            self.t_cost,
            u32::from(self.lanes),
            Some(32),
        )
        .map_err(|_| Error::InvalidSealedFile)?;
        let mut key = Zeroizing::new([0u8; 32]);
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(passphrase, salt, key.as_mut())
            .map_err(|_| Error::InvalidSealedFile)?;
        Ok(key)
    }
}

/// `true` se `bytes` começa pelo magic de um arquivo cifrado. Não valida o resto.
pub fn is_sealed(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

/// Cifra `plaintext` com a senha. Salt e nonce novos a cada chamada. Senha vazia é recusada.
pub fn seal(plaintext: &[u8], passphrase: &[u8], params: KdfParams) -> Result<Vec<u8>, Error> {
    if passphrase.is_empty() {
        return Err(Error::EmptyPassphrase);
    }
    // Revalida: `params` pode ter vindo de uma constante ou de `new`, mas o formato só aceita o que `open` aceita.
    let params = KdfParams::new(params.m_cost_kib, params.t_cost, params.lanes)?;
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    let mut rng = secp256k1::rand::thread_rng();
    let _ = &mut rng;
    rng.fill_bytes(&mut nonce);

    let mut out = Vec::with_capacity(HEADER_LEN + plaintext.len() + TAG_LEN);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&params.m_cost_kib.to_be_bytes());
    out.extend_from_slice(&params.t_cost.to_be_bytes());
    out.push(params.lanes);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);

    let key = params.derive_key(passphrase, &salt)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_ref()));
    let encrypted = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &out,
            },
        )
        .map_err(|_| Error::WrongPassphraseOrCorrupt)?;
    out.extend_from_slice(&encrypted);
    Ok(out)
}

/// Abre um arquivo cifrado. Os parâmetros do cabeçalho são conferidos **antes** de rodar o KDF.
pub fn open(bytes: &[u8], passphrase: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
    if passphrase.is_empty() {
        return Err(Error::EmptyPassphrase);
    }
    if bytes.len() < HEADER_LEN + TAG_LEN || !is_sealed(bytes) || bytes[4] != VERSION {
        return Err(Error::InvalidSealedFile);
    }
    let (header, body) = bytes.split_at(HEADER_LEN);
    let m = u32::from_be_bytes(header[5..9].try_into().expect("4 bytes"));
    let t = u32::from_be_bytes(header[9..13].try_into().expect("4 bytes"));
    let params = KdfParams::new(m, t, header[13])?;
    let salt: [u8; SALT_LEN] = header[14..14 + SALT_LEN].try_into().expect("16 bytes");
    let nonce: [u8; NONCE_LEN] = header[14 + SALT_LEN..HEADER_LEN]
        .try_into()
        .expect("24 bytes");

    let key = params.derive_key(passphrase, &salt)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_ref()));
    cipher
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: body,
                aad: header,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| Error::WrongPassphraseOrCorrupt)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASS: &[u8] = b"correct horse battery staple";
    const SECRET: &[u8] = b"segredo-da-mestra-0123456789abcdef";

    fn sealed() -> Vec<u8> {
        seal(SECRET, PASS, KdfParams::MIN).unwrap()
    }

    /// Cabeçalho forjado com os parâmetros dados e um corpo qualquer (nunca chega ao KDF se os
    /// parâmetros forem recusados).
    fn forged(version: u8, m: u32, t: u32, lanes: u8) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.push(version);
        out.extend_from_slice(&m.to_be_bytes());
        out.extend_from_slice(&t.to_be_bytes());
        out.push(lanes);
        out.extend_from_slice(&[7; 16 + 24]);
        out.extend_from_slice(&[9; 32 + TAG_LEN]);
        out
    }

    #[test]
    fn open_returns_what_was_sealed() {
        assert_eq!(&*open(&sealed(), PASS).unwrap(), SECRET);
    }

    #[test]
    fn wrong_passphrase_is_the_same_error_as_corruption() {
        let wrong = open(&sealed(), b"outra senha").unwrap_err();
        assert!(matches!(wrong, Error::WrongPassphraseOrCorrupt));
        let mut corrupt = sealed();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 1;
        assert!(matches!(
            open(&corrupt, PASS).unwrap_err(),
            Error::WrongPassphraseOrCorrupt
        ));
    }

    #[test]
    fn flipping_any_single_byte_fails() {
        let original = sealed();
        for i in 0..original.len() {
            let mut tampered = original.clone();
            tampered[i] ^= 0x01;
            assert!(open(&tampered, PASS).is_err(), "byte {i} adulterado abriu");
        }
    }

    #[test]
    fn truncation_at_every_length_fails_without_panic() {
        let original = sealed();
        for len in 0..original.len() {
            assert!(open(&original[..len], PASS).is_err(), "tamanho {len} abriu");
        }
    }

    #[test]
    fn trailing_garbage_fails() {
        let mut extended = sealed();
        extended.push(0);
        assert!(open(&extended, PASS).is_err());
    }

    #[test]
    fn wrong_magic_and_unknown_version_are_invalid_files() {
        let mut bad_magic = sealed();
        bad_magic[0] = b'X';
        assert!(matches!(
            open(&bad_magic, PASS).unwrap_err(),
            Error::InvalidSealedFile
        ));
        let mut bad_version = sealed();
        bad_version[4] = VERSION + 1;
        assert!(matches!(
            open(&bad_version, PASS).unwrap_err(),
            Error::InvalidSealedFile
        ));
    }

    #[test]
    fn header_params_outside_the_bounds_are_rejected_before_the_kdf() {
        let m_min = KdfParams::MIN.m_cost_kib;
        let cases = [
            ("m acima do teto", forged(VERSION, u32::MAX, 1, 1)),
            (
                "m um acima do teto",
                forged(VERSION, KdfParams::MAX_M_COST_KIB + 1, 1, 1),
            ),
            ("m abaixo do piso", forged(VERSION, m_min - 1, 1, 1)),
            ("t zero", forged(VERSION, m_min, 0, 1)),
            (
                "t acima do teto",
                forged(VERSION, m_min, KdfParams::MAX_T_COST + 1, 1),
            ),
            ("lanes zero", forged(VERSION, m_min, 1, 0)),
            (
                "lanes acima do teto",
                forged(VERSION, m_min, 1, KdfParams::MAX_LANES + 1),
            ),
        ];
        for (name, bytes) in cases {
            // `InvalidSealedFile` (e não `WrongPassphraseOrCorrupt`) prova que o KDF nem rodou.
            assert!(
                matches!(open(&bytes, PASS).unwrap_err(), Error::InvalidSealedFile),
                "{name}"
            );
        }
    }

    #[test]
    fn params_at_the_bounds_are_accepted() {
        assert!(KdfParams::new(KdfParams::MIN.m_cost_kib, 1, 1).is_ok());
        assert!(KdfParams::new(KdfParams::MAX_M_COST_KIB, KdfParams::MAX_T_COST, 4).is_ok());
        assert!(KdfParams::new(KdfParams::MIN.m_cost_kib - 1, 1, 1).is_err());
        assert!(KdfParams::new(KdfParams::MAX_M_COST_KIB + 1, 1, 1).is_err());
        assert!(KdfParams::new(KdfParams::MIN.m_cost_kib, 0, 1).is_err());
        assert!(KdfParams::new(KdfParams::MIN.m_cost_kib, KdfParams::MAX_T_COST + 1, 1).is_err());
        assert!(KdfParams::new(KdfParams::MIN.m_cost_kib, 1, 0).is_err());
        assert!(KdfParams::new(KdfParams::MIN.m_cost_kib, 1, KdfParams::MAX_LANES + 1).is_err());
    }

    #[test]
    fn default_params_are_within_the_bounds() {
        let d = KdfParams::DEFAULT;
        assert_eq!(KdfParams::new(d.m_cost_kib, d.t_cost, d.lanes).unwrap(), d);
    }

    #[test]
    fn each_seal_uses_fresh_salt_and_nonce() {
        let (a, b) = (sealed(), sealed());
        assert_ne!(a[14..30], b[14..30], "salt repetido");
        assert_ne!(a[30..54], b[30..54], "nonce repetido");
        assert_ne!(a[54..], b[54..]);
    }

    #[test]
    fn plaintext_does_not_appear_in_the_output() {
        let out = sealed();
        assert!(!out.windows(SECRET.len()).any(|w| w == SECRET));
        assert_eq!(out.len(), HEADER_LEN + SECRET.len() + TAG_LEN);
    }

    #[test]
    fn empty_passphrase_is_refused_on_seal_and_open() {
        assert!(matches!(
            seal(SECRET, b"", KdfParams::MIN).unwrap_err(),
            Error::EmptyPassphrase
        ));
        assert!(matches!(
            open(&sealed(), b"").unwrap_err(),
            Error::EmptyPassphrase
        ));
    }

    #[test]
    fn is_sealed_recognizes_only_the_magic() {
        assert!(is_sealed(&sealed()));
        // Arquivo antigo da mestra (protobuf Ed25519 começa por 0x08) e vazio.
        assert!(!is_sealed(&[0x08, 0x01, 0x12, 0x40]));
        assert!(!is_sealed(&[]));
        assert!(!is_sealed(b"KIN"));
    }
}
