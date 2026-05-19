use {
    crate::rpc::TokenAccountBalance,
    reqwest::StatusCode,
    serde::Deserialize,
    serde_json::Value,
    solana_pubkey::Pubkey,
    std::{str::FromStr, thread::sleep, time::Duration},
    thiserror::Error,
};

const SOLSCAN_BASE_URL: &str = "https://pro-api.solscan.io";
const TOKEN_HOLDERS_PATH: &str = "/v2.0/token/holders";
const SOLSCAN_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SOLSCAN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SOLSCAN_PAGE_SIZE: usize = 40;
const SOLSCAN_MAX_ATTEMPTS: usize = 4;
const SOLSCAN_INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const SOLSCAN_MAX_RETRY_DELAY: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct SolscanClient {
    api_key: String,
    base_url: String,
    client: reqwest::blocking::Client,
}

impl SolscanClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, SOLSCAN_BASE_URL)
    }

    fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            client: reqwest::blocking::Client::builder()
                .connect_timeout(SOLSCAN_CONNECT_TIMEOUT)
                .timeout(SOLSCAN_REQUEST_TIMEOUT)
                .user_agent(concat!("airdrop/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("valid Solscan HTTP client timeout configuration"),
        }
    }

    pub fn get_token_holders(
        &self,
        token_address: &Pubkey,
        limit: usize,
    ) -> Result<Vec<TokenAccountBalance>, SolscanError> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        eprintln!(
            "Solscan: fetching holders for {} (limit {})",
            token_address,
            display_limit(limit)
        );
        let mut holders = Vec::with_capacity(limit.min(SOLSCAN_PAGE_SIZE));
        let mut page = 1_usize;

        while holders.len() < limit {
            let response = self.request_token_holders_page(token_address, page)?;
            let total = response.total.unwrap_or(0);
            let item_count = response.items.len();

            for item in response.items {
                holders.push(TokenAccountBalance::try_from(item)?);
                if holders.len() == limit {
                    break;
                }
            }

            eprintln!(
                "Solscan: {} page {} returned {} holder(s); collected {}/{}",
                token_address,
                page,
                item_count,
                holders.len(),
                if total == 0 {
                    display_limit(limit)
                } else {
                    total.to_string()
                }
            );

            if item_count == 0 || holders.len() >= total {
                break;
            }

            page += 1;
        }

        Ok(holders)
    }

    pub fn get_all_token_holders(
        &self,
        token_address: &Pubkey,
    ) -> Result<Vec<TokenAccountBalance>, SolscanError> {
        self.get_token_holders(token_address, usize::MAX)
    }

    fn request_token_holders_page(
        &self,
        token_address: &Pubkey,
        page: usize,
    ) -> Result<SolscanTokenHoldersData, SolscanError> {
        let mut retry_delay = SOLSCAN_INITIAL_RETRY_DELAY;
        let url = format!(
            "{}{}?address={token_address}&page={page}&page_size={SOLSCAN_PAGE_SIZE}",
            self.base_url, TOKEN_HOLDERS_PATH
        );

        for attempt in 1..=SOLSCAN_MAX_ATTEMPTS {
            let response = self
                .client
                .get(&url)
                .header("token", &self.api_key)
                .send()
                .map_err(|source| SolscanError::Transport {
                    message: source.to_string(),
                })?;

            let status = response.status();
            if !status.is_success() {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_retry_after);
                let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                let message = response.text().unwrap_or_default();

                if retryable && attempt < SOLSCAN_MAX_ATTEMPTS {
                    let delay = retry_after
                        .unwrap_or(retry_delay)
                        .min(SOLSCAN_MAX_RETRY_DELAY);
                    sleep(delay);
                    retry_delay = (retry_delay * 2).min(SOLSCAN_MAX_RETRY_DELAY);
                    continue;
                }

                return Err(SolscanError::HttpStatus {
                    status: status.as_u16(),
                    attempts: attempt,
                    message,
                });
            }

            let envelope: SolscanTokenHoldersEnvelope =
                response.json().map_err(|source| SolscanError::Decode {
                    message: source.to_string(),
                })?;

            if !envelope.success {
                return Err(SolscanError::Remote {
                    message: envelope.error_message(),
                });
            }

            return envelope.data.ok_or(SolscanError::MissingData);
        }

        unreachable!("Solscan retry loop always returns before exhausting attempts")
    }
}

impl std::fmt::Debug for SolscanClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SolscanClient")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub enum SolscanError {
    #[error("Solscan transport error: {message}")]
    Transport { message: String },
    #[error("Solscan HTTP status {status} after {attempts} attempt(s): {message}")]
    HttpStatus {
        status: u16,
        attempts: usize,
        message: String,
    },
    #[error("Solscan response decode error: {message}")]
    Decode { message: String },
    #[error("Solscan request failed: {message}")]
    Remote { message: String },
    #[error("Solscan response did not include holder data")]
    MissingData,
    #[error("Solscan returned invalid holder public key `{value}` in {field}")]
    InvalidPubkey { field: &'static str, value: String },
    #[error("Solscan returned invalid holder amount `{value}`")]
    InvalidAmount { value: String },
    #[error("Solscan returned invalid holder decimals `{value}`")]
    InvalidDecimals { value: u64 },
}

#[derive(Debug, Deserialize)]
struct SolscanTokenHoldersEnvelope {
    success: bool,
    data: Option<SolscanTokenHoldersData>,
    errors: Option<Value>,
    error_message: Option<String>,
}

impl SolscanTokenHoldersEnvelope {
    fn error_message(self) -> String {
        self.error_message
            .or_else(|| self.errors.map(|errors| errors.to_string()))
            .unwrap_or_else(|| "unknown Solscan error".to_owned())
    }
}

#[derive(Debug, Deserialize)]
struct SolscanTokenHoldersData {
    total: Option<usize>,
    #[serde(default)]
    items: Vec<SolscanHolderItem>,
}

#[derive(Debug, Deserialize)]
struct SolscanHolderItem {
    address: String,
    amount: Value,
    decimals: u64,
    owner: String,
}

impl TryFrom<SolscanHolderItem> for TokenAccountBalance {
    type Error = SolscanError;

    fn try_from(value: SolscanHolderItem) -> Result<Self, Self::Error> {
        let decimals = u8::try_from(value.decimals).map_err(|_| SolscanError::InvalidDecimals {
            value: value.decimals,
        })?;
        let amount = parse_amount(&value.amount)?;

        Ok(Self {
            token_account: parse_pubkey("token/holders.items.address", &value.address)?,
            owner_wallet: Some(parse_pubkey("token/holders.items.owner", &value.owner)?),
            ui_amount_string: format_ui_amount(&amount, decimals),
            amount,
            decimals,
        })
    }
}

fn parse_amount(value: &Value) -> Result<String, SolscanError> {
    let amount = match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => {
            return Err(SolscanError::InvalidAmount {
                value: value.to_string(),
            });
        }
    };

    if amount.is_empty() || !amount.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(SolscanError::InvalidAmount { value: amount });
    }

    Ok(amount)
}

fn format_ui_amount(raw_amount: &str, decimals: u8) -> String {
    let decimals = usize::from(decimals);
    if decimals == 0 {
        return raw_amount.to_owned();
    }

    let amount = raw_amount.trim_start_matches('0');
    let amount = if amount.is_empty() { "0" } else { amount };

    let (whole, fractional) = if amount.len() > decimals {
        let split_at = amount.len() - decimals;
        (amount[..split_at].to_owned(), amount[split_at..].to_owned())
    } else {
        let mut fractional = "0".repeat(decimals - amount.len());
        fractional.push_str(amount);
        ("0".to_owned(), fractional)
    };

    let fractional = fractional.trim_end_matches('0');
    if fractional.is_empty() {
        whole
    } else {
        format!("{whole}.{fractional}")
    }
}

fn parse_pubkey(field: &'static str, value: &str) -> Result<Pubkey, SolscanError> {
    Pubkey::from_str(value).map_err(|_| SolscanError::InvalidPubkey {
        field,
        value: value.to_owned(),
    })
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    value
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .filter(|duration| !duration.is_zero())
}

fn display_limit(limit: usize) -> String {
    if limit == usize::MAX {
        "all".to_owned()
    } else {
        limit.to_string()
    }
}

#[cfg(test)]
mod tests {
    use {super::*, serde_json::json};

    #[test]
    fn parses_holder_item_with_string_amount() {
        let item = SolscanHolderItem {
            address: "11111111111111111111111111111111".to_owned(),
            amount: json!("123400"),
            decimals: 3,
            owner: "SysvarRent111111111111111111111111111111111".to_owned(),
        };

        let balance = TokenAccountBalance::try_from(item).unwrap();

        assert_eq!(
            balance.owner_wallet.unwrap().to_string(),
            "SysvarRent111111111111111111111111111111111"
        );
        assert_eq!(balance.amount, "123400");
        assert_eq!(balance.decimals, 3);
        assert_eq!(balance.ui_amount_string, "123.4");
    }

    #[test]
    fn parses_holder_item_with_integer_amount() {
        let item = SolscanHolderItem {
            address: "11111111111111111111111111111111".to_owned(),
            amount: json!(42),
            decimals: 6,
            owner: "SysvarRent111111111111111111111111111111111".to_owned(),
        };

        let balance = TokenAccountBalance::try_from(item).unwrap();

        assert_eq!(balance.amount, "42");
        assert_eq!(balance.ui_amount_string, "0.000042");
    }

    #[test]
    fn rejects_fractional_holder_amounts() {
        let item = SolscanHolderItem {
            address: "11111111111111111111111111111111".to_owned(),
            amount: json!(1.5),
            decimals: 6,
            owner: "SysvarRent111111111111111111111111111111111".to_owned(),
        };

        let err = TokenAccountBalance::try_from(item).unwrap_err();

        assert!(matches!(err, SolscanError::InvalidAmount { .. }));
    }

    #[test]
    fn redacts_api_key_in_debug() {
        let client = SolscanClient::with_base_url("secret-key", "https://example.invalid");

        let debug = format!("{client:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-key"));
    }
}
