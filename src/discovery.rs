use {
    crate::{
        config::ValidatedConfig,
        rpc::{ParsedAccount, RpcError, RpcReader, TokenAccountBalance, parse_raw_amount},
    },
    solana_pubkey::Pubkey,
    std::{collections::BTreeMap, str::FromStr},
    thiserror::Error,
};

pub fn discover_holders(
    rpc: &impl RpcReader,
    config: &ValidatedConfig,
    source_wallet: &Pubkey,
) -> Result<DiscoveryReport, DiscoveryError> {
    let distribution_token = validate_original_spl_mint(rpc, &config.distribution_token_address)?;
    let target_tokens = config
        .target_token_addresses
        .iter()
        .map(|token_address| validate_original_spl_mint(rpc, token_address))
        .collect::<Result<Vec<_>, _>>()?;

    let mut recipients = BTreeMap::<Pubkey, DiscoveredRecipient>::new();
    let mut skipped = Vec::new();

    for target_token in &target_tokens {
        let largest = rpc.get_token_largest_accounts(&target_token.token_address)?;
        for (index, balance) in largest.into_iter().enumerate() {
            let rank = index + 1;
            match verify_candidate_token_account(rpc, &target_token.token_address, balance, rank) {
                Ok(candidate) => {
                    if let Some(reason) = exclusion_reason(
                        rpc,
                        &candidate.owner_wallet,
                        source_wallet,
                        &config.manual_exclude_wallets,
                        &config.distribution_token_address,
                    )? {
                        skipped.push(SkippedCandidate::from_candidate(candidate, reason));
                        continue;
                    }

                    recipients
                        .entry(candidate.owner_wallet)
                        .and_modify(|recipient| {
                            recipient.best_rank = recipient.best_rank.min(candidate.rank);
                            recipient.holdings.push(candidate.holding.clone());
                        })
                        .or_insert_with(|| DiscoveredRecipient {
                            wallet: candidate.owner_wallet,
                            best_rank: candidate.rank,
                            holdings: vec![candidate.holding],
                        });
                }
                Err(skip) => skipped.push(skip),
            }
        }
    }

    let mut recipients: Vec<_> = recipients.into_values().collect();
    recipients.sort_by(|left, right| {
        left.best_rank
            .cmp(&right.best_rank)
            .then_with(|| left.wallet.to_string().cmp(&right.wallet.to_string()))
    });

    if recipients.len() > config.max_recipients {
        let over_limit = recipients.split_off(config.max_recipients);
        for recipient in over_limit {
            skipped.extend(
                recipient
                    .holdings
                    .into_iter()
                    .map(|holding| SkippedCandidate {
                        target_token_address: holding.target_token_address,
                        token_account: holding.token_account,
                        owner_wallet: Some(recipient.wallet),
                        rank: holding.rank,
                        reason: SkipReason::RecipientLimit,
                    }),
            );
        }
    }

    Ok(DiscoveryReport {
        distribution_token,
        target_tokens,
        recipients,
        skipped,
    })
}

pub fn validate_original_spl_mint(
    rpc: &impl RpcReader,
    token_address: &Pubkey,
) -> Result<TokenMetadata, DiscoveryError> {
    let account = rpc
        .get_account(token_address)?
        .ok_or(DiscoveryError::MissingTokenAccount {
            token_address: *token_address,
        })?;

    if account.executable {
        return Err(DiscoveryError::TokenAccountIsExecutable {
            token_address: *token_address,
        });
    }

    let token_2022_program = token_2022_program_id();
    if account.owner_program == token_2022_program {
        return Err(DiscoveryError::UnsupportedToken2022 {
            token_address: *token_address,
        });
    }

    let token_program = token_program_id();
    if account.owner_program != token_program {
        return Err(DiscoveryError::UnsupportedTokenProgram {
            token_address: *token_address,
            owner_program: account.owner_program,
        });
    }

    match account.parsed {
        Some(ParsedAccount::Mint { decimals, supply }) => Ok(TokenMetadata {
            token_address: *token_address,
            token_program,
            decimals,
            supply,
        }),
        _ => Err(DiscoveryError::TokenAccountIsNotMint {
            token_address: *token_address,
        }),
    }
}

fn verify_candidate_token_account(
    rpc: &impl RpcReader,
    expected_target_token: &Pubkey,
    balance: TokenAccountBalance,
    rank: usize,
) -> Result<VerifiedCandidate, SkippedCandidate> {
    if parse_raw_amount(&balance.amount).unwrap_or(0) == 0 {
        return Err(SkippedCandidate::from_balance(
            expected_target_token,
            &balance,
            rank,
            SkipReason::ZeroBalance,
        ));
    }

    let Some(account) = rpc.get_account(&balance.token_account).map_err(|_| {
        SkippedCandidate::from_balance(
            expected_target_token,
            &balance,
            rank,
            SkipReason::TokenAccountLookupFailed,
        )
    })?
    else {
        return Err(SkippedCandidate::from_balance(
            expected_target_token,
            &balance,
            rank,
            SkipReason::MissingTokenAccount,
        ));
    };

    if account.executable {
        return Err(SkippedCandidate::from_balance(
            expected_target_token,
            &balance,
            rank,
            SkipReason::ExecutableTokenAccount,
        ));
    }

    if account.owner_program != token_program_id() {
        return Err(SkippedCandidate::from_balance(
            expected_target_token,
            &balance,
            rank,
            SkipReason::UnsupportedTokenProgram,
        ));
    }

    let Some(ParsedAccount::TokenAccount {
        mint,
        owner,
        amount,
        decimals,
    }) = account.parsed
    else {
        return Err(SkippedCandidate::from_balance(
            expected_target_token,
            &balance,
            rank,
            SkipReason::MalformedTokenAccount,
        ));
    };

    if mint != *expected_target_token {
        return Err(SkippedCandidate::from_balance(
            expected_target_token,
            &balance,
            rank,
            SkipReason::TokenMintMismatch,
        ));
    }

    if parse_raw_amount(&amount).unwrap_or(0) == 0 {
        return Err(SkippedCandidate::from_balance(
            expected_target_token,
            &balance,
            rank,
            SkipReason::ZeroBalance,
        ));
    }

    Ok(VerifiedCandidate {
        owner_wallet: owner,
        rank,
        holding: TargetHolding {
            target_token_address: *expected_target_token,
            token_account: balance.token_account,
            raw_amount: amount,
            decimals,
            rank,
        },
    })
}

fn exclusion_reason(
    rpc: &impl RpcReader,
    owner_wallet: &Pubkey,
    source_wallet: &Pubkey,
    manual_exclude_wallets: &[Pubkey],
    distribution_token: &Pubkey,
) -> Result<Option<SkipReason>, DiscoveryError> {
    if owner_wallet == source_wallet {
        return Ok(Some(SkipReason::SourceWallet));
    }

    if manual_exclude_wallets.contains(owner_wallet) {
        return Ok(Some(SkipReason::ManualExclude));
    }

    if !owner_wallet.is_on_curve() {
        return Ok(Some(SkipReason::OffCurveOwner));
    }

    if let Some(owner_account) = rpc.get_account(owner_wallet)? {
        if owner_account.executable {
            return Ok(Some(SkipReason::ExecutableOwner));
        }

        if owner_account.owner_program != system_program_id() {
            return Ok(Some(SkipReason::ProgramOwnedOwner));
        }
    }

    if owner_has_positive_distribution_balance(rpc, owner_wallet, distribution_token)? {
        return Ok(Some(SkipReason::ExistingDistributionHolder));
    }

    Ok(None)
}

fn owner_has_positive_distribution_balance(
    rpc: &impl RpcReader,
    owner_wallet: &Pubkey,
    distribution_token: &Pubkey,
) -> Result<bool, DiscoveryError> {
    for token_account in rpc.get_token_accounts_by_owner(owner_wallet, distribution_token)? {
        let Some(ParsedAccount::TokenAccount { amount, .. }) = token_account.account.parsed else {
            continue;
        };
        if parse_raw_amount(&amount).unwrap_or(0) > 0 {
            return Ok(true);
        }
    }

    Ok(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryReport {
    pub distribution_token: TokenMetadata,
    pub target_tokens: Vec<TokenMetadata>,
    pub recipients: Vec<DiscoveredRecipient>,
    pub skipped: Vec<SkippedCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenMetadata {
    pub token_address: Pubkey,
    pub token_program: Pubkey,
    pub decimals: u8,
    pub supply: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRecipient {
    pub wallet: Pubkey,
    pub best_rank: usize,
    pub holdings: Vec<TargetHolding>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetHolding {
    pub target_token_address: Pubkey,
    pub token_account: Pubkey,
    pub raw_amount: String,
    pub decimals: u8,
    pub rank: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifiedCandidate {
    owner_wallet: Pubkey,
    rank: usize,
    holding: TargetHolding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedCandidate {
    pub target_token_address: Pubkey,
    pub token_account: Pubkey,
    pub owner_wallet: Option<Pubkey>,
    pub rank: usize,
    pub reason: SkipReason,
}

impl SkippedCandidate {
    fn from_balance(
        target_token_address: &Pubkey,
        balance: &TokenAccountBalance,
        rank: usize,
        reason: SkipReason,
    ) -> Self {
        Self {
            target_token_address: *target_token_address,
            token_account: balance.token_account,
            owner_wallet: None,
            rank,
            reason,
        }
    }

    fn from_candidate(candidate: VerifiedCandidate, reason: SkipReason) -> Self {
        Self {
            target_token_address: candidate.holding.target_token_address,
            token_account: candidate.holding.token_account,
            owner_wallet: Some(candidate.owner_wallet),
            rank: candidate.rank,
            reason,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    MissingTokenAccount,
    TokenAccountLookupFailed,
    MalformedTokenAccount,
    ZeroBalance,
    TokenMintMismatch,
    UnsupportedTokenProgram,
    ExecutableTokenAccount,
    SourceWallet,
    ManualExclude,
    OffCurveOwner,
    ExecutableOwner,
    ProgramOwnedOwner,
    ExistingDistributionHolder,
    RecipientLimit,
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error("configured token account `{token_address}` does not exist")]
    MissingTokenAccount { token_address: Pubkey },
    #[error("configured token account `{token_address}` is executable")]
    TokenAccountIsExecutable { token_address: Pubkey },
    #[error("configured token `{token_address}` uses Token-2022, which is not supported in v0")]
    UnsupportedToken2022 { token_address: Pubkey },
    #[error("configured token `{token_address}` is owned by unsupported program `{owner_program}`")]
    UnsupportedTokenProgram {
        token_address: Pubkey,
        owner_program: Pubkey,
    },
    #[error("configured token account `{token_address}` is not a parsed SPL mint")]
    TokenAccountIsNotMint { token_address: Pubkey },
}

pub fn token_program_id() -> Pubkey {
    Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
        .expect("valid SPL Token Program id")
}

pub fn token_2022_program_id() -> Pubkey {
    Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb")
        .expect("valid Token-2022 Program id")
}

pub fn system_program_id() -> Pubkey {
    Pubkey::from_str("11111111111111111111111111111111").expect("valid System Program id")
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::rpc::{RpcAccount, RpcTokenAccount, TokenAccountBalance},
        solana_keypair::{Keypair, Signer},
        std::collections::HashMap,
    };

    #[derive(Default)]
    struct MockRpc {
        accounts: HashMap<Pubkey, RpcAccount>,
        largest: HashMap<Pubkey, Vec<TokenAccountBalance>>,
        owned_token_accounts: HashMap<(Pubkey, Pubkey), Vec<RpcTokenAccount>>,
    }

    impl RpcReader for MockRpc {
        fn get_account(&self, address: &Pubkey) -> Result<Option<RpcAccount>, RpcError> {
            Ok(self.accounts.get(address).cloned())
        }

        fn get_token_largest_accounts(
            &self,
            mint: &Pubkey,
        ) -> Result<Vec<TokenAccountBalance>, RpcError> {
            Ok(self.largest.get(mint).cloned().unwrap_or_default())
        }

        fn get_token_accounts_by_owner(
            &self,
            owner: &Pubkey,
            mint: &Pubkey,
        ) -> Result<Vec<RpcTokenAccount>, RpcError> {
            Ok(self
                .owned_token_accounts
                .get(&(*owner, *mint))
                .cloned()
                .unwrap_or_default())
        }
    }

    #[test]
    fn validates_original_spl_mint() {
        let mint = pubkey();
        let mut rpc = MockRpc::default();
        rpc.accounts.insert(mint, mint_account(6));

        let metadata = validate_original_spl_mint(&rpc, &mint).unwrap();

        assert_eq!(metadata.token_address, mint);
        assert_eq!(metadata.token_program, token_program_id());
        assert_eq!(metadata.decimals, 6);
    }

    #[test]
    fn rejects_token_2022_mints() {
        let mint = pubkey();
        let mut account = mint_account(6);
        account.owner_program = token_2022_program_id();
        let mut rpc = MockRpc::default();
        rpc.accounts.insert(mint, account);

        let err = validate_original_spl_mint(&rpc, &mint).unwrap_err();

        assert!(matches!(err, DiscoveryError::UnsupportedToken2022 { .. }));
    }

    #[test]
    fn discovers_and_excludes_candidates() {
        let distribution = pubkey();
        let target = pubkey();
        let source_wallet = keypair_pubkey();
        let good_owner = keypair_pubkey();
        let manual_owner = keypair_pubkey();
        let existing_holder = keypair_pubkey();
        let program_owned = keypair_pubkey();
        let token_account_good = pubkey();
        let token_account_source = pubkey();
        let token_account_manual = pubkey();
        let token_account_existing = pubkey();
        let token_account_program_owned = pubkey();
        let token_account_zero = pubkey();
        let distribution_holder_account = pubkey();

        let mut rpc = MockRpc::default();
        rpc.accounts.insert(distribution, mint_account(6));
        rpc.accounts.insert(target, mint_account(6));
        rpc.accounts.insert(good_owner, system_account());
        rpc.accounts.insert(source_wallet, system_account());
        rpc.accounts.insert(manual_owner, system_account());
        rpc.accounts.insert(existing_holder, system_account());
        rpc.accounts.insert(program_owned, program_owned_account());
        rpc.accounts.insert(
            token_account_good,
            token_account(target, good_owner, "100", 6),
        );
        rpc.accounts.insert(
            token_account_source,
            token_account(target, source_wallet, "90", 6),
        );
        rpc.accounts.insert(
            token_account_manual,
            token_account(target, manual_owner, "80", 6),
        );
        rpc.accounts.insert(
            token_account_existing,
            token_account(target, existing_holder, "70", 6),
        );
        rpc.accounts.insert(
            token_account_program_owned,
            token_account(target, program_owned, "60", 6),
        );
        rpc.accounts.insert(
            token_account_zero,
            token_account(target, keypair_pubkey(), "0", 6),
        );
        rpc.largest.insert(
            target,
            vec![
                balance(token_account_good, "100", 6),
                balance(token_account_source, "90", 6),
                balance(token_account_manual, "80", 6),
                balance(token_account_existing, "70", 6),
                balance(token_account_program_owned, "60", 6),
                balance(token_account_zero, "0", 6),
            ],
        );
        rpc.owned_token_accounts.insert(
            (existing_holder, distribution),
            vec![RpcTokenAccount {
                token_account: distribution_holder_account,
                account: token_account(distribution, existing_holder, "1", 6),
            }],
        );

        let config = ValidatedConfig {
            cluster_name: "mainnet-beta".to_owned(),
            rpc_url_env: "SOLANA_RPC_URL".to_owned(),
            distribution_token_address: distribution,
            total_amount_ui: "1000".to_owned(),
            target_token_addresses: vec![target],
            max_recipients: 100,
            manual_exclude_wallets: vec![manual_owner],
            solscan: crate::config::ValidatedSolscanConfig {
                enabled: false,
                api_key_env: "SOLSCAN_API_KEY".to_owned(),
            },
        };

        let report = discover_holders(&rpc, &config, &source_wallet).unwrap();

        assert_eq!(report.recipients.len(), 1);
        assert_eq!(report.recipients[0].wallet, good_owner);
        assert_eq!(
            skipped_reasons(&report),
            vec![
                SkipReason::SourceWallet,
                SkipReason::ManualExclude,
                SkipReason::ExistingDistributionHolder,
                SkipReason::ProgramOwnedOwner,
                SkipReason::ZeroBalance,
            ]
        );
    }

    #[test]
    fn excludes_off_curve_owner() {
        let distribution = pubkey();
        let target = pubkey();
        let source_wallet = keypair_pubkey();
        let token_account_address = pubkey();
        let (off_curve_owner, _) = Pubkey::find_program_address(&[b"owner"], &pubkey());

        let mut rpc = MockRpc::default();
        rpc.accounts.insert(distribution, mint_account(6));
        rpc.accounts.insert(target, mint_account(6));
        rpc.accounts.insert(
            token_account_address,
            token_account(target, off_curve_owner, "100", 6),
        );
        rpc.largest
            .insert(target, vec![balance(token_account_address, "100", 6)]);

        let config = ValidatedConfig {
            cluster_name: "mainnet-beta".to_owned(),
            rpc_url_env: "SOLANA_RPC_URL".to_owned(),
            distribution_token_address: distribution,
            total_amount_ui: "1000".to_owned(),
            target_token_addresses: vec![target],
            max_recipients: 100,
            manual_exclude_wallets: vec![],
            solscan: crate::config::ValidatedSolscanConfig {
                enabled: false,
                api_key_env: "SOLSCAN_API_KEY".to_owned(),
            },
        };

        let report = discover_holders(&rpc, &config, &source_wallet).unwrap();

        assert!(report.recipients.is_empty());
        assert_eq!(report.skipped[0].reason, SkipReason::OffCurveOwner);
    }

    #[test]
    fn applies_global_recipient_limit() {
        let distribution = pubkey();
        let target = pubkey();
        let source_wallet = keypair_pubkey();
        let owner_one = keypair_pubkey();
        let owner_two = keypair_pubkey();
        let token_account_one = pubkey();
        let token_account_two = pubkey();

        let mut rpc = MockRpc::default();
        rpc.accounts.insert(distribution, mint_account(6));
        rpc.accounts.insert(target, mint_account(6));
        rpc.accounts.insert(owner_one, system_account());
        rpc.accounts.insert(owner_two, system_account());
        rpc.accounts.insert(
            token_account_one,
            token_account(target, owner_one, "100", 6),
        );
        rpc.accounts
            .insert(token_account_two, token_account(target, owner_two, "90", 6));
        rpc.largest.insert(
            target,
            vec![
                balance(token_account_one, "100", 6),
                balance(token_account_two, "90", 6),
            ],
        );

        let config = ValidatedConfig {
            cluster_name: "mainnet-beta".to_owned(),
            rpc_url_env: "SOLANA_RPC_URL".to_owned(),
            distribution_token_address: distribution,
            total_amount_ui: "1000".to_owned(),
            target_token_addresses: vec![target],
            max_recipients: 1,
            manual_exclude_wallets: vec![],
            solscan: crate::config::ValidatedSolscanConfig {
                enabled: false,
                api_key_env: "SOLSCAN_API_KEY".to_owned(),
            },
        };

        let report = discover_holders(&rpc, &config, &source_wallet).unwrap();

        assert_eq!(report.recipients.len(), 1);
        assert_eq!(report.recipients[0].wallet, owner_one);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].owner_wallet, Some(owner_two));
        assert_eq!(report.skipped[0].reason, SkipReason::RecipientLimit);
    }

    fn skipped_reasons(report: &DiscoveryReport) -> Vec<SkipReason> {
        report
            .skipped
            .iter()
            .map(|skipped| skipped.reason)
            .collect()
    }

    fn mint_account(decimals: u8) -> RpcAccount {
        RpcAccount {
            owner_program: token_program_id(),
            executable: false,
            parsed: Some(ParsedAccount::Mint {
                decimals,
                supply: "1000000".to_owned(),
            }),
        }
    }

    fn token_account(mint: Pubkey, owner: Pubkey, amount: &str, decimals: u8) -> RpcAccount {
        RpcAccount {
            owner_program: token_program_id(),
            executable: false,
            parsed: Some(ParsedAccount::TokenAccount {
                mint,
                owner,
                amount: amount.to_owned(),
                decimals,
            }),
        }
    }

    fn system_account() -> RpcAccount {
        RpcAccount {
            owner_program: system_program_id(),
            executable: false,
            parsed: None,
        }
    }

    fn program_owned_account() -> RpcAccount {
        RpcAccount {
            owner_program: pubkey(),
            executable: false,
            parsed: None,
        }
    }

    fn balance(token_account: Pubkey, amount: &str, decimals: u8) -> TokenAccountBalance {
        TokenAccountBalance {
            token_account,
            amount: amount.to_owned(),
            decimals,
            ui_amount_string: amount.to_owned(),
        }
    }

    fn pubkey() -> Pubkey {
        keypair_pubkey()
    }

    fn keypair_pubkey() -> Pubkey {
        Keypair::new().pubkey()
    }
}
