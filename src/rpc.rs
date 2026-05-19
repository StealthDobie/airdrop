use {
    serde::{Deserialize, Serialize, de::DeserializeOwned},
    serde_json::{Value, json},
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    std::{str::FromStr, thread::sleep, time::Duration},
    thiserror::Error,
};

const CONFIRMED_COMMITMENT: &str = "confirmed";
const RPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const RPC_MAX_ATTEMPTS: usize = 6;
const RPC_INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const RPC_MAX_RETRY_DELAY: Duration = Duration::from_secs(15);
const RPC_GET_MULTIPLE_ACCOUNTS_LIMIT: usize = 100;

pub trait RpcReader {
    fn get_account(&self, address: &Pubkey) -> Result<Option<RpcAccount>, RpcError>;
    fn get_multiple_accounts(
        &self,
        addresses: &[Pubkey],
    ) -> Result<Vec<Option<RpcAccount>>, RpcError> {
        addresses
            .iter()
            .map(|address| self.get_account(address))
            .collect()
    }
    fn get_token_largest_accounts(
        &self,
        mint: &Pubkey,
    ) -> Result<Vec<TokenAccountBalance>, RpcError>;
    fn get_token_accounts_by_owner(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
    ) -> Result<Vec<RpcTokenAccount>, RpcError>;
    fn get_minimum_balance_for_rent_exemption(&self, data_len: usize) -> Result<u64, RpcError>;
}

pub trait RpcSimulator {
    fn get_latest_blockhash(&self) -> Result<Hash, RpcError>;
    fn simulate_transaction(
        &self,
        encoded_transaction: &str,
    ) -> Result<TransactionSimulation, RpcError>;
}

pub trait RpcSender {
    fn get_balance(&self, address: &Pubkey) -> Result<u64, RpcError>;
    fn send_transaction(&self, encoded_transaction: &str) -> Result<String, RpcError>;
    fn get_signature_status(&self, signature: &str) -> Result<Option<SignatureStatus>, RpcError>;
}

#[derive(Debug, Clone)]
pub struct HttpRpcClient {
    endpoint: String,
    client: reqwest::blocking::Client,
}

impl HttpRpcClient {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            client: reqwest::blocking::Client::builder()
                .connect_timeout(RPC_CONNECT_TIMEOUT)
                .timeout(RPC_REQUEST_TIMEOUT)
                .build()
                .expect("valid RPC HTTP client timeout configuration"),
        }
    }

    fn request_context_value<T: DeserializeOwned>(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RpcValue<T>, RpcError> {
        self.request(method, params)
    }

    fn request<T: DeserializeOwned>(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<T, RpcError> {
        let request = RpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method,
            params,
        };
        let mut retry_delay = RPC_INITIAL_RETRY_DELAY;

        for attempt in 1..=RPC_MAX_ATTEMPTS {
            let response = self
                .client
                .post(&self.endpoint)
                .json(&request)
                .send()
                .map_err(|source| RpcError::Transport {
                    method,
                    message: source.to_string(),
                })?;

            let status = response.status();
            if !status.is_success() {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_retry_after);
                let retryable = status.as_u16() == 429 || status.is_server_error();
                let message = response.text().unwrap_or_default();

                if retryable && attempt < RPC_MAX_ATTEMPTS {
                    let delay = retry_after.unwrap_or(retry_delay).min(RPC_MAX_RETRY_DELAY);
                    sleep(delay);
                    retry_delay = (retry_delay * 2).min(RPC_MAX_RETRY_DELAY);
                    continue;
                }

                return Err(RpcError::HttpStatus {
                    method,
                    status: status.as_u16(),
                    attempts: attempt,
                    message,
                });
            }

            let response: RpcEnvelope<T> = response.json().map_err(|source| RpcError::Decode {
                method,
                message: source.to_string(),
            })?;

            if let Some(error) = response.error {
                return Err(RpcError::Remote {
                    method,
                    code: error.code,
                    message: error.message,
                });
            }

            return response.result.ok_or(RpcError::MissingResult { method });
        }

        unreachable!("RPC retry loop always returns before exhausting attempts")
    }
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    value
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .filter(|duration| !duration.is_zero())
}

impl RpcReader for HttpRpcClient {
    fn get_account(&self, address: &Pubkey) -> Result<Option<RpcAccount>, RpcError> {
        let response = self.request_context_value::<Option<JsonAccount>>(
            "getAccountInfo",
            json!([
                address.to_string(),
                {
                    "encoding": "jsonParsed",
                    "commitment": CONFIRMED_COMMITMENT
                }
            ]),
        )?;

        response
            .value
            .map(RpcAccount::try_from)
            .transpose()
            .map_err(|source| RpcError::InvalidAccount {
                address: *address,
                source,
            })
    }

    fn get_multiple_accounts(
        &self,
        addresses: &[Pubkey],
    ) -> Result<Vec<Option<RpcAccount>>, RpcError> {
        let mut accounts = Vec::with_capacity(addresses.len());

        for chunk in addresses.chunks(RPC_GET_MULTIPLE_ACCOUNTS_LIMIT) {
            let response = self.request_context_value::<Vec<Option<JsonAccount>>>(
                "getMultipleAccounts",
                json!([
                    chunk.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    {
                        "encoding": "jsonParsed",
                        "commitment": CONFIRMED_COMMITMENT
                    }
                ]),
            )?;

            if response.value.len() != chunk.len() {
                return Err(RpcError::InvalidResponseLength {
                    method: "getMultipleAccounts",
                    expected: chunk.len(),
                    actual: response.value.len(),
                });
            }

            for (address, account) in chunk.iter().zip(response.value) {
                accounts.push(
                    account
                        .map(RpcAccount::try_from)
                        .transpose()
                        .map_err(|source| RpcError::InvalidAccount {
                            address: *address,
                            source,
                        })?,
                );
            }
        }

        Ok(accounts)
    }

    fn get_token_largest_accounts(
        &self,
        mint: &Pubkey,
    ) -> Result<Vec<TokenAccountBalance>, RpcError> {
        let response = self.request_context_value::<Vec<JsonTokenAccountBalance>>(
            "getTokenLargestAccounts",
            json!([
                mint.to_string(),
                {
                    "commitment": CONFIRMED_COMMITMENT
                }
            ]),
        )?;

        response
            .value
            .into_iter()
            .map(TokenAccountBalance::try_from)
            .collect()
    }

    fn get_token_accounts_by_owner(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
    ) -> Result<Vec<RpcTokenAccount>, RpcError> {
        let response = self.request_context_value::<Vec<JsonTokenAccountWithPubkey>>(
            "getTokenAccountsByOwner",
            json!([
                owner.to_string(),
                {
                    "mint": mint.to_string()
                },
                {
                    "encoding": "jsonParsed",
                    "commitment": CONFIRMED_COMMITMENT
                }
            ]),
        )?;

        response
            .value
            .into_iter()
            .map(RpcTokenAccount::try_from)
            .collect()
    }

    fn get_minimum_balance_for_rent_exemption(&self, data_len: usize) -> Result<u64, RpcError> {
        self.request(
            "getMinimumBalanceForRentExemption",
            json!([
                data_len,
                {
                    "commitment": CONFIRMED_COMMITMENT
                }
            ]),
        )
    }
}

impl RpcSimulator for HttpRpcClient {
    fn get_latest_blockhash(&self) -> Result<Hash, RpcError> {
        let response = self.request_context_value::<JsonLatestBlockhash>(
            "getLatestBlockhash",
            json!([
                {
                    "commitment": CONFIRMED_COMMITMENT
                }
            ]),
        )?;

        Hash::from_str(&response.value.blockhash).map_err(|_| RpcError::InvalidBlockhash {
            value: response.value.blockhash,
        })
    }

    fn simulate_transaction(
        &self,
        encoded_transaction: &str,
    ) -> Result<TransactionSimulation, RpcError> {
        let response = self.request_context_value::<JsonTransactionSimulation>(
            "simulateTransaction",
            json!([encoded_transaction, simulation_request_config()]),
        )?;

        Ok(TransactionSimulation {
            err: response.value.err,
            logs: response.value.logs.unwrap_or_default(),
            units_consumed: response.value.units_consumed,
        })
    }
}

impl RpcSender for HttpRpcClient {
    fn get_balance(&self, address: &Pubkey) -> Result<u64, RpcError> {
        let response = self.request_context_value::<u64>(
            "getBalance",
            json!([
                address.to_string(),
                {
                    "commitment": CONFIRMED_COMMITMENT
                }
            ]),
        )?;

        Ok(response.value)
    }

    fn send_transaction(&self, encoded_transaction: &str) -> Result<String, RpcError> {
        self.request(
            "sendTransaction",
            json!([encoded_transaction, send_transaction_request_config()]),
        )
    }

    fn get_signature_status(&self, signature: &str) -> Result<Option<SignatureStatus>, RpcError> {
        let response = self.request_context_value::<Vec<Option<JsonSignatureStatus>>>(
            "getSignatureStatuses",
            json!([
                [signature],
                {
                    "searchTransactionHistory": true
                }
            ]),
        )?;

        if response.value.len() != 1 {
            return Err(RpcError::InvalidResponseLength {
                method: "getSignatureStatuses",
                expected: 1,
                actual: response.value.len(),
            });
        }

        Ok(response
            .value
            .into_iter()
            .next()
            .flatten()
            .map(SignatureStatus::from))
    }
}

fn simulation_request_config() -> Value {
    json!({
        "encoding": "base64",
        "commitment": CONFIRMED_COMMITMENT,
        "sigVerify": false,
        "replaceRecentBlockhash": true
    })
}

fn send_transaction_request_config() -> Value {
    json!({
        "encoding": "base64",
        "skipPreflight": false,
        "preflightCommitment": CONFIRMED_COMMITMENT
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcAccount {
    pub owner_program: Pubkey,
    pub executable: bool,
    pub parsed: Option<ParsedAccount>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedAccount {
    Mint {
        decimals: u8,
        supply: String,
    },
    TokenAccount {
        mint: Pubkey,
        owner: Pubkey,
        amount: String,
        decimals: u8,
    },
    Other {
        account_type: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenAccountBalance {
    pub token_account: Pubkey,
    pub owner_wallet: Option<Pubkey>,
    pub amount: String,
    pub decimals: u8,
    pub ui_amount_string: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcTokenAccount {
    pub token_account: Pubkey,
    pub account: RpcAccount,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransactionSimulation {
    pub err: Option<Value>,
    pub logs: Vec<String>,
    pub units_consumed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureStatus {
    pub slot: u64,
    pub status: Option<Value>,
    pub confirmation_status: Option<String>,
    pub err: Option<Value>,
}

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("RPC transport error calling {method}: {message}")]
    Transport {
        method: &'static str,
        message: String,
    },
    #[error("RPC HTTP status {status} calling {method} after {attempts} attempt(s): {message}")]
    HttpStatus {
        method: &'static str,
        status: u16,
        attempts: usize,
        message: String,
    },
    #[error("RPC response decode error calling {method}: {message}")]
    Decode {
        method: &'static str,
        message: String,
    },
    #[error("RPC {method} failed with code {code}: {message}")]
    Remote {
        method: &'static str,
        code: i64,
        message: String,
    },
    #[error("RPC {method} returned no result")]
    MissingResult { method: &'static str },
    #[error("RPC {method} returned {actual} result(s), expected {expected}")]
    InvalidResponseLength {
        method: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("RPC returned invalid account `{address}`: {source}")]
    InvalidAccount {
        address: Pubkey,
        source: ParseRpcAccountError,
    },
    #[error("RPC returned invalid public key `{value}` in {field}")]
    InvalidPubkey { field: &'static str, value: String },
    #[error("RPC returned invalid token amount `{value}` in {field}")]
    InvalidAmount { field: &'static str, value: String },
    #[error("RPC returned invalid latest blockhash `{value}`")]
    InvalidBlockhash { value: String },
}

#[derive(Debug, Error)]
pub enum ParseRpcAccountError {
    #[error("invalid owner program `{0}`")]
    InvalidOwner(String),
    #[error("parsed account is missing info")]
    MissingInfo,
    #[error("parsed mint is missing decimals")]
    MissingMintDecimals,
    #[error("parsed mint is missing supply")]
    MissingMintSupply,
    #[error("parsed token account is missing mint")]
    MissingTokenMint,
    #[error("parsed token account is missing owner")]
    MissingTokenOwner,
    #[error("parsed token account is missing amount")]
    MissingTokenAmount,
    #[error("parsed token account is missing decimals")]
    MissingTokenDecimals,
    #[error("invalid public key `{value}` in {field}")]
    InvalidPubkey { field: &'static str, value: String },
}

#[derive(Debug, Serialize)]
struct RpcRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: Value,
}

#[derive(Debug, Deserialize)]
struct RpcEnvelope<T> {
    result: Option<T>,
    error: Option<RpcRemoteError>,
}

#[derive(Debug, Deserialize)]
struct RpcRemoteError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct RpcValue<T> {
    value: T,
}

#[derive(Debug, Deserialize)]
struct JsonAccount {
    owner: String,
    executable: bool,
    data: JsonAccountData,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JsonAccountData {
    Parsed {
        parsed: JsonParsedAccount,
    },
    #[allow(dead_code)]
    Raw(Value),
}

#[derive(Debug, Deserialize)]
struct JsonParsedAccount {
    #[serde(rename = "type")]
    account_type: String,
    info: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct JsonTokenAccountBalance {
    address: String,
    amount: String,
    decimals: u8,
    #[serde(rename = "uiAmountString")]
    ui_amount_string: String,
}

#[derive(Debug, Deserialize)]
struct JsonTokenAccountWithPubkey {
    pubkey: String,
    account: JsonAccount,
}

#[derive(Debug, Deserialize)]
struct JsonLatestBlockhash {
    blockhash: String,
}

#[derive(Debug, Deserialize)]
struct JsonTransactionSimulation {
    err: Option<Value>,
    logs: Option<Vec<String>>,
    #[serde(rename = "unitsConsumed")]
    units_consumed: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct JsonSignatureStatus {
    slot: u64,
    err: Option<Value>,
    status: Option<Value>,
    #[serde(rename = "confirmationStatus")]
    confirmation_status: Option<String>,
}

impl From<JsonSignatureStatus> for SignatureStatus {
    fn from(value: JsonSignatureStatus) -> Self {
        Self {
            slot: value.slot,
            status: value.status,
            confirmation_status: value.confirmation_status,
            err: value.err,
        }
    }
}

impl TryFrom<JsonAccount> for RpcAccount {
    type Error = ParseRpcAccountError;

    fn try_from(value: JsonAccount) -> Result<Self, Self::Error> {
        let owner_program = parse_pubkey("account.owner", &value.owner)
            .map_err(|_| ParseRpcAccountError::InvalidOwner(value.owner.clone()))?;
        let parsed = match value.data {
            JsonAccountData::Parsed { parsed } => Some(ParsedAccount::try_from(parsed)?),
            JsonAccountData::Raw(_) => None,
        };

        Ok(Self {
            owner_program,
            executable: value.executable,
            parsed,
        })
    }
}

impl TryFrom<JsonParsedAccount> for ParsedAccount {
    type Error = ParseRpcAccountError;

    fn try_from(value: JsonParsedAccount) -> Result<Self, Self::Error> {
        let Some(info) = value.info else {
            return Err(ParseRpcAccountError::MissingInfo);
        };

        match value.account_type.as_str() {
            "mint" => Ok(Self::Mint {
                decimals: json_u8(&info, "decimals")
                    .ok_or(ParseRpcAccountError::MissingMintDecimals)?,
                supply: json_string(&info, "supply")
                    .ok_or(ParseRpcAccountError::MissingMintSupply)?,
            }),
            "account" => {
                let token_amount = info
                    .get("tokenAmount")
                    .ok_or(ParseRpcAccountError::MissingTokenAmount)?;
                Ok(Self::TokenAccount {
                    mint: parse_pubkey_value("tokenAccount.mint", &info, "mint")
                        .map_err(|_| ParseRpcAccountError::MissingTokenMint)?,
                    owner: parse_pubkey_value("tokenAccount.owner", &info, "owner")
                        .map_err(|_| ParseRpcAccountError::MissingTokenOwner)?,
                    amount: json_string(token_amount, "amount")
                        .ok_or(ParseRpcAccountError::MissingTokenAmount)?,
                    decimals: json_u8(token_amount, "decimals")
                        .ok_or(ParseRpcAccountError::MissingTokenDecimals)?,
                })
            }
            _ => Ok(Self::Other {
                account_type: value.account_type,
            }),
        }
    }
}

impl TryFrom<JsonTokenAccountBalance> for TokenAccountBalance {
    type Error = RpcError;

    fn try_from(value: JsonTokenAccountBalance) -> Result<Self, Self::Error> {
        if parse_raw_amount(&value.amount).is_none() {
            return Err(RpcError::InvalidAmount {
                field: "getTokenLargestAccounts.amount",
                value: value.amount,
            });
        }

        Ok(Self {
            token_account: parse_pubkey("getTokenLargestAccounts.address", &value.address)?,
            owner_wallet: None,
            amount: value.amount,
            decimals: value.decimals,
            ui_amount_string: value.ui_amount_string,
        })
    }
}

impl TryFrom<JsonTokenAccountWithPubkey> for RpcTokenAccount {
    type Error = RpcError;

    fn try_from(value: JsonTokenAccountWithPubkey) -> Result<Self, Self::Error> {
        let token_account = parse_pubkey("getTokenAccountsByOwner.pubkey", &value.pubkey)?;
        let account =
            RpcAccount::try_from(value.account).map_err(|source| RpcError::InvalidAccount {
                address: token_account,
                source,
            })?;
        Ok(Self {
            token_account,
            account,
        })
    }
}

pub fn parse_raw_amount(value: &str) -> Option<u128> {
    if value.is_empty() || !value.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn parse_pubkey(field: &'static str, value: &str) -> Result<Pubkey, RpcError> {
    Pubkey::from_str(value).map_err(|_| RpcError::InvalidPubkey {
        field,
        value: value.to_owned(),
    })
}

fn parse_pubkey_value(
    field: &'static str,
    info: &Value,
    key: &'static str,
) -> Result<Pubkey, ParseRpcAccountError> {
    let value = json_string(info, key).ok_or(ParseRpcAccountError::InvalidPubkey {
        field,
        value: String::new(),
    })?;
    Pubkey::from_str(&value).map_err(|_| ParseRpcAccountError::InvalidPubkey { field, value })
}

fn json_string(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(ToOwned::to_owned)
}

fn json_u8(value: &Value, key: &str) -> Option<u8> {
    value.get(key)?.as_u64()?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use {super::*, serde_json::json};

    #[test]
    fn parses_json_token_account() {
        let account = JsonAccount {
            owner: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_owned(),
            executable: false,
            data: JsonAccountData::Parsed {
                parsed: JsonParsedAccount {
                    account_type: "account".to_owned(),
                    info: Some(json!({
                        "mint": "11111111111111111111111111111111",
                        "owner": "SysvarRent111111111111111111111111111111111",
                        "tokenAmount": {
                            "amount": "42",
                            "decimals": 6
                        }
                    })),
                },
            },
        };

        let account = RpcAccount::try_from(account).unwrap();

        assert_eq!(
            account.owner_program.to_string(),
            token_program_id().to_string()
        );
        assert!(matches!(
            account.parsed,
            Some(ParsedAccount::TokenAccount { amount, decimals, .. })
                if amount == "42" && decimals == 6
        ));
    }

    #[test]
    fn rejects_invalid_raw_amounts() {
        assert_eq!(parse_raw_amount("0"), Some(0));
        assert_eq!(parse_raw_amount("123"), Some(123));
        assert_eq!(parse_raw_amount(""), None);
        assert_eq!(parse_raw_amount("1.23"), None);
    }

    #[test]
    fn constructs_http_client_with_fixed_timeouts() {
        let client = HttpRpcClient::new("https://api.mainnet-beta.solana.com");

        assert_eq!(client.endpoint, "https://api.mainnet-beta.solana.com");
        assert_eq!(RPC_CONNECT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(RPC_REQUEST_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn parses_retry_after_seconds() {
        assert_eq!(parse_retry_after("1"), Some(Duration::from_secs(1)));
        assert_eq!(parse_retry_after("15"), Some(Duration::from_secs(15)));
        assert_eq!(parse_retry_after("0"), None);
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    }

    #[test]
    fn parses_transaction_simulation_result() {
        let response: RpcValue<JsonTransactionSimulation> = serde_json::from_value(json!({
            "context": {
                "slot": 123
            },
            "value": {
                "err": null,
                "logs": ["Program log: ok"],
                "unitsConsumed": 42
            }
        }))
        .unwrap();

        assert_eq!(response.value.err, None);
        assert_eq!(
            response.value.logs,
            Some(vec!["Program log: ok".to_owned()])
        );
        assert_eq!(response.value.units_consumed, Some(42));
    }

    #[test]
    fn simulation_request_replaces_recent_blockhash() {
        let config = simulation_request_config();

        assert_eq!(config["encoding"], "base64");
        assert_eq!(config["commitment"], "confirmed");
        assert_eq!(config["sigVerify"], false);
        assert_eq!(config["replaceRecentBlockhash"], true);
    }

    #[test]
    fn send_transaction_request_uses_confirmed_preflight() {
        let config = send_transaction_request_config();

        assert_eq!(config["encoding"], "base64");
        assert_eq!(config["skipPreflight"], false);
        assert_eq!(config["preflightCommitment"], "confirmed");
    }

    #[test]
    fn parses_signature_status() {
        let response: RpcValue<Vec<Option<JsonSignatureStatus>>> = serde_json::from_value(json!({
            "context": {
                "slot": 123
            },
            "value": [{
                "slot": 120,
                "confirmations": 1,
                "confirmationStatus": "confirmed",
                "err": null
            }]
        }))
        .unwrap();

        let status = SignatureStatus::from(response.value.into_iter().next().unwrap().unwrap());

        assert_eq!(status.slot, 120);
        assert_eq!(status.confirmation_status.as_deref(), Some("confirmed"));
        assert_eq!(status.err, None);
    }

    #[test]
    fn parses_latest_blockhash_result() {
        let response: RpcValue<JsonLatestBlockhash> = serde_json::from_value(json!({
            "context": {
                "slot": 123
            },
            "value": {
                "blockhash": "11111111111111111111111111111111",
                "lastValidBlockHeight": 321
            }
        }))
        .unwrap();

        assert_eq!(
            Hash::from_str(&response.value.blockhash).unwrap(),
            Hash::default()
        );
    }

    fn token_program_id() -> Pubkey {
        Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap()
    }
}
