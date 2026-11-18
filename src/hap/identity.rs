//! The accessory's long-term Ed25519 identity (from crtd `src/raop/pairing.rs`). It must survive
//! restarts, or controllers see a different accessory.

use ed25519_dalek::{Signer, SigningKey};
use rand_core::OsRng;
use std::path::Path;

pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    pub fn load_or_create(path: &Path) -> std::io::Result<Self> {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(seed) = <[u8; 32]>::try_from(bytes.as_slice()) {
                return Ok(Self { signing: SigningKey::from_bytes(&seed) });
            }
        }
        let signing = SigningKey::generate(&mut OsRng);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, signing.to_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self { signing })
    }

    #[cfg(test)]
    pub fn random() -> Self {
        Self { signing: SigningKey::generate(&mut OsRng) }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }
}
