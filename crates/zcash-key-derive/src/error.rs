use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid mnemonic: {0}")]
    Mnemonic(#[from] bip39::Error),

    #[error("key derivation failed: {0}")]
    Derivation(String),

    #[error("BIP-32 error: {0}")]
    Bip32(#[from] bitcoin::bip32::Error),
}
