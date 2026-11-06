//! Identidade com provedores (D7): standalone e TruthID opcional, chaves de device, Peer ID.
//!
//! O resto do Kin só enxerga [`IdentityProvider`]: um ID estável, a capacidade de assinar e a
//! autorização de devices. Quem está por trás (keypair local, TruthID) é detalhe do provedor.

mod device;
mod error;
mod provider;
mod standalone;
mod store;

pub use device::{
    DeviceCertificate, DeviceKey, NostrPublicKey, ed25519_bytes, public_key_from_ed25519,
};
pub use error::Error;
pub use libp2p_identity::{PeerId, PublicKey};
pub use provider::{IdentityId, IdentityProvider};
pub use standalone::StandaloneIdentity;
