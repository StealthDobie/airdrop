use {
    crate::{
        discovery::{discover_holders, discover_holders_with_solscan},
        env::OsEnv,
        execution::{
            check_source_sol_funding, confirmation_matches, confirmation_phrase,
            required_sol_lamports, send_plan,
        },
        planning::{create_distribution_plan, write_plan_artifacts},
        rpc::HttpRpcClient,
        runtime::RuntimeConfig,
        simulation::{SimulationReport, simulate_plan, write_simulation_artifact},
        solscan::SolscanClient,
    },
    anyhow::Context,
    clap::{Parser, Subcommand},
    std::{
        io,
        path::{Path, PathBuf},
    },
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
    /// Plan, simulate, confirm, and submit the configured airdrop.
    Send {
        #[arg(short, long, default_value = "config.toml")]
        config: PathBuf,
    },
}

pub fn run() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    match cli.command {
        Command::Validate { config } => validate(config),
        Command::Run { config } => run_dry(config),
        Command::Send { config } => send(config),
    }
}

fn run_dry(config: PathBuf) -> anyhow::Result<()> {
    let prepared = prepare_run(config)?;
    ensure_simulation_success(&prepared)?;
    print_plan_summary(
        &prepared,
        "Plan and simulation OK (dry run; no transactions sent)",
    );
    Ok(())
}

fn send(config: PathBuf) -> anyhow::Result<()> {
    let prepared = prepare_run(config)?;
    ensure_simulation_success(&prepared)?;
    print_plan_summary(
        &prepared,
        "Plan and simulation OK (ready to send; no transactions sent yet)",
    );

    let funding = check_source_sol_funding(&prepared.rpc, &prepared.plan)?;
    println!(
        "Source SOL balance: {} lamports ({} SOL)",
        funding.available_lamports,
        format_lamports_as_sol(funding.available_lamports)
    );
    println!(
        "Estimated SOL needed for fees + ATA deposits: {} lamports ({} SOL)",
        funding.required_lamports,
        format_lamports_as_sol(funding.required_lamports)
    );
    if !funding.is_funded() {
        anyhow::bail!(
            "source wallet needs {} more lamports ({} SOL) before sending",
            funding.shortfall_lamports,
            format_lamports_as_sol(funding.shortfall_lamports)
        );
    }

    let phrase = confirmation_phrase(&prepared.plan);
    println!(
        "This will submit {} mainnet-beta transaction(s) to distribute {} to {} recipient(s).",
        prepared.plan.batches.len(),
        prepared.plan.total_amount_ui,
        prepared.plan.recipients.len()
    );
    println!("Type `{phrase}` to send:");
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    if !confirmation_matches(&input, &prepared.plan) {
        anyhow::bail!("confirmation phrase did not match; no transactions sent");
    }

    let report = send_plan(
        &prepared.rpc,
        &prepared.plan,
        &prepared.artifacts,
        prepared.runtime.source_wallet.keypair(),
    )?;

    println!(
        "Send complete: {} transaction(s), {} recipient(s)",
        report.batch_count, report.recipient_count
    );
    for batch in report.batches {
        println!(
            "  batch {} signature {} ({} recipient(s), slot {})",
            batch.batch_index + 1,
            batch.signature,
            batch.recipient_count,
            batch.slot
        );
    }
    println!("Ledger: {}", prepared.artifacts.ledger_path.display());
    Ok(())
}

fn prepare_run(config: PathBuf) -> anyhow::Result<PreparedRun> {
    let runtime = load_runtime(config)?;
    eprintln!(
        "Loaded config: cluster={}, targets={}, max_recipients={}, solscan_holder_fetch_limit={}",
        runtime.config.cluster_name,
        runtime.config.target_token_addresses.len(),
        runtime.config.max_recipients,
        runtime.config.solscan.holder_fetch_limit
    );
    let rpc = HttpRpcClient::new(runtime.rpc_url.clone());
    let report = if let Some(solscan_api_key) = runtime.solscan_api_key.as_deref() {
        eprintln!("Starting holder discovery with Solscan");
        let solscan = SolscanClient::new(solscan_api_key);
        discover_holders_with_solscan(
            &rpc,
            &solscan,
            &runtime.config,
            &runtime.source_wallet.public_key(),
        )?
    } else {
        eprintln!("Starting holder discovery with RPC");
        discover_holders(&rpc, &runtime.config, &runtime.source_wallet.public_key())?
    };
    eprintln!(
        "Discovery complete: recipients={}, skipped={}",
        report.recipients.len(),
        report.skipped.len()
    );
    eprintln!("Building dry-run distribution plan");
    let plan = create_distribution_plan(
        &rpc,
        &runtime.config,
        report,
        &runtime.source_wallet.public_key(),
    )?;
    eprintln!("Writing plan artifacts");
    let artifacts = write_plan_artifacts(&plan, Path::new("runs"))?;
    eprintln!("Simulating planned transactions");
    let simulation_report = simulate_plan(&rpc, &plan)?;
    let simulation_artifact_path = write_simulation_artifact(&simulation_report, &artifacts)?;
    eprintln!(
        "Simulation complete: succeeded={}, failed={}",
        simulation_report.succeeded_batch_count(),
        simulation_report.failed_batch_count()
    );

    Ok(PreparedRun {
        runtime,
        rpc,
        plan,
        artifacts,
        simulation_report,
        simulation_artifact_path,
    })
}

fn ensure_simulation_success(prepared: &PreparedRun) -> anyhow::Result<()> {
    if prepared.simulation_report.failed_batch_count() > 0 {
        anyhow::bail!(
            "simulation failed for {} planned transaction(s); see {}",
            prepared.simulation_report.failed_batch_count(),
            prepared.simulation_artifact_path.display()
        );
    }

    Ok(())
}

fn print_plan_summary(prepared: &PreparedRun, heading: &str) {
    let plan = &prepared.plan;
    println!("{heading}");
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
    println!(
        "Simulated transactions: {}",
        prepared.simulation_report.succeeded_batch_count()
    );
    println!("Recipient ATAs to create: {}", plan.ata_creations());
    println!(
        "Estimated signature fees: {} lamports ({} SOL)",
        plan.estimated_signature_fee_lamports,
        format_lamports_as_sol(plan.estimated_signature_fee_lamports)
    );
    println!(
        "Estimated ATA rent deposits: {} lamports ({} SOL)",
        plan.estimated_ata_rent_lamports,
        format_lamports_as_sol(plan.estimated_ata_rent_lamports)
    );
    println!(
        "Rent per recipient ATA: {} lamports ({} SOL)",
        plan.rent_per_ata_lamports,
        format_lamports_as_sol(plan.rent_per_ata_lamports)
    );
    println!(
        "Estimated total SOL needed: {} lamports ({} SOL)",
        required_sol_lamports(plan),
        format_lamports_as_sol(required_sol_lamports(plan))
    );
    println!("Skipped candidates: {}", plan.skipped.len());
    for (reason, count) in plan.skipped_counts_by_reason() {
        println!("  {reason}: {count}");
    }
    println!("Plan artifacts: {}", prepared.artifacts.run_dir.display());
    println!(
        "Simulation: {}",
        prepared.simulation_artifact_path.display()
    );
    println!("Ledger: {}", prepared.artifacts.ledger_path.display());
}

fn validate(config_path: PathBuf) -> anyhow::Result<()> {
    let runtime = load_runtime(config_path)?;

    println!("Config OK");
    println!("Cluster: {}", runtime.config.cluster_name);
    println!("RPC env: {}", runtime.config.rpc_url_env);
    println!("Source wallet: {}", runtime.source_wallet.public_key());
    println!(
        "Holder discovery: {}",
        if runtime.solscan_api_key.is_some() {
            "solscan"
        } else {
            "rpc"
        }
    );
    println!(
        "Solscan holder fetch limit: {}",
        runtime.config.solscan.holder_fetch_limit
    );
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

struct PreparedRun {
    runtime: RuntimeConfig,
    rpc: HttpRpcClient,
    plan: crate::planning::DistributionPlan,
    artifacts: crate::planning::PlanArtifacts,
    simulation_report: SimulationReport,
    simulation_artifact_path: PathBuf,
}

fn format_lamports_as_sol(lamports: u64) -> String {
    const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
    let whole = lamports / LAMPORTS_PER_SOL;
    let fractional = lamports % LAMPORTS_PER_SOL;

    if fractional == 0 {
        return whole.to_string();
    }

    let fractional = format!("{fractional:09}").trim_end_matches('0').to_owned();
    format!("{whole}.{fractional}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_lamports_as_sol_without_rounding() {
        assert_eq!(format_lamports_as_sol(0), "0");
        assert_eq!(format_lamports_as_sol(5_000), "0.000005");
        assert_eq!(format_lamports_as_sol(185_000), "0.000185");
        assert_eq!(format_lamports_as_sol(701_512_320), "0.70151232");
        assert_eq!(format_lamports_as_sol(1_000_000_000), "1");
        assert_eq!(format_lamports_as_sol(1_234_567_890), "1.23456789");
    }
}
