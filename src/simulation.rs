use {
    crate::{
        discovery::system_program_id,
        planning::{
            DistributionPlan, PlanArtifacts, PlannedBatch, TransactionMetrics,
            associated_token_program_id,
        },
        rpc::{RpcError, RpcSimulator, TransactionSimulation},
    },
    base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD},
    serde::Serialize,
    solana_hash::Hash,
    solana_instruction::{AccountMeta, Instruction},
    solana_transaction::Transaction,
    std::{
        fs::File,
        io::Write,
        path::{Path, PathBuf},
    },
    thiserror::Error,
};

pub fn simulate_plan(
    rpc: &impl RpcSimulator,
    plan: &DistributionPlan,
) -> Result<SimulationReport, SimulationError> {
    eprintln!(
        "Fetching latest blockhash for {} planned transaction simulation(s)",
        plan.batches.len()
    );
    let recent_blockhash = rpc.get_latest_blockhash()?;
    let mut batches = Vec::with_capacity(plan.batches.len());

    for batch in &plan.batches {
        eprintln!(
            "Simulating batch {}/{}: recipients={}, ata_creations={}",
            batch.index + 1,
            plan.batches.len(),
            batch.recipient_count,
            batch.ata_creations
        );
        let built = build_unsigned_batch_transaction(plan, batch, &recent_blockhash)?;
        let result = rpc.simulate_transaction(&built.encoded_transaction)?;

        batches.push(BatchSimulation {
            batch_index: batch.index,
            recipient_count: batch.recipient_count,
            ata_creations: batch.ata_creations,
            metrics: built.metrics,
            result,
        });
    }

    Ok(SimulationReport {
        latest_blockhash: recent_blockhash.to_string(),
        batches,
    })
}

pub fn write_simulation_artifact(
    report: &SimulationReport,
    artifacts: &PlanArtifacts,
) -> Result<PathBuf, SimulationError> {
    write_json(&artifacts.simulation_path, report)?;
    Ok(artifacts.simulation_path.clone())
}

fn build_unsigned_batch_transaction(
    plan: &DistributionPlan,
    batch: &PlannedBatch,
    recent_blockhash: &Hash,
) -> Result<BuiltBatchTransaction, SimulationError> {
    let instructions = build_batch_instructions(plan, batch)?;
    let mut transaction = Transaction::new_with_payer(&instructions, Some(&plan.source_wallet));
    transaction.message.recent_blockhash = recent_blockhash.clone();
    let serialized_transaction =
        bincode::serialize(&transaction).map_err(|source| SimulationError::Serialize {
            batch_index: batch.index,
            source,
        })?;
    let metrics = transaction_metrics(&transaction, serialized_transaction.len(), batch);

    if !metrics.within_limits() {
        return Err(SimulationError::TransactionLimitExceeded {
            batch_index: batch.index,
            metrics,
        });
    }

    Ok(BuiltBatchTransaction {
        encoded_transaction: BASE64_STANDARD.encode(serialized_transaction),
        metrics,
    })
}

fn build_batch_instructions(
    plan: &DistributionPlan,
    batch: &PlannedBatch,
) -> Result<Vec<Instruction>, SimulationError> {
    let mut instructions = Vec::with_capacity(batch.recipient_count + batch.ata_creations);

    for batch_recipient in &batch.recipients {
        let recipient = plan.recipients.get(batch_recipient.recipient_index).ok_or(
            SimulationError::UnknownRecipientIndex {
                batch_index: batch.index,
                recipient_index: batch_recipient.recipient_index,
            },
        )?;

        if batch_recipient.create_recipient_ata {
            instructions.push(create_associated_token_account_idempotent_instruction(
                &plan.source_wallet,
                &batch_recipient.wallet,
                &plan.distribution_token_address,
                &batch_recipient.recipient_ata,
                &plan.distribution_token_program,
            ));
        }

        instructions.push(
            spl_token_2022_interface::instruction::transfer_checked(
                &plan.distribution_token_program,
                &plan.source_ata,
                &plan.distribution_token_address,
                &batch_recipient.recipient_ata,
                &plan.source_wallet,
                &[],
                recipient.amount_raw,
                plan.distribution_decimals,
            )
            .map_err(|source| SimulationError::TransferInstruction {
                batch_index: batch.index,
                recipient_index: batch_recipient.recipient_index,
                message: source.to_string(),
            })?,
        );
    }

    Ok(instructions)
}

fn create_associated_token_account_idempotent_instruction(
    payer: &solana_pubkey::Pubkey,
    wallet: &solana_pubkey::Pubkey,
    mint: &solana_pubkey::Pubkey,
    associated_token_account: &solana_pubkey::Pubkey,
    token_program: &solana_pubkey::Pubkey,
) -> Instruction {
    Instruction {
        program_id: associated_token_program_id(),
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(*associated_token_account, false),
            AccountMeta::new_readonly(*wallet, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program_id(), false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

fn transaction_metrics(
    transaction: &Transaction,
    serialized_size: usize,
    batch: &PlannedBatch,
) -> TransactionMetrics {
    TransactionMetrics {
        serialized_size,
        account_locks: transaction.message.account_keys.len(),
        top_level_instruction_count: transaction.message.instructions.len(),
        estimated_executed_instruction_count: batch.metrics.estimated_executed_instruction_count,
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), SimulationError> {
    let mut contents = serde_json::to_string_pretty(value)?;
    contents.push('\n');
    let mut file = File::create(path).map_err(|source| SimulationError::Io {
        path: path.display().to_string(),
        source,
    })?;
    file.write_all(contents.as_bytes())
        .map_err(|source| SimulationError::Io {
            path: path.display().to_string(),
            source,
        })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SimulationReport {
    pub latest_blockhash: String,
    pub batches: Vec<BatchSimulation>,
}

impl SimulationReport {
    pub fn succeeded_batch_count(&self) -> usize {
        self.batches
            .iter()
            .filter(|batch| batch.result.err.is_none())
            .count()
    }

    pub fn failed_batch_count(&self) -> usize {
        self.batches
            .iter()
            .filter(|batch| batch.result.err.is_some())
            .count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchSimulation {
    pub batch_index: usize,
    pub recipient_count: usize,
    pub ata_creations: usize,
    pub metrics: TransactionMetrics,
    pub result: TransactionSimulation,
}

#[derive(Debug)]
struct BuiltBatchTransaction {
    encoded_transaction: String,
    metrics: TransactionMetrics,
}

#[derive(Debug, Error)]
pub enum SimulationError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("failed to serialize planned batch {batch_index} transaction: {source}")]
    Serialize {
        batch_index: usize,
        #[source]
        source: bincode::Error,
    },
    #[error(
        "planned batch {batch_index} transaction exceeds legacy transaction limits: {metrics:?}"
    )]
    TransactionLimitExceeded {
        batch_index: usize,
        metrics: TransactionMetrics,
    },
    #[error("planned batch {batch_index} references missing recipient index {recipient_index}")]
    UnknownRecipientIndex {
        batch_index: usize,
        recipient_index: usize,
    },
    #[error(
        "failed to build TransferChecked instruction for batch {batch_index} recipient {recipient_index}: {message}"
    )]
    TransferInstruction {
        batch_index: usize,
        recipient_index: usize,
        message: String,
    },
    #[error("failed to write simulation artifact `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            discovery::token_2022_program_id,
            planning::{PlannedBatchRecipient, PlannedRecipient, derive_associated_token_account},
        },
        solana_keypair::{Keypair, Signer},
    };

    #[test]
    fn builds_idempotent_ata_create_and_transfer_checked_instructions() {
        let fixture = SimulationFixture::new(1, true);
        let instructions = build_batch_instructions(&fixture.plan, &fixture.plan.batches[0])
            .expect("valid instructions");

        assert_eq!(instructions.len(), 2);
        assert_eq!(instructions[0].program_id, associated_token_program_id());
        assert_eq!(instructions[0].data, vec![1]);
        assert_eq!(instructions[0].accounts.len(), 6);
        assert!(instructions[0].accounts[0].is_signer);
        assert!(instructions[0].accounts[0].is_writable);
        assert_eq!(
            instructions[0].accounts[5].pubkey,
            fixture.plan.distribution_token_program
        );

        assert_eq!(instructions[1].program_id, token_2022_program_id());
        assert_eq!(instructions[1].accounts.len(), 4);
        assert_eq!(instructions[1].accounts[0].pubkey, fixture.plan.source_ata);
        assert_eq!(
            instructions[1].accounts[2].pubkey,
            fixture.plan.recipients[0].recipient_ata
        );
        assert!(instructions[1].accounts[3].is_signer);
        assert_eq!(instructions[1].data.len(), 10);
    }

    #[test]
    fn builds_unsigned_transaction_with_real_serialized_metrics() {
        let fixture = SimulationFixture::new(2, false);
        let built = build_unsigned_batch_transaction(
            &fixture.plan,
            &fixture.plan.batches[0],
            &Hash::default(),
        )
        .expect("valid transaction");

        assert!(built.metrics.within_limits());
        assert!(built.metrics.serialized_size > 0);
        assert_eq!(built.metrics.top_level_instruction_count, 2);
        assert_eq!(built.metrics.account_locks, 6);
        assert!(!built.encoded_transaction.is_empty());
    }

    #[test]
    fn rejects_oversized_legacy_transaction() {
        let fixture = SimulationFixture::new(40, true);
        let err = build_unsigned_batch_transaction(
            &fixture.plan,
            &fixture.plan.batches[0],
            &Hash::default(),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            SimulationError::TransactionLimitExceeded { .. }
        ));
    }

    struct SimulationFixture {
        plan: DistributionPlan,
    }

    impl SimulationFixture {
        fn new(recipient_count: usize, create_atas: bool) -> Self {
            let source_wallet = Keypair::new().pubkey();
            let distribution_token = Keypair::new().pubkey();
            let distribution_token_program = token_2022_program_id();
            let source_ata = derive_associated_token_account(
                &source_wallet,
                &distribution_token,
                &distribution_token_program,
            );
            let mut recipients = Vec::with_capacity(recipient_count);
            let mut batch_recipients = Vec::with_capacity(recipient_count);

            for index in 0..recipient_count {
                let wallet = Keypair::new().pubkey();
                let recipient_ata = derive_associated_token_account(
                    &wallet,
                    &distribution_token,
                    &distribution_token_program,
                );
                recipients.push(PlannedRecipient {
                    wallet,
                    recipient_ata,
                    amount_raw: 10,
                    amount_ui: "0.00000001".to_owned(),
                    create_recipient_ata: create_atas,
                    best_rank: index + 1,
                    holdings: vec![],
                });
                batch_recipients.push(PlannedBatchRecipient {
                    recipient_index: index,
                    wallet,
                    recipient_ata,
                    create_recipient_ata: create_atas,
                });
            }

            let ata_creations = if create_atas { recipient_count } else { 0 };
            let estimated_executed_instruction_count = recipient_count + ata_creations * 4;
            let batch = PlannedBatch {
                index: 0,
                recipient_count,
                ata_creations,
                metrics: TransactionMetrics {
                    serialized_size: 0,
                    account_locks: 0,
                    top_level_instruction_count: recipient_count + ata_creations,
                    estimated_executed_instruction_count,
                },
                recipients: batch_recipients,
            };

            Self {
                plan: DistributionPlan {
                    cluster_name: "mainnet-beta".to_owned(),
                    source_wallet,
                    source_ata,
                    source_balance_raw: 1_000_000,
                    source_balance_ui: "0.001".to_owned(),
                    distribution_token_address: distribution_token,
                    distribution_token_program,
                    distribution_decimals: 9,
                    total_amount_raw: recipient_count as u64 * 10,
                    total_amount_ui: "0.000001".to_owned(),
                    amount_per_recipient_raw: 10,
                    amount_per_recipient_ui: "0.00000001".to_owned(),
                    remainder_raw: 0,
                    remainder_ui: "0".to_owned(),
                    recipients,
                    skipped: vec![],
                    batches: vec![batch],
                    rent_per_ata_lamports: 2_039_280,
                    estimated_ata_rent_lamports: 2_039_280_u64.saturating_mul(ata_creations as u64),
                    estimated_signature_fee_lamports: 5_000,
                },
            }
        }
    }
}
