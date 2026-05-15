use {
    crate::env::EnvReader,
    base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD},
    solana_keypair::{KEYPAIR_LENGTH, Keypair, Signer},
    solana_pubkey::Pubkey,
    std::fmt,
    thiserror::Error,
};

pub const SOURCE_PRIVATE_KEY_BASE58: &str = "SOURCE_PRIVATE_KEY_BASE58";
pub const SOURCE_PRIVATE_KEY_BASE64: &str = "SOURCE_PRIVATE_KEY_BASE64";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePrivateKeyEncoding {
    Base58,
    Base64,
}

pub struct SourceWallet {
    keypair: Keypair,
    encoding: SourcePrivateKeyEncoding,
}

impl SourceWallet {
    pub fn from_env(env: &impl EnvReader) -> Result<Self, SourceWalletError> {
        let base58 = env.get(SOURCE_PRIVATE_KEY_BASE58);
        let base64 = env.get(SOURCE_PRIVATE_KEY_BASE64);

        match (base58, base64) {
            (None, None) => Err(SourceWalletError::Missing),
            (Some(_), Some(_)) => Err(SourceWalletError::Multiple),
            (Some(encoded), None) => Self::from_base58(&encoded),
            (None, Some(encoded)) => Self::from_base64(&encoded),
        }
    }

    pub fn from_base58(encoded: &str) -> Result<Self, SourceWalletError> {
        let keypair = Keypair::try_from_base58_string(encoded.trim()).map_err(|_| {
            SourceWalletError::Invalid {
                env_var: SOURCE_PRIVATE_KEY_BASE58,
            }
        })?;
        Ok(Self {
            keypair,
            encoding: SourcePrivateKeyEncoding::Base58,
        })
    }

    pub fn from_base64(encoded: &str) -> Result<Self, SourceWalletError> {
        let bytes =
            BASE64_STANDARD
                .decode(encoded.trim())
                .map_err(|_| SourceWalletError::Invalid {
                    env_var: SOURCE_PRIVATE_KEY_BASE64,
                })?;

        if bytes.len() != KEYPAIR_LENGTH {
            return Err(SourceWalletError::InvalidLength {
                env_var: SOURCE_PRIVATE_KEY_BASE64,
                actual: bytes.len(),
            });
        }

        let keypair =
            Keypair::try_from(bytes.as_slice()).map_err(|_| SourceWalletError::Invalid {
                env_var: SOURCE_PRIVATE_KEY_BASE64,
            })?;

        Ok(Self {
            keypair,
            encoding: SourcePrivateKeyEncoding::Base64,
        })
    }

    pub fn keypair(&self) -> &Keypair {
        &self.keypair
    }

    pub fn public_key(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    pub fn encoding(&self) -> SourcePrivateKeyEncoding {
        self.encoding
    }
}

impl fmt::Debug for SourceWallet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceWallet")
            .field("public_key", &self.public_key().to_string())
            .field("encoding", &self.encoding)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SourceWalletError {
    #[error("set exactly one of SOURCE_PRIVATE_KEY_BASE58 or SOURCE_PRIVATE_KEY_BASE64")]
    Missing,
    #[error("set only one of SOURCE_PRIVATE_KEY_BASE58 or SOURCE_PRIVATE_KEY_BASE64")]
    Multiple,
    #[error("{env_var} is not valid source keypair material")]
    Invalid { env_var: &'static str },
    #[error("{env_var} decoded to {actual} bytes, expected {KEYPAIR_LENGTH} bytes")]
    InvalidLength {
        env_var: &'static str,
        actual: usize,
    },
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD},
        solana_keypair::Keypair,
        std::collections::BTreeMap,
    };

    #[test]
    fn loads_base58_source_key() {
        let keypair = Keypair::new();
        let encoded = keypair.to_base58_string();
        let mut env = BTreeMap::new();
        env.insert(SOURCE_PRIVATE_KEY_BASE58.to_owned(), encoded.clone());

        let wallet = SourceWallet::from_env(&env).unwrap();

        assert_eq!(wallet.public_key(), keypair.pubkey());
        assert_eq!(wallet.encoding(), SourcePrivateKeyEncoding::Base58);
        assert!(!format!("{wallet:?}").contains(&encoded));
    }

    #[test]
    fn loads_base64_source_key() {
        let keypair = Keypair::new();
        let encoded = BASE64_STANDARD.encode(keypair.to_bytes());
        let mut env = BTreeMap::new();
        env.insert(SOURCE_PRIVATE_KEY_BASE64.to_owned(), encoded.clone());

        let wallet = SourceWallet::from_env(&env).unwrap();

        assert_eq!(wallet.public_key(), keypair.pubkey());
        assert_eq!(wallet.encoding(), SourcePrivateKeyEncoding::Base64);
        assert!(!format!("{wallet:?}").contains(&encoded));
    }

    #[test]
    fn rejects_missing_source_key() {
        let err = SourceWallet::from_env(&BTreeMap::new()).unwrap_err();

        assert_eq!(err, SourceWalletError::Missing);
    }

    #[test]
    fn rejects_multiple_source_keys() {
        let keypair = Keypair::new();
        let mut env = BTreeMap::new();
        env.insert(
            SOURCE_PRIVATE_KEY_BASE58.to_owned(),
            keypair.to_base58_string(),
        );
        env.insert(
            SOURCE_PRIVATE_KEY_BASE64.to_owned(),
            BASE64_STANDARD.encode(keypair.to_bytes()),
        );

        let err = SourceWallet::from_env(&env).unwrap_err();

        assert_eq!(err, SourceWalletError::Multiple);
    }

    #[test]
    fn rejects_short_base64_source_key() {
        let mut env = BTreeMap::new();
        env.insert(
            SOURCE_PRIVATE_KEY_BASE64.to_owned(),
            BASE64_STANDARD.encode([1_u8, 2, 3]),
        );

        let err = SourceWallet::from_env(&env).unwrap_err();

        assert_eq!(
            err,
            SourceWalletError::InvalidLength {
                env_var: SOURCE_PRIVATE_KEY_BASE64,
                actual: 3
            }
        );
    }
}
