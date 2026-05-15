use {
    crate::{
        discovery::discover_holders,
        env::OsEnv,
        planning::{create_distribution_plan, write_plan_artifacts},
        rpc::HttpRpcClient,
        runtime::RuntimeConfig,
    },
    anyhow::Context,
    clap::{Parser, Subcommand},
    std::path::{Path, PathBuf},
};

#[derive(Debug, Parser)]
#[command(author, version, about)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate local config and source-wallet environment.
    Validate {
        #[arg(short, long, default_value = "config.toml")]
        config: PathBuf,
    },
    /// Load the v0 run inputs. Later slices add RPC discovery and sending.
    Run {
        #[arg(short, long, default_value = "config.toml")]
        config: PathBuf,
    },
}

pub fn run() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    match cli.command {
        Command::Validate { config } => validate(config),
        Command::Run { config } => {
            let runtime = load_runtime(config)?;
            let rpc = HttpRpcClient::new(runtime.rpc_url.clone());
            let report =
                discover_holders(&rpc, &runtime.config, &runtime.source_wallet.public_key())?;
            let plan = create_distribution_plan(
                &rpc,
                &runtime.config,
                report,
                &runtime.source_wallet.public_key(),
            )?;
            let artifacts = write_plan_artifacts(&plan, Path::new("runs"))?;

            println!("Plan OK (dry run; no transactions sent)");
            println!("Cluster: {}", plan.cluster_name);
            println!("Source wallet: {}", plan.source_wallet);
            println!("Source ATA: {}", plan.source_ata);
            println!("Source balance: {}", plan.source_balance_ui);
            println!("Distribution token: {}", plan.distribution_token_address);
            println!(
                "Total distribution: {} (raw {})",
                plan.total_amount_ui, plan.total_amount_raw
            );
            println!("Recipients: {}", plan.recipients.len());
            println!(
                "Amount per recipient: {} (raw {})",
                plan.amount_per_recipient_ui, plan.amount_per_recipient_raw
            );
            println!(
                "Remainder left in source wallet: {} (raw {})",
                plan.remainder_ui, plan.remainder_raw
            );
            println!("Planned transactions: {}", plan.batches.len());
            println!("Recipient ATAs to create: {}", plan.ata_creations());
            println!(
                "Estimated signature fees: {} lamports",
                plan.estimated_signature_fee_lamports
            );
            println!(
                "Estimated ATA rent exposure: {} lamports",
                plan.estimated_ata_rent_lamports
            );
            println!("Skipped candidates: {}", plan.skipped.len());
            for (reason, count) in plan.skipped_counts_by_reason() {
                println!("  {reason}: {count}");
            }
            println!("Plan artifacts: {}", artifacts.run_dir.display());
            println!("Ledger: {}", artifacts.ledger_path.display());
            Ok(())
        }
    }
}

fn validate(config_path: PathBuf) -> anyhow::Result<()> {
    let runtime = load_runtime(config_path)?;

    println!("Config OK");
    println!("Cluster: {}", runtime.config.cluster_name);
    println!("RPC env: {}", runtime.config.rpc_url_env);
    println!("Source wallet: {}", runtime.source_wallet.public_key());
    println!(
        "Distribution token: {}",
        runtime.config.distribution_token_address
    );
    println!(
        "Target tokens: {}",
        runtime.config.target_token_addresses.len()
    );
    println!("Max recipients: {}", runtime.config.max_recipients);

    Ok(())
}

fn load_runtime(config_path: PathBuf) -> anyhow::Result<RuntimeConfig> {
    let env = OsEnv;
    RuntimeConfig::from_path_and_env(&config_path, &env)
        .with_context(|| format!("failed to load {}", config_path.display()))
}
