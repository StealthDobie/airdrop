use {
    crate::{
        config::{ConfigError, ConfigFile, ValidatedConfig},
        env::EnvReader,
        source_wallet::{SourceWallet, SourceWalletError},
    },
    std::fmt,
    std::path::Path,
    thiserror::Error,
};

pub struct RuntimeConfig {
    pub config: ValidatedConfig,
    pub source_wallet: SourceWallet,
    pub rpc_url: String,
    pub solscan_api_key: Option<String>,
}

impl RuntimeConfig {
    pub fn from_path_and_env(
        path: impl AsRef<Path>,
        env: &impl EnvReader,
    ) -> Result<Self, RuntimeConfigError> {
        let config = ConfigFile::from_path(path)?.validate()?;
        Self::from_validated_config_and_env(config, env)
    }

    pub fn from_validated_config_and_env(
        config: ValidatedConfig,
        env: &impl EnvReader,
    ) -> Result<Self, RuntimeConfigError> {
        let source_wallet = SourceWallet::from_env(env)?;
        let rpc_url_env = require_env(env, &config.rpc_url_env)?;
        let solscan_api_key_env = if config.solscan.enabled {
            Some(require_env(env, &config.solscan.api_key_env)?)
        } else {
            None
        };

        Ok(Self {
            config,
            source_wallet,
            rpc_url: rpc_url_env,
            solscan_api_key: solscan_api_key_env,
        })
    }
}

impl fmt::Debug for RuntimeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeConfig")
            .field("config", &self.config)
            .field("source_wallet", &self.source_wallet)
            .field("rpc_url", &"<redacted>")
            .field(
                "solscan_api_key",
                &self.solscan_api_key.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

fn require_env(env: &impl EnvReader, key: &str) -> Result<String, RuntimeConfigError> {
    env.get(key).ok_or_else(|| RuntimeConfigError::MissingEnv {
        env_var: key.to_owned(),
    })
}

#[derive(Debug, Error)]
pub enum RuntimeConfigError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    SourceWallet(#[from] SourceWalletError),
    #[error("required environment variable `{env_var}` is not set")]
    MissingEnv { env_var: String },
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::config::ConfigFile,
        solana_keypair::{Keypair, Signer},
        std::collections::BTreeMap,
    };

    const CONFIG: &str = r#"
[cluster]
name = "mainnet-beta"
rpc_url_env = "SOLANA_RPC_URL"

[distribution]
token_address = "J3mfHoQb27xHL1xUYsoPfU1vZHbzCeK7fZYvsWeYdoge"
total_amount_ui = "1000"

[targeting]
target_token_addresses = ["11111111111111111111111111111111"]
max_recipients = 10
manual_exclude_wallets = []
"#;

    #[test]
    fn loads_runtime_without_printing_secrets() {
        let keypair = Keypair::new();
        let secret = keypair.to_base58_string();
        let mut env = BTreeMap::new();
        env.insert(
            "SOLANA_RPC_URL".to_owned(),
            "https://example.invalid".to_owned(),
        );
        env.insert("SOURCE_PRIVATE_KEY_BASE58".to_owned(), secret.clone());

        let config = ConfigFile::from_toml_str(CONFIG)
            .unwrap()
            .validate()
            .unwrap();
        let runtime = RuntimeConfig::from_validated_config_and_env(config, &env).unwrap();

        let debug = format!("{runtime:?}");
        assert_eq!(runtime.source_wallet.public_key(), keypair.pubkey());
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(&secret));
    }

    #[test]
    fn requires_rpc_url_env() {
        let keypair = Keypair::new();
        let mut env = BTreeMap::new();
        env.insert(
            "SOURCE_PRIVATE_KEY_BASE58".to_owned(),
            keypair.to_base58_string(),
        );

        let config = ConfigFile::from_toml_str(CONFIG)
            .unwrap()
            .validate()
            .unwrap();
        let err = RuntimeConfig::from_validated_config_and_env(config, &env).unwrap_err();

        assert!(matches!(
            err,
            RuntimeConfigError::MissingEnv { env_var } if env_var == "SOLANA_RPC_URL"
        ));
    }
}
