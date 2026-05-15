use {
    serde::{Deserialize, Serialize, de::DeserializeOwned},
    serde_json::{Value, json},
    solana_pubkey::Pubkey,
    std::str::FromStr,
    thiserror::Error,
};

const CONFIRMED_COMMITMENT: &str = "confirmed";

pub trait RpcReader {
    fn get_account(&self, address: &Pubkey) -> Result<Option<RpcAccount>, RpcError>;
    fn get_token_largest_accounts(
        &self,
        mint: &Pubkey,
    ) -> Result<Vec<TokenAccountBalance>, RpcError>;
    fn get_token_accounts_by_owner(
        &self,
        owner: &Pubkey,
        mint: &Pubkey,
    ) -> Result<Vec<RpcTokenAccount>, RpcError>;
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
            client: reqwest::blocking::Client::new(),
        }
    }

    fn request<T: DeserializeOwned>(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RpcValue<T>, RpcError> {
        let request = RpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method,
            params,
        };
        let response: RpcEnvelope<RpcValue<T>> = self
            .client
            .post(&self.endpoint)
            .json(&request)
            .send()
            .map_err(|source| RpcError::Transport {
                method,
                message: source.to_string(),
            })?
            .error_for_status()
            .map_err(|source| RpcError::Transport {
                method,
                message: source.to_string(),
            })?
            .json()
            .map_err(|source| RpcError::Decode {
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

        response.result.ok_or(RpcError::MissingResult { method })
    }
}

impl RpcReader for HttpRpcClient {
    fn get_account(&self, address: &Pubkey) -> Result<Option<RpcAccount>, RpcError> {
        let response = self.request::<Option<JsonAccount>>(
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

    fn get_token_largest_accounts(
        &self,
        mint: &Pubkey,
    ) -> Result<Vec<TokenAccountBalance>, RpcError> {
        let response = self.request::<Vec<JsonTokenAccountBalance>>(
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
        let response = self.request::<Vec<JsonTokenAccountWithPubkey>>(
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
    pub amount: String,
    pub decimals: u8,
    pub ui_amount_string: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcTokenAccount {
    pub token_account: Pubkey,
    pub account: RpcAccount,
}

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("RPC transport error calling {method}: {message}")]
    Transport {
        method: &'static str,
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
    #[error("RPC returned invalid account `{address}`: {source}")]
    InvalidAccount {
        address: Pubkey,
        source: ParseRpcAccountError,
    },
    #[error("RPC returned invalid public key `{value}` in {field}")]
    InvalidPubkey { field: &'static str, value: String },
    #[error("RPC returned invalid token amount `{value}` in {field}")]
    InvalidAmount { field: &'static str, value: String },
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

    fn token_program_id() -> Pubkey {
        Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap()
    }
}
