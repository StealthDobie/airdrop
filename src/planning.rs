use {
    crate::{
        config::ValidatedConfig,
        discovery::{
            DiscoveredRecipient, DiscoveryReport, SkipReason, SkippedCandidate, TargetHolding,
            system_program_id,
        },
        rpc::{ParsedAccount, RpcAccount, RpcError, RpcReader, parse_raw_amount},
    },
    serde::{Deserialize, Serialize},
    solana_pubkey::Pubkey,
    std::{
        cmp::Ordering,
        collections::{BTreeMap, BTreeSet},
        fs,
        fs::File,
        io::Write,
        path::{Path, PathBuf},
        str::FromStr,
        time::{SystemTime, UNIX_EPOCH},
    },
    thiserror::Error,
};

pub const LEGACY_TRANSACTION_SIZE_LIMIT: usize = 1_232;
pub const ACCOUNT_LOCK_LIMIT: usize = 64;
pub const EXECUTED_INSTRUCTION_LIMIT: usize = 64;
const TOKEN_2022_ASSOCIATED_TOKEN_ACCOUNT_DATA_LEN: usize = 170;
const ESTIMATED_SIGNATURE_FEE_LAMPORTS: u64 = 5_000;

const SIGNATURE_LENGTH: usize = 64;
const MESSAGE_HEADER_LENGTH: usize = 3;
const RECENT_BLOCKHASH_LENGTH: usize = 32;
const ATA_CREATE_ACCOUNT_COUNT: usize = 6;
const ATA_CREATE_DATA_LENGTH: usize = 1;
const ATA_CREATE_EXECUTED_INSTRUCTION_ESTIMATE: usize = 4;
const TRANSFER_CHECKED_ACCOUNT_COUNT: usize = 4;
const TRANSFER_CHECKED_DATA_LENGTH: usize = 10;

pub fn create_distribution_plan(
    rpc: &impl RpcReader,
    config: &ValidatedConfig,
    report: DiscoveryReport,
    source_wallet: &Pubkey,
) -> Result<DistributionPlan, PlanError> {
    if report.recipients.is_empty() {
        return Err(PlanError::NoRecipients);
    }

    let total_amount_raw =
        ui_amount_to_raw(&config.total_amount_ui, report.distribution_token.decimals)?;
    let recipient_count = report.recipients.len();
    let amount_per_recipient_raw = total_amount_raw / recipient_count as u64;
    let remainder_raw = total_amount_raw % recipient_count as u64;
    if amount_per_recipient_raw == 0 {
        return Err(PlanError::AmountTooSmallForRecipientCount {
            total_amount_raw,
            recipient_count,
        });
    }

    let source_ata = derive_associated_token_account(
        source_wallet,
        &report.distribution_token.token_address,
        &report.distribution_token.token_program,
    );
    eprintln!("Reading source token account {}", source_ata);
    let source_balance_raw = read_required_token_account(
        rpc,
        &source_ata,
        &report.distribution_token.token_address,
        &report.distribution_token.token_program,
        source_wallet,
        report.distribution_token.decimals,
        TokenAccountRole::Source,
    )?
    .amount_raw;

    if source_balance_raw < total_amount_raw {
        return Err(PlanError::InsufficientSourceBalance {
            source_ata,
            available_raw: source_balance_raw,
            required_raw: total_amount_raw,
        });
    }

    eprintln!(
        "Planning recipient ATAs for {} recipient(s)",
        recipient_count
    );
    let recipient_inputs = report
        .recipients
        .into_iter()
        .map(|recipient| {
            let recipient_ata = derive_associated_token_account(
                &recipient.wallet,
                &report.distribution_token.token_address,
                &report.distribution_token.token_program,
            );
            (recipient, recipient_ata)
        })
        .collect::<Vec<_>>();
    let recipient_atas = recipient_inputs
        .iter()
        .map(|(_, recipient_ata)| *recipient_ata)
        .collect::<Vec<_>>();
    eprintln!("Reading {} recipient ATA account(s)", recipient_atas.len());
    let recipient_ata_accounts = rpc.get_multiple_accounts(&recipient_atas)?;

    let mut seen_recipients = BTreeSet::new();
    let mut planned_recipients = Vec::with_capacity(recipient_count);
    for ((recipient, recipient_ata), recipient_ata_account) in
        recipient_inputs.into_iter().zip(recipient_ata_accounts)
    {
        if !seen_recipients.insert(recipient.wallet) {
            return Err(PlanError::DuplicateRecipient {
                wallet: recipient.wallet,
            });
        }

        planned_recipients.push(plan_recipient(
            recipient,
            recipient_ata,
            recipient_ata_account,
            &report.distribution_token.token_address,
            &report.distribution_token.token_program,
            report.distribution_token.decimals,
            amount_per_recipient_raw,
        )?);
    }

    eprintln!(
        "Packing {} planned recipient(s) into transactions",
        planned_recipients.len()
    );
    let batches = pack_recipients(
        &planned_recipients,
        source_wallet,
        &source_ata,
        &report.distribution_token.token_address,
        &report.distribution_token.token_program,
    )?;
    let rent_per_ata_lamports =
        rpc.get_minimum_balance_for_rent_exemption(TOKEN_2022_ASSOCIATED_TOKEN_ACCOUNT_DATA_LEN)?;
    let ata_creations = planned_recipients
        .iter()
        .filter(|recipient| recipient.create_recipient_ata)
        .count();
    let estimated_signature_fee_lamports =
        ESTIMATED_SIGNATURE_FEE_LAMPORTS.saturating_mul(batches.len() as u64);

    Ok(DistributionPlan {
        cluster_name: config.cluster_name.clone(),
        source_wallet: *source_wallet,
        source_ata,
        source_balance_raw,
        source_balance_ui: format_raw_amount(
            source_balance_raw,
            report.distribution_token.decimals,
        ),
        distribution_token_address: report.distribution_token.token_address,
        distribution_token_program: report.distribution_token.token_program,
        distribution_decimals: report.distribution_token.decimals,
        total_amount_raw,
        total_amount_ui: format_raw_amount(total_amount_raw, report.distribution_token.decimals),
        amount_per_recipient_raw,
        amount_per_recipient_ui: format_raw_amount(
            amount_per_recipient_raw,
            report.distribution_token.decimals,
        ),
        remainder_raw,
        remainder_ui: format_raw_amount(remainder_raw, report.distribution_token.decimals),
        recipients: planned_recipients,
        skipped: report.skipped,
        batches,
        rent_per_ata_lamports,
        estimated_ata_rent_lamports: rent_per_ata_lamports.saturating_mul(ata_creations as u64),
        estimated_signature_fee_lamports,
    })
}

fn plan_recipient(
    recipient: DiscoveredRecipient,
    recipient_ata: Pubkey,
    recipient_ata_account: Option<RpcAccount>,
    distribution_token: &Pubkey,
    token_program: &Pubkey,
    decimals: u8,
    amount_per_recipient_raw: u64,
) -> Result<PlannedRecipient, PlanError> {
    let existing_ata = parse_optional_token_account(
        recipient_ata_account,
        &recipient_ata,
        distribution_token,
        token_program,
        &recipient.wallet,
        decimals,
        TokenAccountRole::Recipient,
    )?;

    Ok(PlannedRecipient {
        wallet: recipient.wallet,
        recipient_ata,
        amount_raw: amount_per_recipient_raw,
        amount_ui: format_raw_amount(amount_per_recipient_raw, decimals),
        create_recipient_ata: existing_ata.is_none(),
        best_rank: recipient.best_rank,
        holdings: recipient.holdings,
    })
}

pub fn write_plan_artifacts(
    plan: &DistributionPlan,
    config: &ValidatedConfig,
    runs_dir: impl AsRef<Path>,
) -> Result<PlanArtifacts, PlanError> {
    let run_id = run_id()?;
    let run_dir = runs_dir.as_ref().join(&run_id);
    create_dir_all(&run_dir)?;

    let artifacts = PlanArtifacts {
        run_id,
        run_dir: run_dir.clone(),
        plan_path: run_dir.join("plan.json"),
        recipients_path: run_dir.join("recipients.csv"),
        skipped_path: run_dir.join("skipped.csv"),
        ledger_path: run_dir.join("ledger.jsonl"),
        simulation_path: run_dir.join("simulation.json"),
    };

    write_json(
        &artifacts.plan_path,
        &PlanJson::from_plan(plan, config, &artifacts),
    )?;
    write_recipients_csv(&artifacts.recipients_path, plan)?;
    write_skipped_csv(&artifacts.skipped_path, plan)?;
    write_text(&artifacts.ledger_path, "")?;

    Ok(artifacts)
}

pub fn read_plan_artifacts(
    run_dir: impl AsRef<Path>,
) -> Result<(DistributionPlan, PlanArtifacts), PlanError> {
    let artifacts = plan_artifacts_for_run_dir(run_dir.as_ref())?;
    let contents = fs::read_to_string(&artifacts.plan_path).map_err(|source| PlanError::Read {
        path: artifacts.plan_path.display().to_string(),
        source,
    })?;
    let plan_json: PlanJson = serde_json::from_str(&contents)?;
    let plan = plan_json.into_plan(&artifacts.plan_path)?;

    Ok((plan, artifacts))
}

pub fn validate_cached_plan_config(
    run_dir: impl AsRef<Path>,
    config: &ValidatedConfig,
) -> Result<(), PlanError> {
    let artifacts = plan_artifacts_for_run_dir(run_dir.as_ref())?;
    let contents = fs::read_to_string(&artifacts.plan_path).map_err(|source| PlanError::Read {
        path: artifacts.plan_path.display().to_string(),
        source,
    })?;
    let plan_json: PlanJson = serde_json::from_str(&contents)?;
    let cached_config = plan_json
        .config
        .ok_or_else(|| PlanError::MissingPlanConfigSnapshot {
            path: artifacts.plan_path.display().to_string(),
        })?;

    cached_config.validate_matches(config, &artifacts.plan_path)
}

fn plan_artifacts_for_run_dir(run_dir: &Path) -> Result<PlanArtifacts, PlanError> {
    let run_dir = run_dir.to_path_buf();
    let run_id = run_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PlanError::InvalidRunDirectory {
            path: run_dir.display().to_string(),
        })?
        .to_owned();

    Ok(PlanArtifacts {
        run_id,
        run_dir: run_dir.clone(),
        plan_path: run_dir.join("plan.json"),
        recipients_path: run_dir.join("recipients.csv"),
        skipped_path: run_dir.join("skipped.csv"),
        ledger_path: run_dir.join("ledger.jsonl"),
        simulation_path: run_dir.join("simulation.json"),
    })
}

fn pack_recipients(
    recipients: &[PlannedRecipient],
    source_wallet: &Pubkey,
    source_ata: &Pubkey,
    distribution_token: &Pubkey,
    token_program: &Pubkey,
) -> Result<Vec<PlannedBatch>, PlanError> {
    let mut batches = Vec::new();
    let mut current = Vec::<usize>::new();

    for index in 0..recipients.len() {
        let mut candidate = current.clone();
        candidate.push(index);
        let candidate_metrics = estimate_legacy_transaction(
            recipients,
            &candidate,
            source_wallet,
            source_ata,
            distribution_token,
            token_program,
        );

        if candidate_metrics.within_limits() {
            current = candidate;
            continue;
        }

        if current.is_empty() {
            return Err(PlanError::RecipientDoesNotFit {
                wallet: recipients[index].wallet,
                metrics: candidate_metrics,
            });
        }

        let batch = build_batch(
            batches.len(),
            recipients,
            &current,
            source_wallet,
            source_ata,
            distribution_token,
            token_program,
        );
        batches.push(batch);
        current = vec![index];

        let single_metrics = estimate_legacy_transaction(
            recipients,
            &current,
            source_wallet,
            source_ata,
            distribution_token,
            token_program,
        );
        if !single_metrics.within_limits() {
            return Err(PlanError::RecipientDoesNotFit {
                wallet: recipients[index].wallet,
                metrics: single_metrics,
            });
        }
    }

    if !current.is_empty() {
        let batch = build_batch(
            batches.len(),
            recipients,
            &current,
            source_wallet,
            source_ata,
            distribution_token,
            token_program,
        );
        batches.push(batch);
    }

    Ok(batches)
}

fn build_batch(
    index: usize,
    recipients: &[PlannedRecipient],
    recipient_indexes: &[usize],
    source_wallet: &Pubkey,
    source_ata: &Pubkey,
    distribution_token: &Pubkey,
    token_program: &Pubkey,
) -> PlannedBatch {
    let metrics = estimate_legacy_transaction(
        recipients,
        recipient_indexes,
        source_wallet,
        source_ata,
        distribution_token,
        token_program,
    );
    let recipients = recipient_indexes
        .iter()
        .map(|recipient_index| {
            let recipient = &recipients[*recipient_index];
            PlannedBatchRecipient {
                recipient_index: *recipient_index,
                wallet: recipient.wallet,
                recipient_ata: recipient.recipient_ata,
                create_recipient_ata: recipient.create_recipient_ata,
            }
        })
        .collect::<Vec<_>>();
    let ata_creations = recipients
        .iter()
        .filter(|recipient| recipient.create_recipient_ata)
        .count();

    PlannedBatch {
        index,
        recipient_count: recipients.len(),
        ata_creations,
        metrics,
        recipients,
    }
}

fn estimate_legacy_transaction(
    recipients: &[PlannedRecipient],
    recipient_indexes: &[usize],
    source_wallet: &Pubkey,
    source_ata: &Pubkey,
    distribution_token: &Pubkey,
    token_program: &Pubkey,
) -> TransactionMetrics {
    let mut accounts = BTreeSet::from([
        *source_wallet,
        *source_ata,
        *distribution_token,
        *token_program,
    ]);
    let mut ata_creations = 0_usize;

    for index in recipient_indexes {
        let recipient = &recipients[*index];
        accounts.insert(recipient.recipient_ata);
        if recipient.create_recipient_ata {
            ata_creations += 1;
            accounts.insert(recipient.wallet);
            accounts.insert(associated_token_program_id());
            accounts.insert(system_program_id());
        }
    }

    let transfer_count = recipient_indexes.len();
    let top_level_instruction_count = transfer_count + ata_creations;
    let estimated_executed_instruction_count =
        transfer_count + ata_creations * ATA_CREATE_EXECUTED_INSTRUCTION_ESTIMATE;
    let instructions_size = ata_creations
        * compiled_instruction_size(ATA_CREATE_ACCOUNT_COUNT, ATA_CREATE_DATA_LENGTH)
        + transfer_count
            * compiled_instruction_size(
                TRANSFER_CHECKED_ACCOUNT_COUNT,
                TRANSFER_CHECKED_DATA_LENGTH,
            );
    let message_size = MESSAGE_HEADER_LENGTH
        + short_vec_size(accounts.len())
        + accounts.len() * 32
        + RECENT_BLOCKHASH_LENGTH
        + short_vec_size(top_level_instruction_count)
        + instructions_size;
    let serialized_size = short_vec_size(1) + SIGNATURE_LENGTH + message_size;

    TransactionMetrics {
        serialized_size,
        account_locks: accounts.len(),
        top_level_instruction_count,
        estimated_executed_instruction_count,
    }
}

fn compiled_instruction_size(account_count: usize, data_len: usize) -> usize {
    1 + short_vec_size(account_count) + account_count + short_vec_size(data_len) + data_len
}

fn short_vec_size(mut value: usize) -> usize {
    let mut size = 1;
    while value >= 0x80 {
        value >>= 7;
        size += 1;
    }
    size
}

fn read_required_token_account(
    rpc: &impl RpcReader,
    address: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
    owner: &Pubkey,
    decimals: u8,
    role: TokenAccountRole,
) -> Result<TokenAccountState, PlanError> {
    read_optional_token_account(rpc, address, mint, token_program, owner, decimals, role)?.ok_or(
        PlanError::MissingTokenAccount {
            address: *address,
            role,
        },
    )
}

fn read_optional_token_account(
    rpc: &impl RpcReader,
    address: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
    owner: &Pubkey,
    decimals: u8,
    role: TokenAccountRole,
) -> Result<Option<TokenAccountState>, PlanError> {
    parse_optional_token_account(
        rpc.get_account(address)?,
        address,
        mint,
        token_program,
        owner,
        decimals,
        role,
    )
}

fn parse_optional_token_account(
    account: Option<RpcAccount>,
    address: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
    owner: &Pubkey,
    decimals: u8,
    role: TokenAccountRole,
) -> Result<Option<TokenAccountState>, PlanError> {
    let Some(account) = account else {
        return Ok(None);
    };

    if account.executable {
        return Err(PlanError::InvalidTokenAccount {
            address: *address,
            role,
            reason: "account is executable",
        });
    }

    if account.owner_program != *token_program {
        return Err(PlanError::InvalidTokenAccount {
            address: *address,
            role,
            reason: "account is not owned by the expected Token-2022 Program",
        });
    }

    let Some(ParsedAccount::TokenAccount {
        mint: actual_mint,
        owner: actual_owner,
        amount,
        decimals: actual_decimals,
    }) = account.parsed
    else {
        return Err(PlanError::InvalidTokenAccount {
            address: *address,
            role,
            reason: "account is not a parsed token account",
        });
    };

    if actual_mint != *mint {
        return Err(PlanError::InvalidTokenAccount {
            address: *address,
            role,
            reason: "token account mint does not match the distribution token",
        });
    }

    if actual_owner != *owner {
        return Err(PlanError::InvalidTokenAccount {
            address: *address,
            role,
            reason: "token account owner does not match the expected wallet",
        });
    }

    if actual_decimals != decimals {
        return Err(PlanError::InvalidTokenAccount {
            address: *address,
            role,
            reason: "token account decimals do not match the distribution token",
        });
    }

    Ok(Some(TokenAccountState {
        amount_raw: parse_u64_token_amount(&amount)?,
    }))
}

pub fn derive_associated_token_account(
    wallet: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Pubkey {
    Pubkey::find_program_address(
        &[wallet.as_ref(), token_program.as_ref(), mint.as_ref()],
        &associated_token_program_id(),
    )
    .0
}

pub fn associated_token_program_id() -> Pubkey {
    Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")
        .expect("valid Associated Token Account Program id")
}

fn parse_u64_token_amount(value: &str) -> Result<u64, PlanError> {
    let amount = parse_raw_amount(value).ok_or_else(|| PlanError::InvalidRawAmount {
        value: value.to_owned(),
    })?;
    amount.try_into().map_err(|_| PlanError::InvalidRawAmount {
        value: value.to_owned(),
    })
}

fn ui_amount_to_raw(value: &str, decimals: u8) -> Result<u64, PlanError> {
    let (whole, fractional) = value
        .split_once('.')
        .map_or((value, ""), |(whole, fractional)| (whole, fractional));
    if whole.is_empty()
        || !whole.chars().all(|ch| ch.is_ascii_digit())
        || !fractional.chars().all(|ch| ch.is_ascii_digit())
    {
        return Err(PlanError::InvalidUiAmount {
            amount: value.to_owned(),
        });
    }

    let decimals = usize::from(decimals);
    if fractional.len() > decimals {
        return Err(PlanError::TooManyFractionalDigits {
            amount: value.to_owned(),
            decimals,
        });
    }

    let mut raw = String::with_capacity(whole.len() + decimals);
    raw.push_str(whole.trim_start_matches('0'));
    raw.push_str(fractional);
    raw.extend(std::iter::repeat_n('0', decimals - fractional.len()));

    let raw = raw.trim_start_matches('0');
    let raw = if raw.is_empty() { "0" } else { raw };
    raw.parse()
        .map_err(|_| PlanError::AmountExceedsSplTokenLimit {
            amount: value.to_owned(),
        })
}

fn format_raw_amount(raw: u64, decimals: u8) -> String {
    if decimals == 0 {
        return raw.to_string();
    }

    let decimals = usize::from(decimals);
    let digits = raw.to_string();
    let (whole, fractional) = match digits.len().cmp(&decimals) {
        Ordering::Greater => {
            let split_at = digits.len() - decimals;
            (digits[..split_at].to_owned(), digits[split_at..].to_owned())
        }
        _ => {
            let mut fractional = String::with_capacity(decimals);
            fractional.extend(std::iter::repeat_n('0', decimals - digits.len()));
            fractional.push_str(&digits);
            ("0".to_owned(), fractional)
        }
    };

    let fractional = fractional.trim_end_matches('0');
    if fractional.is_empty() {
        whole
    } else {
        format!("{whole}.{fractional}")
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), PlanError> {
    let mut contents = serde_json::to_string_pretty(value)?;
    contents.push('\n');
    write_text(path, &contents)
}

fn write_recipients_csv(path: &Path, plan: &DistributionPlan) -> Result<(), PlanError> {
    let mut contents =
        "wallet,recipient_ata,amount_raw,amount_ui,ata_action,best_rank,holding_count\n".to_owned();
    for recipient in &plan.recipients {
        contents.push_str(&format!(
            "{},{},{},{},{},{},{}\n",
            recipient.wallet,
            recipient.recipient_ata,
            recipient.amount_raw,
            recipient.amount_ui,
            if recipient.create_recipient_ata {
                "create"
            } else {
                "exists"
            },
            recipient.best_rank,
            recipient.holdings.len()
        ));
    }
    write_text(path, &contents)
}

fn write_skipped_csv(path: &Path, plan: &DistributionPlan) -> Result<(), PlanError> {
    let mut contents = "target_token_address,token_account,owner_wallet,rank,reason\n".to_owned();
    for skipped in &plan.skipped {
        contents.push_str(&format!(
            "{},{},{},{},{:?}\n",
            skipped.target_token_address,
            skipped.token_account,
            skipped
                .owner_wallet
                .map(|wallet| wallet.to_string())
                .unwrap_or_default(),
            skipped.rank,
            skipped.reason
        ));
    }
    write_text(path, &contents)
}

fn write_text(path: &Path, contents: &str) -> Result<(), PlanError> {
    let mut file = File::create(path).map_err(|source| PlanError::Io {
        path: path.display().to_string(),
        source,
    })?;
    file.write_all(contents.as_bytes())
        .map_err(|source| PlanError::Io {
            path: path.display().to_string(),
            source,
        })
}

fn create_dir_all(path: &Path) -> Result<(), PlanError> {
    fs::create_dir_all(path).map_err(|source| PlanError::Io {
        path: path.display().to_string(),
        source,
    })
}

fn run_id() -> Result<String, PlanError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| PlanError::SystemTimeBeforeUnixEpoch)?;
    Ok(duration.as_millis().to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistributionPlan {
    pub cluster_name: String,
    pub source_wallet: Pubkey,
    pub source_ata: Pubkey,
    pub source_balance_raw: u64,
    pub source_balance_ui: String,
    pub distribution_token_address: Pubkey,
    pub distribution_token_program: Pubkey,
    pub distribution_decimals: u8,
    pub total_amount_raw: u64,
    pub total_amount_ui: String,
    pub amount_per_recipient_raw: u64,
    pub amount_per_recipient_ui: String,
    pub remainder_raw: u64,
    pub remainder_ui: String,
    pub recipients: Vec<PlannedRecipient>,
    pub skipped: Vec<SkippedCandidate>,
    pub batches: Vec<PlannedBatch>,
    pub rent_per_ata_lamports: u64,
    pub estimated_ata_rent_lamports: u64,
    pub estimated_signature_fee_lamports: u64,
}

impl DistributionPlan {
    pub fn ata_creations(&self) -> usize {
        self.recipients
            .iter()
            .filter(|recipient| recipient.create_recipient_ata)
            .count()
    }

    pub fn skipped_counts_by_reason(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for skipped in &self.skipped {
            *counts.entry(format!("{:?}", skipped.reason)).or_insert(0) += 1;
        }
        counts
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRecipient {
    pub wallet: Pubkey,
    pub recipient_ata: Pubkey,
    pub amount_raw: u64,
    pub amount_ui: String,
    pub create_recipient_ata: bool,
    pub best_rank: usize,
    pub holdings: Vec<TargetHolding>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedBatch {
    pub index: usize,
    pub recipient_count: usize,
    pub ata_creations: usize,
    pub metrics: TransactionMetrics,
    pub recipients: Vec<PlannedBatchRecipient>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedBatchRecipient {
    pub recipient_index: usize,
    pub wallet: Pubkey,
    pub recipient_ata: Pubkey,
    pub create_recipient_ata: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
pub struct TransactionMetrics {
    pub serialized_size: usize,
    pub account_locks: usize,
    pub top_level_instruction_count: usize,
    pub estimated_executed_instruction_count: usize,
}

impl TransactionMetrics {
    pub fn within_limits(&self) -> bool {
        self.serialized_size <= LEGACY_TRANSACTION_SIZE_LIMIT
            && self.account_locks <= ACCOUNT_LOCK_LIMIT
            && self.estimated_executed_instruction_count <= EXECUTED_INSTRUCTION_LIMIT
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanArtifacts {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub plan_path: PathBuf,
    pub recipients_path: PathBuf,
    pub skipped_path: PathBuf,
    pub ledger_path: PathBuf,
    pub simulation_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenAccountRole {
    Source,
    Recipient,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TokenAccountState {
    amount_raw: u64,
}

#[derive(Debug, Error)]
pub enum PlanError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("no recipients remain after discovery and exclusions")]
    NoRecipients,
    #[error("distribution amount `{amount}` is not a valid decimal amount")]
    InvalidUiAmount { amount: String },
    #[error(
        "distribution amount `{amount}` has more fractional digits than token decimals ({decimals})"
    )]
    TooManyFractionalDigits { amount: String, decimals: usize },
    #[error("distribution amount `{amount}` exceeds the SPL Token u64 raw amount limit")]
    AmountExceedsSplTokenLimit { amount: String },
    #[error("raw token amount `{value}` is invalid or exceeds the SPL Token u64 amount limit")]
    InvalidRawAmount { value: String },
    #[error(
        "distribution total {total_amount_raw} raw units is too small for {recipient_count} recipients"
    )]
    AmountTooSmallForRecipientCount {
        total_amount_raw: u64,
        recipient_count: usize,
    },
    #[error(
        "source token account `{source_ata}` has {available_raw} raw units, need {required_raw}"
    )]
    InsufficientSourceBalance {
        source_ata: Pubkey,
        available_raw: u64,
        required_raw: u64,
    },
    #[error("{role:?} token account `{address}` does not exist")]
    MissingTokenAccount {
        address: Pubkey,
        role: TokenAccountRole,
    },
    #[error("{role:?} token account `{address}` is invalid: {reason}")]
    InvalidTokenAccount {
        address: Pubkey,
        role: TokenAccountRole,
        reason: &'static str,
    },
    #[error("recipient `{wallet}` appears more than once in the discovery report")]
    DuplicateRecipient { wallet: Pubkey },
    #[error("recipient `{wallet}` cannot fit in a single legacy transaction: {metrics:?}")]
    RecipientDoesNotFit {
        wallet: Pubkey,
        metrics: TransactionMetrics,
    },
    #[error("system time is before the Unix epoch")]
    SystemTimeBeforeUnixEpoch,
    #[error("failed to write plan artifact `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read plan artifact `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid run directory `{path}`")]
    InvalidRunDirectory { path: String },
    #[error("plan artifact `{path}` has invalid `{field}` public key `{value}`")]
    InvalidPlanPubkey {
        path: String,
        field: &'static str,
        value: String,
    },
    #[error("plan artifact `{path}` has invalid skip reason `{value}`")]
    InvalidPlanSkipReason { path: String, value: String },
    #[error(
        "plan artifact `{path}` does not include a config snapshot; run `airdrop run` again before using automatic send cache"
    )]
    MissingPlanConfigSnapshot { path: String },
    #[error(
        "plan artifact `{path}` config mismatch for {field}: cached `{cached}`, current `{current}`"
    )]
    PlanConfigMismatch {
        path: String,
        field: &'static str,
        cached: String,
        current: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct PlanJson {
    #[serde(default)]
    config: Option<PlanConfigJson>,
    summary: PlanSummaryJson,
    recipients: Vec<RecipientJson>,
    skipped: Vec<SkippedJson>,
    batches: Vec<BatchJson>,
    artifact_paths: ArtifactPathsJson,
}

impl PlanJson {
    fn from_plan(
        plan: &DistributionPlan,
        config: &ValidatedConfig,
        artifacts: &PlanArtifacts,
    ) -> Self {
        Self {
            config: Some(PlanConfigJson::from_config(config)),
            summary: PlanSummaryJson::from_plan(plan),
            recipients: plan.recipients.iter().map(RecipientJson::from).collect(),
            skipped: plan.skipped.iter().map(SkippedJson::from).collect(),
            batches: plan.batches.iter().map(BatchJson::from).collect(),
            artifact_paths: ArtifactPathsJson::from(artifacts),
        }
    }

    fn into_plan(self, path: &Path) -> Result<DistributionPlan, PlanError> {
        let summary = self.summary;
        let recipients = self
            .recipients
            .into_iter()
            .map(|recipient| recipient.into_planned_recipient(path))
            .collect::<Result<Vec<_>, _>>()?;
        let skipped = self
            .skipped
            .into_iter()
            .map(|skipped| skipped.into_skipped_candidate(path))
            .collect::<Result<Vec<_>, _>>()?;
        let batches = self
            .batches
            .into_iter()
            .map(|batch| batch.into_planned_batch(path))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(DistributionPlan {
            cluster_name: summary.cluster_name,
            source_wallet: parse_plan_pubkey(
                path,
                "summary.source_wallet",
                &summary.source_wallet,
            )?,
            source_ata: parse_plan_pubkey(path, "summary.source_ata", &summary.source_ata)?,
            source_balance_raw: summary.source_balance_raw,
            source_balance_ui: summary.source_balance_ui,
            distribution_token_address: parse_plan_pubkey(
                path,
                "summary.distribution_token_address",
                &summary.distribution_token_address,
            )?,
            distribution_token_program: parse_plan_pubkey(
                path,
                "summary.distribution_token_program",
                &summary.distribution_token_program,
            )?,
            distribution_decimals: summary.distribution_decimals,
            total_amount_raw: summary.total_amount_raw,
            total_amount_ui: summary.total_amount_ui,
            amount_per_recipient_raw: summary.amount_per_recipient_raw,
            amount_per_recipient_ui: summary.amount_per_recipient_ui,
            remainder_raw: summary.remainder_raw,
            remainder_ui: summary.remainder_ui,
            recipients,
            skipped,
            batches,
            rent_per_ata_lamports: summary.rent_per_ata_lamports,
            estimated_ata_rent_lamports: summary.estimated_ata_rent_lamports,
            estimated_signature_fee_lamports: summary.estimated_signature_fee_lamports,
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct PlanConfigJson {
    cluster_name: String,
    distribution_token_address: String,
    total_amount_ui: String,
    target_token_addresses: Vec<String>,
    max_recipients: usize,
    manual_exclude_wallets: Vec<String>,
    solscan_enabled: bool,
    solscan_holder_fetch_limit: usize,
}

impl PlanConfigJson {
    fn from_config(config: &ValidatedConfig) -> Self {
        Self {
            cluster_name: config.cluster_name.clone(),
            distribution_token_address: config.distribution_token_address.to_string(),
            total_amount_ui: config.total_amount_ui.clone(),
            target_token_addresses: pubkey_strings(&config.target_token_addresses),
            max_recipients: config.max_recipients,
            manual_exclude_wallets: pubkey_strings(&config.manual_exclude_wallets),
            solscan_enabled: config.solscan.enabled,
            solscan_holder_fetch_limit: config.solscan.holder_fetch_limit,
        }
    }

    fn validate_matches(self, config: &ValidatedConfig, path: &Path) -> Result<(), PlanError> {
        let current = Self::from_config(config);

        require_cached_config_match(
            path,
            "cluster.name",
            self.cluster_name,
            current.cluster_name,
        )?;
        require_cached_config_match(
            path,
            "distribution.token_address",
            self.distribution_token_address,
            current.distribution_token_address,
        )?;
        require_cached_config_match(
            path,
            "distribution.total_amount_ui",
            self.total_amount_ui,
            current.total_amount_ui,
        )?;
        require_cached_config_match(
            path,
            "targeting.target_token_addresses",
            self.target_token_addresses.join(","),
            current.target_token_addresses.join(","),
        )?;
        require_cached_config_match(
            path,
            "targeting.max_recipients",
            self.max_recipients.to_string(),
            current.max_recipients.to_string(),
        )?;
        require_cached_config_match(
            path,
            "targeting.manual_exclude_wallets",
            self.manual_exclude_wallets.join(","),
            current.manual_exclude_wallets.join(","),
        )?;
        require_cached_config_match(
            path,
            "providers.solscan.enabled",
            self.solscan_enabled.to_string(),
            current.solscan_enabled.to_string(),
        )?;
        require_cached_config_match(
            path,
            "providers.solscan.holder_fetch_limit",
            self.solscan_holder_fetch_limit.to_string(),
            current.solscan_holder_fetch_limit.to_string(),
        )
    }
}

fn pubkey_strings(values: &[Pubkey]) -> Vec<String> {
    values.iter().map(ToString::to_string).collect()
}

fn require_cached_config_match(
    path: &Path,
    field: &'static str,
    cached: String,
    current: String,
) -> Result<(), PlanError> {
    if cached == current {
        return Ok(());
    }

    Err(PlanError::PlanConfigMismatch {
        path: path.display().to_string(),
        field,
        cached,
        current,
    })
}

#[derive(Debug, Deserialize, Serialize)]
struct PlanSummaryJson {
    cluster_name: String,
    source_wallet: String,
    source_ata: String,
    source_balance_raw: u64,
    source_balance_ui: String,
    distribution_token_address: String,
    distribution_token_program: String,
    distribution_decimals: u8,
    total_amount_raw: u64,
    total_amount_ui: String,
    amount_per_recipient_raw: u64,
    amount_per_recipient_ui: String,
    remainder_raw: u64,
    remainder_ui: String,
    recipient_count: usize,
    skipped_count: usize,
    skipped_counts_by_reason: BTreeMap<String, usize>,
    batch_count: usize,
    ata_creations: usize,
    rent_per_ata_lamports: u64,
    estimated_ata_rent_lamports: u64,
    estimated_signature_fee_lamports: u64,
}

impl PlanSummaryJson {
    fn from_plan(plan: &DistributionPlan) -> Self {
        Self {
            cluster_name: plan.cluster_name.clone(),
            source_wallet: plan.source_wallet.to_string(),
            source_ata: plan.source_ata.to_string(),
            source_balance_raw: plan.source_balance_raw,
            source_balance_ui: plan.source_balance_ui.clone(),
            distribution_token_address: plan.distribution_token_address.to_string(),
            distribution_token_program: plan.distribution_token_program.to_string(),
            distribution_decimals: plan.distribution_decimals,
            total_amount_raw: plan.total_amount_raw,
            total_amount_ui: plan.total_amount_ui.clone(),
            amount_per_recipient_raw: plan.amount_per_recipient_raw,
            amount_per_recipient_ui: plan.amount_per_recipient_ui.clone(),
            remainder_raw: plan.remainder_raw,
            remainder_ui: plan.remainder_ui.clone(),
            recipient_count: plan.recipients.len(),
            skipped_count: plan.skipped.len(),
            skipped_counts_by_reason: plan.skipped_counts_by_reason(),
            batch_count: plan.batches.len(),
            ata_creations: plan.ata_creations(),
            rent_per_ata_lamports: plan.rent_per_ata_lamports,
            estimated_ata_rent_lamports: plan.estimated_ata_rent_lamports,
            estimated_signature_fee_lamports: plan.estimated_signature_fee_lamports,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct RecipientJson {
    wallet: String,
    recipient_ata: String,
    amount_raw: u64,
    amount_ui: String,
    create_recipient_ata: bool,
    best_rank: usize,
    holdings: Vec<HoldingJson>,
}

impl From<&PlannedRecipient> for RecipientJson {
    fn from(recipient: &PlannedRecipient) -> Self {
        Self {
            wallet: recipient.wallet.to_string(),
            recipient_ata: recipient.recipient_ata.to_string(),
            amount_raw: recipient.amount_raw,
            amount_ui: recipient.amount_ui.clone(),
            create_recipient_ata: recipient.create_recipient_ata,
            best_rank: recipient.best_rank,
            holdings: recipient.holdings.iter().map(HoldingJson::from).collect(),
        }
    }
}

impl RecipientJson {
    fn into_planned_recipient(self, path: &Path) -> Result<PlannedRecipient, PlanError> {
        Ok(PlannedRecipient {
            wallet: parse_plan_pubkey(path, "recipients.wallet", &self.wallet)?,
            recipient_ata: parse_plan_pubkey(
                path,
                "recipients.recipient_ata",
                &self.recipient_ata,
            )?,
            amount_raw: self.amount_raw,
            amount_ui: self.amount_ui,
            create_recipient_ata: self.create_recipient_ata,
            best_rank: self.best_rank,
            holdings: self
                .holdings
                .into_iter()
                .map(|holding| holding.into_target_holding(path))
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct HoldingJson {
    target_token_address: String,
    token_account: String,
    raw_amount: String,
    decimals: u8,
    rank: usize,
}

impl From<&TargetHolding> for HoldingJson {
    fn from(holding: &TargetHolding) -> Self {
        Self {
            target_token_address: holding.target_token_address.to_string(),
            token_account: holding.token_account.to_string(),
            raw_amount: holding.raw_amount.clone(),
            decimals: holding.decimals,
            rank: holding.rank,
        }
    }
}

impl HoldingJson {
    fn into_target_holding(self, path: &Path) -> Result<TargetHolding, PlanError> {
        Ok(TargetHolding {
            target_token_address: parse_plan_pubkey(
                path,
                "holdings.target_token_address",
                &self.target_token_address,
            )?,
            token_account: parse_plan_pubkey(path, "holdings.token_account", &self.token_account)?,
            raw_amount: self.raw_amount,
            decimals: self.decimals,
            rank: self.rank,
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct SkippedJson {
    target_token_address: String,
    token_account: String,
    owner_wallet: Option<String>,
    rank: usize,
    reason: String,
}

impl From<&SkippedCandidate> for SkippedJson {
    fn from(skipped: &SkippedCandidate) -> Self {
        Self {
            target_token_address: skipped.target_token_address.to_string(),
            token_account: skipped.token_account.to_string(),
            owner_wallet: skipped.owner_wallet.map(|wallet| wallet.to_string()),
            rank: skipped.rank,
            reason: format!("{:?}", skipped.reason),
        }
    }
}

impl SkippedJson {
    fn into_skipped_candidate(self, path: &Path) -> Result<SkippedCandidate, PlanError> {
        Ok(SkippedCandidate {
            target_token_address: parse_plan_pubkey(
                path,
                "skipped.target_token_address",
                &self.target_token_address,
            )?,
            token_account: parse_plan_pubkey(path, "skipped.token_account", &self.token_account)?,
            owner_wallet: self
                .owner_wallet
                .as_deref()
                .map(|wallet| parse_plan_pubkey(path, "skipped.owner_wallet", wallet))
                .transpose()?,
            rank: self.rank,
            reason: parse_skip_reason(path, &self.reason)?,
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct BatchJson {
    index: usize,
    recipient_count: usize,
    ata_creations: usize,
    metrics: TransactionMetrics,
    recipients: Vec<BatchRecipientJson>,
}

impl From<&PlannedBatch> for BatchJson {
    fn from(batch: &PlannedBatch) -> Self {
        Self {
            index: batch.index,
            recipient_count: batch.recipient_count,
            ata_creations: batch.ata_creations,
            metrics: batch.metrics,
            recipients: batch
                .recipients
                .iter()
                .map(BatchRecipientJson::from)
                .collect(),
        }
    }
}

impl BatchJson {
    fn into_planned_batch(self, path: &Path) -> Result<PlannedBatch, PlanError> {
        let recipients = self
            .recipients
            .into_iter()
            .map(|recipient| recipient.into_planned_batch_recipient(path))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(PlannedBatch {
            index: self.index,
            recipient_count: self.recipient_count,
            ata_creations: self.ata_creations,
            metrics: self.metrics,
            recipients,
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct BatchRecipientJson {
    recipient_index: usize,
    wallet: String,
    recipient_ata: String,
    create_recipient_ata: bool,
}

impl From<&PlannedBatchRecipient> for BatchRecipientJson {
    fn from(recipient: &PlannedBatchRecipient) -> Self {
        Self {
            recipient_index: recipient.recipient_index,
            wallet: recipient.wallet.to_string(),
            recipient_ata: recipient.recipient_ata.to_string(),
            create_recipient_ata: recipient.create_recipient_ata,
        }
    }
}

impl BatchRecipientJson {
    fn into_planned_batch_recipient(self, path: &Path) -> Result<PlannedBatchRecipient, PlanError> {
        Ok(PlannedBatchRecipient {
            recipient_index: self.recipient_index,
            wallet: parse_plan_pubkey(path, "batches.recipients.wallet", &self.wallet)?,
            recipient_ata: parse_plan_pubkey(
                path,
                "batches.recipients.recipient_ata",
                &self.recipient_ata,
            )?,
            create_recipient_ata: self.create_recipient_ata,
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ArtifactPathsJson {
    run_id: String,
    run_dir: String,
    plan_path: String,
    recipients_path: String,
    skipped_path: String,
    ledger_path: String,
    simulation_path: String,
}

fn parse_plan_pubkey(path: &Path, field: &'static str, value: &str) -> Result<Pubkey, PlanError> {
    Pubkey::from_str(value).map_err(|_| PlanError::InvalidPlanPubkey {
        path: path.display().to_string(),
        field,
        value: value.to_owned(),
    })
}

fn parse_skip_reason(path: &Path, value: &str) -> Result<SkipReason, PlanError> {
    let reason = match value {
        "MissingTokenAccount" => SkipReason::MissingTokenAccount,
        "MalformedTokenAccount" => SkipReason::MalformedTokenAccount,
        "ZeroBalance" => SkipReason::ZeroBalance,
        "TokenMintMismatch" => SkipReason::TokenMintMismatch,
        "UnsupportedTokenProgram" => SkipReason::UnsupportedTokenProgram,
        "ExecutableTokenAccount" => SkipReason::ExecutableTokenAccount,
        "SourceWallet" => SkipReason::SourceWallet,
        "ManualExclude" => SkipReason::ManualExclude,
        "OffCurveOwner" => SkipReason::OffCurveOwner,
        "ExecutableOwner" => SkipReason::ExecutableOwner,
        "ProgramOwnedOwner" => SkipReason::ProgramOwnedOwner,
        "ExistingDistributionHolder" => SkipReason::ExistingDistributionHolder,
        "ExistingDistributionAccount" => SkipReason::ExistingDistributionAccount,
        "RecipientLimit" => SkipReason::RecipientLimit,
        _ => {
            return Err(PlanError::InvalidPlanSkipReason {
                path: path.display().to_string(),
                value: value.to_owned(),
            });
        }
    };

    Ok(reason)
}

impl From<&PlanArtifacts> for ArtifactPathsJson {
    fn from(artifacts: &PlanArtifacts) -> Self {
        Self {
            run_id: artifacts.run_id.clone(),
            run_dir: artifacts.run_dir.display().to_string(),
            plan_path: artifacts.plan_path.display().to_string(),
            recipients_path: artifacts.recipients_path.display().to_string(),
            skipped_path: artifacts.skipped_path.display().to_string(),
            ledger_path: artifacts.ledger_path.display().to_string(),
            simulation_path: artifacts.simulation_path.display().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            config::{ValidatedConfig, ValidatedSolscanConfig},
            discovery::{SkipReason, TokenMetadata, token_2022_program_id, token_program_id},
            rpc::{RpcAccount, RpcTokenAccount, TokenAccountBalance},
        },
        solana_keypair::{Keypair, Signer},
        std::{collections::HashMap, fs},
    };

    #[derive(Default)]
    struct MockRpc {
        accounts: HashMap<Pubkey, RpcAccount>,
        rent_lamports: u64,
    }

    impl RpcReader for MockRpc {
        fn get_account(&self, address: &Pubkey) -> Result<Option<RpcAccount>, RpcError> {
            Ok(self.accounts.get(address).cloned())
        }

        fn get_token_largest_accounts(
            &self,
            _mint: &Pubkey,
        ) -> Result<Vec<TokenAccountBalance>, RpcError> {
            Ok(Vec::new())
        }

        fn get_token_accounts_by_mint(
            &self,
            _mint: &Pubkey,
            _token_program: &Pubkey,
        ) -> Result<Vec<RpcTokenAccount>, RpcError> {
            Ok(Vec::new())
        }

        fn get_minimum_balance_for_rent_exemption(
            &self,
            _data_len: usize,
        ) -> Result<u64, RpcError> {
            Ok(if self.rent_lamports == 0 {
                2_074_080
            } else {
                self.rent_lamports
            })
        }
    }

    #[test]
    fn calculates_amounts_and_remainder() {
        let fixture = PlanFixture::new(3, ExistingAta::All);
        let report = fixture.report("10.01", 2);
        let config = fixture.config("10.01");

        let plan = create_distribution_plan(&fixture.rpc, &config, report, &fixture.source_wallet)
            .unwrap();

        assert_eq!(plan.total_amount_raw, 1_001);
        assert_eq!(plan.amount_per_recipient_raw, 333);
        assert_eq!(plan.amount_per_recipient_ui, "3.33");
        assert_eq!(plan.remainder_raw, 2);
        assert_eq!(plan.remainder_ui, "0.02");
        assert_eq!(plan.recipients.len(), 3);
        assert_eq!(plan.ata_creations(), 0);
    }

    #[test]
    fn rejects_fractional_amount_beyond_token_decimals() {
        let fixture = PlanFixture::new(1, ExistingAta::All);
        let report = fixture.report("1.001", 2);
        let config = fixture.config("1.001");

        let err = create_distribution_plan(&fixture.rpc, &config, report, &fixture.source_wallet)
            .unwrap_err();

        assert!(matches!(
            err,
            PlanError::TooManyFractionalDigits {
                amount,
                decimals: 2
            } if amount == "1.001"
        ));
    }

    #[test]
    fn fails_when_source_balance_is_insufficient() {
        let mut fixture = PlanFixture::new(2, ExistingAta::All);
        fixture.set_source_balance("99");
        let report = fixture.report("1.00", 2);
        let config = fixture.config("1.00");

        let err = create_distribution_plan(&fixture.rpc, &config, report, &fixture.source_wallet)
            .unwrap_err();

        assert!(matches!(
            err,
            PlanError::InsufficientSourceBalance {
                available_raw: 99,
                required_raw: 100,
                ..
            }
        ));
    }

    #[test]
    fn rejects_duplicate_recipients_in_report() {
        let fixture = PlanFixture::new(1, ExistingAta::All);
        let mut report = fixture.report("1", 2);
        report.recipients.push(report.recipients[0].clone());
        let config = fixture.config("1");

        let err = create_distribution_plan(&fixture.rpc, &config, report, &fixture.source_wallet)
            .unwrap_err();

        assert!(matches!(err, PlanError::DuplicateRecipient { .. }));
    }

    #[test]
    fn includes_missing_ata_creation_in_plan() {
        let fixture = PlanFixture::new(2, ExistingAta::None);
        let report = fixture.report("2", 2);
        let config = fixture.config("2");

        let plan = create_distribution_plan(&fixture.rpc, &config, report, &fixture.source_wallet)
            .unwrap();

        assert_eq!(plan.ata_creations(), 2);
        assert_eq!(plan.estimated_ata_rent_lamports, 4_148_160);
        assert_eq!(plan.batches[0].ata_creations, 2);
        assert!(plan.batches[0].metrics.top_level_instruction_count >= 4);
    }

    #[test]
    fn splits_batches_to_transaction_limits() {
        let fixture = PlanFixture::new(30, ExistingAta::None);
        let report = fixture.report("30", 2);
        let config = fixture.config("30");

        let plan = create_distribution_plan(&fixture.rpc, &config, report, &fixture.source_wallet)
            .unwrap();

        assert!(plan.batches.len() > 1);
        assert_eq!(
            plan.batches
                .iter()
                .map(|batch| batch.recipient_count)
                .sum::<usize>(),
            30
        );
        for batch in &plan.batches {
            assert!(batch.metrics.within_limits());
            assert!(batch.metrics.serialized_size <= LEGACY_TRANSACTION_SIZE_LIMIT);
            assert!(batch.metrics.account_locks <= ACCOUNT_LOCK_LIMIT);
            assert!(
                batch.metrics.estimated_executed_instruction_count <= EXECUTED_INSTRUCTION_LIMIT
            );
        }
    }

    #[test]
    fn writes_plan_artifacts() {
        let fixture = PlanFixture::new(2, ExistingAta::All);
        let report = fixture.report("2", 2);
        let config = fixture.config("2");
        let plan = create_distribution_plan(&fixture.rpc, &config, report, &fixture.source_wallet)
            .unwrap();
        let runs_dir = std::env::temp_dir().join(format!("airdrop-plan-test-{}", pubkey()));

        let artifacts = write_plan_artifacts(&plan, &config, &runs_dir).unwrap();

        assert!(artifacts.plan_path.exists());
        assert!(artifacts.recipients_path.exists());
        assert!(artifacts.skipped_path.exists());
        assert!(artifacts.ledger_path.exists());
        assert!(
            fs::read_to_string(&artifacts.plan_path)
                .unwrap()
                .contains("\"config\"")
        );
        assert!(
            fs::read_to_string(&artifacts.plan_path)
                .unwrap()
                .contains("\"ledger_path\"")
        );
        assert!(
            fs::read_to_string(&artifacts.plan_path)
                .unwrap()
                .contains("\"simulation_path\"")
        );
        let (loaded_plan, loaded_artifacts) = read_plan_artifacts(&artifacts.run_dir).unwrap();
        assert_eq!(loaded_plan.source_wallet, plan.source_wallet);
        assert_eq!(
            loaded_plan.distribution_token_address,
            plan.distribution_token_address
        );
        assert_eq!(loaded_plan.recipients, plan.recipients);
        assert_eq!(loaded_plan.batches, plan.batches);
        assert_eq!(loaded_artifacts.ledger_path, artifacts.ledger_path);
        validate_cached_plan_config(&artifacts.run_dir, &config).unwrap();

        let mismatch =
            validate_cached_plan_config(&artifacts.run_dir, &fixture.config("3")).unwrap_err();
        assert!(matches!(
            mismatch,
            PlanError::PlanConfigMismatch {
                field: "distribution.total_amount_ui",
                ..
            }
        ));

        fs::remove_dir_all(runs_dir).unwrap();
    }

    enum ExistingAta {
        All,
        None,
    }

    struct PlanFixture {
        rpc: MockRpc,
        source_wallet: Pubkey,
        distribution_token: Pubkey,
        target_token: Pubkey,
        recipients: Vec<Pubkey>,
    }

    impl PlanFixture {
        fn new(recipient_count: usize, existing_ata: ExistingAta) -> Self {
            let source_wallet = pubkey();
            let distribution_token = pubkey();
            let target_token = pubkey();
            let source_ata = derive_associated_token_account(
                &source_wallet,
                &distribution_token,
                &token_2022_program_id(),
            );
            let mut rpc = MockRpc {
                rent_lamports: 2_074_080,
                ..MockRpc::default()
            };
            rpc.accounts.insert(
                source_ata,
                token_account(distribution_token, source_wallet, "1000000000000", 2),
            );

            let mut recipients = Vec::with_capacity(recipient_count);
            for _ in 0..recipient_count {
                let recipient = pubkey();
                let recipient_ata = derive_associated_token_account(
                    &recipient,
                    &distribution_token,
                    &token_2022_program_id(),
                );
                if matches!(existing_ata, ExistingAta::All) {
                    rpc.accounts.insert(
                        recipient_ata,
                        token_account(distribution_token, recipient, "0", 2),
                    );
                }
                recipients.push(recipient);
            }

            Self {
                rpc,
                source_wallet,
                distribution_token,
                target_token,
                recipients,
            }
        }

        fn set_source_balance(&mut self, amount: &str) {
            let source_ata = derive_associated_token_account(
                &self.source_wallet,
                &self.distribution_token,
                &token_2022_program_id(),
            );
            self.rpc.accounts.insert(
                source_ata,
                token_account(self.distribution_token, self.source_wallet, amount, 2),
            );
        }

        fn config(&self, amount: &str) -> ValidatedConfig {
            ValidatedConfig {
                cluster_name: "mainnet-beta".to_owned(),
                rpc_url_env: "SOLANA_RPC_URL".to_owned(),
                distribution_token_address: self.distribution_token,
                total_amount_ui: amount.to_owned(),
                target_token_addresses: vec![self.target_token],
                max_recipients: self.recipients.len().max(1),
                manual_exclude_wallets: vec![],
                solscan: ValidatedSolscanConfig {
                    enabled: false,
                    api_key_env: "SOLSCAN_API_KEY".to_owned(),
                    holder_fetch_limit: 100,
                },
            }
        }

        fn report(&self, amount: &str, decimals: u8) -> DiscoveryReport {
            DiscoveryReport {
                distribution_token: TokenMetadata {
                    token_address: self.distribution_token,
                    token_program: token_2022_program_id(),
                    decimals,
                    supply: "1000000000000".to_owned(),
                },
                target_tokens: vec![TokenMetadata {
                    token_address: self.target_token,
                    token_program: token_2022_program_id(),
                    decimals: 0,
                    supply: "1000000".to_owned(),
                }],
                recipients: self
                    .recipients
                    .iter()
                    .enumerate()
                    .map(|(index, wallet)| DiscoveredRecipient {
                        wallet: *wallet,
                        best_rank: index + 1,
                        holdings: vec![TargetHolding {
                            target_token_address: self.target_token,
                            token_account: pubkey(),
                            raw_amount: amount.to_owned(),
                            decimals,
                            rank: index + 1,
                        }],
                    })
                    .collect(),
                skipped: vec![SkippedCandidate {
                    target_token_address: self.target_token,
                    token_account: pubkey(),
                    owner_wallet: Some(pubkey()),
                    rank: 99,
                    reason: SkipReason::ZeroBalance,
                }],
            }
        }
    }

    fn token_account(mint: Pubkey, owner: Pubkey, amount: &str, decimals: u8) -> RpcAccount {
        RpcAccount {
            owner_program: token_2022_program_id(),
            executable: false,
            parsed: Some(ParsedAccount::TokenAccount {
                mint,
                owner,
                amount: amount.to_owned(),
                decimals,
            }),
        }
    }

    fn pubkey() -> Pubkey {
        Keypair::new().pubkey()
    }

    #[test]
    fn derives_different_atas_for_different_token_programs() {
        let wallet = pubkey();
        let mint = pubkey();

        assert_ne!(
            derive_associated_token_account(&wallet, &mint, &token_program_id()),
            derive_associated_token_account(&wallet, &mint, &token_2022_program_id())
        );
    }
}
