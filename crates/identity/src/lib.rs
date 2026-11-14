//! Identidade com provedores (D7): standalone e TruthID opcional, chaves de device, Peer ID.
//!
//! O resto do Kin só enxerga [`IdentityProvider`]: um ID estável e a autorização de devices. A chave
//! da identidade não assina bytes livres, só formatos com domínio (S1, ADR 0013). Quem está por trás (keypair local, TruthID) é detalhe do provedor.

mod authority;
mod device;
mod error;
mod provider;
mod sealed;
mod standalone;
mod store;

pub use authority::{AuthorityCertificate, AuthorityKey};
pub use device::{
    DeviceCertificate, DeviceKey, NostrPublicKey, ed25519_bytes, public_key_from_ed25519,
};
pub use error::Error;
pub use libp2p_identity::{PeerId, PublicKey};
pub use provider::{IdentityId, IdentityProvider};
pub use sealed::{KdfParams, is_sealed, normalize_passphrase, open, seal};
pub use standalone::StandaloneIdentity;
pub use store::key_file_is_sealed;
