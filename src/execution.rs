use {
    crate::{
        planning::{DistributionPlan, PlanArtifacts, PlannedBatch, PlannedRecipient},
        rpc::{RpcError, RpcSender, RpcSimulator, SignatureStatus},
        simulation::{SimulationError, build_signed_batch_transaction},
    },
    serde::Serialize,
    solana_keypair::Keypair,
    std::{
        fs::{File, OpenOptions},
        io::Write,
        path::Path,
        thread::sleep,
        time::Duration,
    },
    thiserror::Error,
};

const CONFIRMATION_MAX_ATTEMPTS: usize = 60;
const CONFIRMATION_POLL_DELAY: Duration = Duration::from_secs(2);

pub fn required_sol_lamports(plan: &DistributionPlan) -> u64 {
    plan.estimated_signature_fee_lamports
        .saturating_add(plan.estimated_ata_rent_lamports)
}

pub fn confirmation_phrase(plan: &DistributionPlan) -> String {
    format!("SEND {}", plan.recipients.len())
}

pub fn confirmation_matches(input: &str, plan: &DistributionPlan) -> bool {
    input.trim() == confirmation_phrase(plan)
}

pub fn check_source_sol_funding(
    rpc: &impl RpcSender,
    plan: &DistributionPlan,
) -> Result<FundingStatus, SendError> {
    let available_lamports = rpc.get_balance(&plan.source_wallet)?;
    let required_lamports = required_sol_lamports(plan);
    Ok(FundingStatus {
        available_lamports,
        required_lamports,
        shortfall_lamports: required_lamports.saturating_sub(available_lamports),
    })
}

pub fn send_plan<R: RpcSender + RpcSimulator>(
    rpc: &R,
    plan: &DistributionPlan,
    artifacts: &PlanArtifacts,
    source_keypair: &Keypair,
) -> Result<SendReport, SendError> {
    let mut sent_batches = Vec::with_capacity(plan.batches.len());

    for batch in &plan.batches {
        eprintln!(
            "Sending batch {}/{}: recipients={}, ata_creations={}",
            batch.index + 1,
            plan.batches.len(),
            batch.recipient_count,
            batch.ata_creations
        );
        let recent_blockhash = rpc.get_latest_blockhash()?;
        let built = build_signed_batch_transaction(plan, batch, &recent_blockhash, source_keypair)?;
        let expected_signature =
            built
                .signature
                .clone()
                .ok_or(SendError::MissingBuiltSignature {
                    batch_index: batch.index,
                })?;
        let signature = rpc.send_transaction(&built.encoded_transaction)?;

        append_ledger_entry(
            &artifacts.ledger_path,
            &LedgerEntry::BatchSubmitted {
                batch_index: batch.index,
                signature: signature.clone(),
                expected_signature,
                recipient_count: batch.recipient_count,
                ata_creations: batch.ata_creations,
            },
        )?;

        let status = wait_for_confirmation(rpc, &signature)?;
        if let Some(err) = status.err {
            append_batch_failure_entries(
                plan,
                batch,
                &artifacts.ledger_path,
                &signature,
                err.to_string(),
            )?;
            return Err(SendError::TransactionFailed {
                batch_index: batch.index,
                signature,
                err,
            });
        }

        append_ledger_entry(
            &artifacts.ledger_path,
            &LedgerEntry::BatchConfirmed {
                batch_index: batch.index,
                signature: signature.clone(),
                slot: status.slot,
                recipient_count: batch.recipient_count,
            },
        )?;
        append_recipient_confirmed_entries(plan, batch, &artifacts.ledger_path, &signature)?;

        sent_batches.push(SentBatch {
            batch_index: batch.index,
            signature,
            slot: status.slot,
            recipient_count: batch.recipient_count,
            ata_creations: batch.ata_creations,
        });
    }

    Ok(SendReport {
        batch_count: sent_batches.len(),
        recipient_count: sent_batches.iter().map(|batch| batch.recipient_count).sum(),
        batches: sent_batches,
    })
}

fn wait_for_confirmation(
    rpc: &impl RpcSender,
    signature: &str,
) -> Result<SignatureStatus, SendError> {
    for _ in 0..CONFIRMATION_MAX_ATTEMPTS {
        if let Some(status) = rpc.get_signature_status(signature)?
            && (status.err.is_some() || signature_status_is_confirmed(&status))
        {
            return Ok(status);
        }
        sleep(CONFIRMATION_POLL_DELAY);
    }

    Err(SendError::ConfirmationTimedOut {
        signature: signature.to_owned(),
    })
}

fn signature_status_is_confirmed(status: &SignatureStatus) -> bool {
    matches!(
        status.confirmation_status.as_deref(),
        Some("confirmed" | "finalized")
    )
}

fn append_recipient_confirmed_entries(
    plan: &DistributionPlan,
    batch: &PlannedBatch,
    ledger_path: &Path,
    signature: &str,
) -> Result<(), SendError> {
    for batch_recipient in &batch.recipients {
        let recipient = plan.recipients.get(batch_recipient.recipient_index).ok_or(
            SendError::UnknownRecipientIndex {
                batch_index: batch.index,
                recipient_index: batch_recipient.recipient_index,
            },
        )?;
        append_ledger_entry(
            ledger_path,
            &LedgerEntry::RecipientConfirmed {
                batch_index: batch.index,
                signature: signature.to_owned(),
                recipient: RecipientLedgerJson::from_recipient(recipient),
            },
        )?;
    }

    Ok(())
}

fn append_batch_failure_entries(
    plan: &DistributionPlan,
    batch: &PlannedBatch,
    ledger_path: &Path,
    signature: &str,
    err: String,
) -> Result<(), SendError> {
    append_ledger_entry(
        ledger_path,
        &LedgerEntry::BatchFailed {
            batch_index: batch.index,
            signature: signature.to_owned(),
            err: err.clone(),
            recipient_count: batch.recipient_count,
        },
    )?;

    for batch_recipient in &batch.recipients {
        let recipient = plan.recipients.get(batch_recipient.recipient_index).ok_or(
            SendError::UnknownRecipientIndex {
                batch_index: batch.index,
                recipient_index: batch_recipient.recipient_index,
            },
        )?;
        append_ledger_entry(
            ledger_path,
            &LedgerEntry::RecipientFailed {
                batch_index: batch.index,
                signature: signature.to_owned(),
                err: err.clone(),
                recipient: RecipientLedgerJson::from_recipient(recipient),
            },
        )?;
    }

    Ok(())
}

fn append_ledger_entry(path: &Path, entry: &LedgerEntry) -> Result<(), SendError> {
    let mut file = open_ledger(path)?;
    serde_json::to_writer(&mut file, entry)?;
    file.write_all(b"\n").map_err(|source| SendError::Io {
        path: path.display().to_string(),
        source,
    })
}

fn open_ledger(path: &Path) -> Result<File, SendError> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| SendError::Io {
            path: path.display().to_string(),
            source,
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FundingStatus {
    pub available_lamports: u64,
    pub required_lamports: u64,
    pub shortfall_lamports: u64,
}

impl FundingStatus {
    pub fn is_funded(self) -> bool {
        self.shortfall_lamports == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendReport {
    pub batch_count: usize,
    pub recipient_count: usize,
    pub batches: Vec<SentBatch>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentBatch {
    pub batch_index: usize,
    pub signature: String,
    pub slot: u64,
    pub recipient_count: usize,
    pub ata_creations: usize,
}

#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum LedgerEntry {
    BatchSubmitted {
        batch_index: usize,
        signature: String,
        expected_signature: String,
        recipient_count: usize,
        ata_creations: usize,
    },
    BatchConfirmed {
        batch_index: usize,
        signature: String,
        slot: u64,
        recipient_count: usize,
    },
    BatchFailed {
        batch_index: usize,
        signature: String,
        err: String,
        recipient_count: usize,
    },
    RecipientConfirmed {
        batch_index: usize,
        signature: String,
        recipient: RecipientLedgerJson,
    },
    RecipientFailed {
        batch_index: usize,
        signature: String,
        err: String,
        recipient: RecipientLedgerJson,
    },
}

#[derive(Debug, Serialize)]
struct RecipientLedgerJson {
    wallet: String,
    recipient_ata: String,
    amount_raw: u64,
    amount_ui: String,
    created_recipient_ata: bool,
}

impl RecipientLedgerJson {
    fn from_recipient(recipient: &PlannedRecipient) -> Self {
        Self {
            wallet: recipient.wallet.to_string(),
            recipient_ata: recipient.recipient_ata.to_string(),
            amount_raw: recipient.amount_raw,
            amount_ui: recipient.amount_ui.clone(),
            created_recipient_ata: recipient.create_recipient_ata,
        }
    }
}

#[derive(Debug, Error)]
pub enum SendError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(transparent)]
    Build(#[from] SimulationError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("signed batch {batch_index} transaction did not include a signature")]
    MissingBuiltSignature { batch_index: usize },
    #[error("timed out waiting for signature `{signature}` to reach confirmed")]
    ConfirmationTimedOut { signature: String },
    #[error("batch {batch_index} transaction `{signature}` failed: {err}")]
    TransactionFailed {
        batch_index: usize,
        signature: String,
        err: serde_json::Value,
    },
    #[error("batch {batch_index} references missing recipient index {recipient_index}")]
    UnknownRecipientIndex {
        batch_index: usize,
        recipient_index: usize,
    },
    #[error("failed to write ledger `{path}`: {source}")]
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
            planning::{
                DistributionPlan, PlanArtifacts, PlannedBatch, PlannedBatchRecipient,
                TransactionMetrics, derive_associated_token_account,
            },
            rpc::{RpcSimulator, TransactionSimulation},
        },
        base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD},
        serde_json::Value,
        solana_hash::Hash,
        solana_keypair::{Keypair, Signer},
        solana_pubkey::Pubkey,
        solana_transaction::Transaction,
        std::fs,
    };

    #[test]
    fn calculates_required_sol_lamports() {
        let mut plan = empty_plan();
        plan.estimated_signature_fee_lamports = 10;
        plan.estimated_ata_rent_lamports = 90;

        assert_eq!(required_sol_lamports(&plan), 100);
    }

    #[test]
    fn validates_confirmation_phrase() {
        let mut plan = empty_plan();
        plan.recipients = vec![];

        assert_eq!(confirmation_phrase(&plan), "SEND 0");
        assert!(confirmation_matches("SEND 0\n", &plan));
        assert!(!confirmation_matches("send 0", &plan));
    }

    #[test]
    fn identifies_confirmed_signature_statuses() {
        assert!(signature_status_is_confirmed(&SignatureStatus {
            slot: 1,
            confirmation_status: Some("confirmed".to_owned()),
            err: None,
        }));
        assert!(signature_status_is_confirmed(&SignatureStatus {
            slot: 1,
            confirmation_status: Some("finalized".to_owned()),
            err: None,
        }));
        assert!(!signature_status_is_confirmed(&SignatureStatus {
            slot: 1,
            confirmation_status: Some("processed".to_owned()),
            err: None,
        }));
    }

    #[test]
    fn writes_jsonl_ledger_entries() {
        let path = std::env::temp_dir().join(format!("airdrop-ledger-test-{}.jsonl", pubkey()));

        append_ledger_entry(
            &path,
            &LedgerEntry::BatchSubmitted {
                batch_index: 0,
                signature: "sig".to_owned(),
                expected_signature: "sig".to_owned(),
                recipient_count: 2,
                ata_creations: 1,
            },
        )
        .unwrap();
        append_ledger_entry(
            &path,
            &LedgerEntry::BatchConfirmed {
                batch_index: 0,
                signature: "sig".to_owned(),
                slot: 99,
                recipient_count: 2,
            },
        )
        .unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            serde_json::from_str::<Value>(lines[0]).unwrap()["event"],
            "batch_submitted"
        );
        assert_eq!(
            serde_json::from_str::<Value>(lines[1]).unwrap()["event"],
            "batch_confirmed"
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn sends_single_batch_and_writes_ledger() {
        let source_keypair = Keypair::new();
        let mut plan = empty_plan_for_source(source_keypair.pubkey());
        let wallet = pubkey();
        let recipient_ata = derive_associated_token_account(
            &wallet,
            &plan.distribution_token_address,
            &plan.distribution_token_program,
        );
        plan.total_amount_raw = 10;
        plan.total_amount_ui = "0.00000001".to_owned();
        plan.amount_per_recipient_raw = 10;
        plan.amount_per_recipient_ui = "0.00000001".to_owned();
        plan.recipients = vec![PlannedRecipient {
            wallet,
            recipient_ata,
            amount_raw: 10,
            amount_ui: "0.00000001".to_owned(),
            create_recipient_ata: false,
            best_rank: 1,
            holdings: vec![],
        }];
        plan.batches = vec![PlannedBatch {
            index: 0,
            recipient_count: 1,
            ata_creations: 0,
            metrics: TransactionMetrics {
                serialized_size: 0,
                account_locks: 6,
                top_level_instruction_count: 1,
                estimated_executed_instruction_count: 1,
            },
            recipients: vec![PlannedBatchRecipient {
                recipient_index: 0,
                wallet,
                recipient_ata,
                create_recipient_ata: false,
            }],
        }];

        let run_dir = std::env::temp_dir().join(format!("airdrop-send-test-{}", pubkey()));
        fs::create_dir_all(&run_dir).unwrap();
        let artifacts = PlanArtifacts {
            run_id: "test".to_owned(),
            run_dir: run_dir.clone(),
            plan_path: run_dir.join("plan.json"),
            recipients_path: run_dir.join("recipients.csv"),
            skipped_path: run_dir.join("skipped.csv"),
            ledger_path: run_dir.join("ledger.jsonl"),
            simulation_path: run_dir.join("simulation.json"),
        };
        let rpc = MockSendRpc;

        let report = send_plan(&rpc, &plan, &artifacts, &source_keypair).unwrap();

        assert_eq!(report.batch_count, 1);
        assert_eq!(report.recipient_count, 1);
        assert_eq!(report.batches[0].slot, 42);
        let contents = fs::read_to_string(&artifacts.ledger_path).unwrap();
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            serde_json::from_str::<Value>(lines[0]).unwrap()["event"],
            "batch_submitted"
        );
        assert_eq!(
            serde_json::from_str::<Value>(lines[1]).unwrap()["event"],
            "batch_confirmed"
        );
        assert_eq!(
            serde_json::from_str::<Value>(lines[2]).unwrap()["event"],
            "recipient_confirmed"
        );

        fs::remove_dir_all(run_dir).unwrap();
    }

    fn empty_plan() -> DistributionPlan {
        empty_plan_for_source(pubkey())
    }

    fn empty_plan_for_source(source_wallet: Pubkey) -> DistributionPlan {
        let distribution_token = pubkey();
        let distribution_token_program = token_2022_program_id();
        let source_ata = derive_associated_token_account(
            &source_wallet,
            &distribution_token,
            &distribution_token_program,
        );

        DistributionPlan {
            cluster_name: "mainnet-beta".to_owned(),
            source_wallet,
            source_ata,
            source_balance_raw: 0,
            source_balance_ui: "0".to_owned(),
            distribution_token_address: distribution_token,
            distribution_token_program,
            distribution_decimals: 9,
            total_amount_raw: 0,
            total_amount_ui: "0".to_owned(),
            amount_per_recipient_raw: 0,
            amount_per_recipient_ui: "0".to_owned(),
            remainder_raw: 0,
            remainder_ui: "0".to_owned(),
            recipients: vec![],
            skipped: vec![],
            batches: vec![PlannedBatch {
                index: 0,
                recipient_count: 0,
                ata_creations: 0,
                metrics: TransactionMetrics {
                    serialized_size: 0,
                    account_locks: 0,
                    top_level_instruction_count: 0,
                    estimated_executed_instruction_count: 0,
                },
                recipients: vec![],
            }],
            rent_per_ata_lamports: 0,
            estimated_ata_rent_lamports: 0,
            estimated_signature_fee_lamports: 0,
        }
    }

    struct MockSendRpc;

    impl RpcSimulator for MockSendRpc {
        fn get_latest_blockhash(&self) -> Result<Hash, RpcError> {
            Ok(Hash::default())
        }

        fn simulate_transaction(
            &self,
            _encoded_transaction: &str,
        ) -> Result<TransactionSimulation, RpcError> {
            Ok(TransactionSimulation {
                err: None,
                logs: vec![],
                units_consumed: None,
            })
        }
    }

    impl RpcSender for MockSendRpc {
        fn get_balance(&self, _address: &Pubkey) -> Result<u64, RpcError> {
            Ok(1_000_000)
        }

        fn send_transaction(&self, encoded_transaction: &str) -> Result<String, RpcError> {
            let transaction_bytes = BASE64_STANDARD.decode(encoded_transaction).unwrap();
            let transaction: Transaction = bincode::deserialize(&transaction_bytes).unwrap();
            Ok(transaction.signatures[0].to_string())
        }

        fn get_signature_status(
            &self,
            _signature: &str,
        ) -> Result<Option<SignatureStatus>, RpcError> {
            Ok(Some(SignatureStatus {
                slot: 42,
                confirmation_status: Some("confirmed".to_owned()),
                err: None,
            }))
        }
    }

    fn pubkey() -> Pubkey {
        Keypair::new().pubkey()
    }
}
