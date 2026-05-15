use {
    serde::Deserialize,
    solana_pubkey::Pubkey,
    std::{collections::HashSet, fs, path::Path, str::FromStr},
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse TOML config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("cluster.name must be `mainnet-beta`, got `{0}`")]
    UnsupportedCluster(String),
    #[error("{field} must not be empty")]
    EmptyField { field: &'static str },
    #[error("{field} must be greater than zero")]
    NotPositive { field: &'static str },
    #[error("{field} must be a positive decimal string")]
    InvalidAmount { field: &'static str },
    #[error("{field} contains an invalid Solana address `{value}`")]
    InvalidAddress { field: &'static str, value: String },
    #[error("{field} contains duplicate address `{value}`")]
    DuplicateAddress { field: &'static str, value: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub cluster: ClusterConfig,
    pub distribution: DistributionConfig,
    pub targeting: TargetingConfig,
    #[serde(default)]
    pub providers: ProvidersConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    pub name: String,
    pub rpc_url_env: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistributionConfig {
    pub token_address: String,
    pub total_amount_ui: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetingConfig {
    pub target_token_addresses: Vec<String>,
    pub max_recipients: usize,
    #[serde(default)]
    pub manual_exclude_wallets: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub solscan: SolscanConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SolscanConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_solscan_api_key_env")]
    pub api_key_env: String,
}

impl Default for SolscanConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key_env: default_solscan_api_key_env(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedConfig {
    pub cluster_name: String,
    pub rpc_url_env: String,
    pub distribution_token_address: Pubkey,
    pub total_amount_ui: String,
    pub target_token_addresses: Vec<Pubkey>,
    pub max_recipients: usize,
    pub manual_exclude_wallets: Vec<Pubkey>,
    pub solscan: ValidatedSolscanConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSolscanConfig {
    pub enabled: bool,
    pub api_key_env: String,
}

impl ConfigFile {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml_str(&contents)
    }

    pub fn from_toml_str(contents: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(contents)?)
    }

    pub fn validate(self) -> Result<ValidatedConfig, ConfigError> {
        if self.cluster.name != "mainnet-beta" {
            return Err(ConfigError::UnsupportedCluster(self.cluster.name));
        }

        let rpc_url_env = require_nonempty("cluster.rpc_url_env", self.cluster.rpc_url_env)?;
        let distribution_token_address = parse_address(
            "distribution.token_address",
            &self.distribution.token_address,
        )?;
        let total_amount_ui = validate_amount(
            "distribution.total_amount_ui",
            self.distribution.total_amount_ui,
        )?;

        if self.targeting.max_recipients == 0 {
            return Err(ConfigError::NotPositive {
                field: "targeting.max_recipients",
            });
        }

        if self.targeting.target_token_addresses.is_empty() {
            return Err(ConfigError::EmptyField {
                field: "targeting.target_token_addresses",
            });
        }

        let target_token_addresses = parse_unique_addresses(
            "targeting.target_token_addresses",
            self.targeting.target_token_addresses,
        )?;
        let manual_exclude_wallets = parse_unique_addresses(
            "targeting.manual_exclude_wallets",
            self.targeting.manual_exclude_wallets,
        )?;

        let api_key_env = require_nonempty(
            "providers.solscan.api_key_env",
            self.providers.solscan.api_key_env,
        )?;

        Ok(ValidatedConfig {
            cluster_name: "mainnet-beta".to_owned(),
            rpc_url_env,
            distribution_token_address,
            total_amount_ui,
            target_token_addresses,
            max_recipients: self.targeting.max_recipients,
            manual_exclude_wallets,
            solscan: ValidatedSolscanConfig {
                enabled: self.providers.solscan.enabled,
                api_key_env,
            },
        })
    }
}

fn default_solscan_api_key_env() -> String {
    "SOLSCAN_API_KEY".to_owned()
}

fn require_nonempty(field: &'static str, value: String) -> Result<String, ConfigError> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(ConfigError::EmptyField { field });
    }
    Ok(value)
}

fn validate_amount(field: &'static str, value: String) -> Result<String, ConfigError> {
    let value = require_nonempty(field, value)?;
    let mut saw_dot = false;
    let mut saw_digit = false;
    let mut saw_nonzero = false;

    for ch in value.chars() {
        if ch == '.' {
            if saw_dot {
                return Err(ConfigError::InvalidAmount { field });
            }
            saw_dot = true;
            continue;
        }

        if !ch.is_ascii_digit() {
            return Err(ConfigError::InvalidAmount { field });
        }

        saw_digit = true;
        saw_nonzero |= ch != '0';
    }

    if !saw_digit || !saw_nonzero || value.starts_with('.') || value.ends_with('.') {
        return Err(ConfigError::InvalidAmount { field });
    }

    Ok(value)
}

fn parse_unique_addresses(
    field: &'static str,
    values: Vec<String>,
) -> Result<Vec<Pubkey>, ConfigError> {
    let mut seen = HashSet::new();
    let mut addresses = Vec::with_capacity(values.len());

    for value in values {
        let address = parse_address(field, &value)?;
        if !seen.insert(address) {
            return Err(ConfigError::DuplicateAddress { field, value });
        }
        addresses.push(address);
    }

    Ok(addresses)
}

fn parse_address(field: &'static str, value: &str) -> Result<Pubkey, ConfigError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ConfigError::EmptyField { field });
    }
    Pubkey::from_str(value).map_err(|_| ConfigError::InvalidAddress {
        field,
        value: value.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISTRIBUTION: &str = "J3mfHoQb27xHL1xUYsoPfU1vZHbzCeK7fZYvsWeYdoge";
    const TARGET_ONE: &str = "11111111111111111111111111111111";
    const TARGET_TWO: &str = "SysvarRent111111111111111111111111111111111";

    fn valid_config() -> String {
        format!(
            r#"
[cluster]
name = "mainnet-beta"
rpc_url_env = "SOLANA_RPC_URL"

[distribution]
token_address = "{DISTRIBUTION}"
total_amount_ui = "1000.5"

[targeting]
target_token_addresses = ["{TARGET_ONE}", "{TARGET_TWO}"]
max_recipients = 100
manual_exclude_wallets = ["{TARGET_ONE}"]
"#
        )
    }

    #[test]
    fn validates_minimal_config() {
        let config = ConfigFile::from_toml_str(&valid_config())
            .unwrap()
            .validate()
            .unwrap();

        assert_eq!(config.cluster_name, "mainnet-beta");
        assert_eq!(config.rpc_url_env, "SOLANA_RPC_URL");
        assert_eq!(config.target_token_addresses.len(), 2);
        assert_eq!(config.manual_exclude_wallets.len(), 1);
        assert!(!config.solscan.enabled);
        assert_eq!(config.solscan.api_key_env, "SOLSCAN_API_KEY");
    }

    #[test]
    fn rejects_non_mainnet_cluster() {
        let err = ConfigFile::from_toml_str(&valid_config().replace("mainnet-beta", "devnet"))
            .unwrap()
            .validate()
            .unwrap_err();

        assert!(matches!(err, ConfigError::UnsupportedCluster(cluster) if cluster == "devnet"));
    }

    #[test]
    fn rejects_empty_targets() {
        let config = valid_config().replace(
            &format!("target_token_addresses = [\"{TARGET_ONE}\", \"{TARGET_TWO}\"]"),
            "target_token_addresses = []",
        );
        let err = ConfigFile::from_toml_str(&config)
            .unwrap()
            .validate()
            .unwrap_err();

        assert!(
            matches!(err, ConfigError::EmptyField { field } if field == "targeting.target_token_addresses")
        );
    }

    #[test]
    fn rejects_duplicate_targets() {
        let config = valid_config().replace(TARGET_TWO, TARGET_ONE);
        let err = ConfigFile::from_toml_str(&config)
            .unwrap()
            .validate()
            .unwrap_err();

        assert!(
            matches!(err, ConfigError::DuplicateAddress { field, .. } if field == "targeting.target_token_addresses")
        );
    }

    #[test]
    fn rejects_invalid_addresses() {
        let config = valid_config().replace(DISTRIBUTION, "not-a-token-address");
        let err = ConfigFile::from_toml_str(&config)
            .unwrap()
            .validate()
            .unwrap_err();

        assert!(
            matches!(err, ConfigError::InvalidAddress { field, .. } if field == "distribution.token_address")
        );
    }

    #[test]
    fn rejects_zero_amount() {
        let config = valid_config().replace("1000.5", "0.0");
        let err = ConfigFile::from_toml_str(&config)
            .unwrap()
            .validate()
            .unwrap_err();

        assert!(
            matches!(err, ConfigError::InvalidAmount { field } if field == "distribution.total_amount_ui")
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let config = format!("{}\ncommitment = \"confirmed\"\n", valid_config());
        let err = ConfigFile::from_toml_str(&config).unwrap_err();

        assert!(err.to_string().contains("unknown field"));
    }
}
