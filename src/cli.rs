use {
    crate::{
        discovery::{discover_holders, discover_holders_with_solscan},
        env::OsEnv,
        execution::{
            check_source_sol_funding, confirmation_matches, confirmation_phrase,
            required_sol_lamports, send_plan,
        },
        planning::{
            create_distribution_plan, read_plan_artifacts, validate_cached_plan_config,
            write_plan_artifacts,
        },
        rpc::HttpRpcClient,
        runtime::RuntimeConfig,
        simulation::{SimulationReport, simulate_plan, write_simulation_artifact},
        solscan::SolscanClient,
    },
    anyhow::Context,
    clap::{Parser, Subcommand},
    std::{
        fs,
        fs::File,
        io::{self, BufRead, Write},
        path::{Path, PathBuf},
    },
};

const RUNS_DIR: &str = "runs";

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
        /// Resume a previously planned run directory without rebuilding the recipient list.
        #[arg(long)]
        resume: Option<PathBuf>,
    },
}

pub fn run() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    match cli.command {
        Command::Validate { config } => validate(config),
        Command::Run { config } => run_dry(config),
        Command::Send { config, resume } => send(config, resume),
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

fn send(config: PathBuf, resume: Option<PathBuf>) -> anyhow::Result<()> {
    let prepared = if let Some(run_dir) = resume {
        prepare_resume(config, run_dir)?
    } else if let Some(run_dir) = latest_cached_run_dir(Path::new(RUNS_DIR))? {
        println!("Cached run found: {}", run_dir.display());
        println!("Type `Y` to use this cached run, or `N` to run the full scan:");
        io::stdout().flush()?;
        let input = read_confirmation_line()?;
        match parse_cache_choice(&input) {
            Some(true) => prepare_cached_run(config, run_dir)?,
            Some(false) => prepare_run_for_send(config)?,
            None => anyhow::bail!("expected `Y` or `N`; no transactions sent"),
        }
    } else {
        eprintln!("No cached run found; running full scan");
        prepare_run_for_send(config)?
    };
    print_plan_summary(&prepared, send_summary_heading(&prepared));

    let funding = check_source_sol_funding(&prepared.rpc, &prepared.plan, &prepared.artifacts)?;
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
    io::stdout().flush()?;
    let input = read_confirmation_line()?;
    if !confirmation_matches(&input, &prepared.plan) {
        anyhow::bail!(
            "confirmation phrase did not match; expected `{phrase}`, got `{}`; no transactions sent",
            input.trim().escape_debug()
        );
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

fn read_confirmation_line() -> io::Result<String> {
    let mut input = String::new();

    match File::open("/dev/tty") {
        Ok(file) => {
            let mut reader = io::BufReader::new(file);
            reader.read_line(&mut input)?;
        }
        Err(_) => {
            io::stdin().read_line(&mut input)?;
        }
    }

    Ok(input)
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
    let artifacts = write_plan_artifacts(&plan, &runtime.config, Path::new(RUNS_DIR))?;
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
        simulation_report: Some(simulation_report),
        simulation_artifact_path,
    })
}

fn prepare_run_for_send(config: PathBuf) -> anyhow::Result<PreparedRun> {
    let prepared = prepare_run(config)?;
    ensure_simulation_success(&prepared)?;
    Ok(prepared)
}

fn prepare_cached_run(config: PathBuf, run_dir: PathBuf) -> anyhow::Result<PreparedRun> {
    let mut prepared = prepare_resume(config, run_dir)?;
    validate_cached_plan_config(&prepared.artifacts.run_dir, &prepared.runtime.config)
        .with_context(|| {
            format!(
                "cached run {} does not match current config",
                prepared.artifacts.run_dir.display()
            )
        })?;

    eprintln!(
        "Re-simulating cached run {} before send",
        prepared.artifacts.run_dir.display()
    );
    let simulation_report = simulate_plan(&prepared.rpc, &prepared.plan)?;
    let simulation_artifact_path =
        write_simulation_artifact(&simulation_report, &prepared.artifacts).with_context(|| {
            format!(
                "failed to refresh cached simulation {}",
                prepared.artifacts.simulation_path.display()
            )
        })?;
    prepared.simulation_artifact_path = simulation_artifact_path;
    prepared.simulation_report = Some(simulation_report);
    ensure_simulation_success(&prepared)?;
    Ok(prepared)
}

fn prepare_resume(config: PathBuf, run_dir: PathBuf) -> anyhow::Result<PreparedRun> {
    let runtime = load_runtime(config)?;
    let (plan, artifacts) = read_plan_artifacts(&run_dir)
        .with_context(|| format!("failed to resume run {}", run_dir.display()))?;

    if plan.cluster_name != runtime.config.cluster_name {
        anyhow::bail!(
            "resume run cluster `{}` does not match config cluster `{}`",
            plan.cluster_name,
            runtime.config.cluster_name
        );
    }
    if plan.source_wallet != runtime.source_wallet.public_key() {
        anyhow::bail!(
            "resume run source wallet `{}` does not match configured source wallet `{}`",
            plan.source_wallet,
            runtime.source_wallet.public_key()
        );
    }
    if plan.distribution_token_address != runtime.config.distribution_token_address {
        anyhow::bail!(
            "resume run distribution token `{}` does not match config distribution token `{}`",
            plan.distribution_token_address,
            runtime.config.distribution_token_address
        );
    }

    eprintln!("Resuming saved run {}", artifacts.run_dir.display());
    let rpc_url = runtime.rpc_url.clone();
    let simulation_artifact_path = artifacts.simulation_path.clone();
    Ok(PreparedRun {
        runtime,
        rpc: HttpRpcClient::new(rpc_url),
        plan,
        artifacts,
        simulation_report: None,
        simulation_artifact_path,
    })
}

fn latest_cached_run_dir(runs_dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let entries = match fs::read_dir(runs_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(source)
                .with_context(|| format!("failed to read runs directory {}", runs_dir.display()));
        }
    };
    let mut candidates = Vec::new();

    for entry in entries {
        let entry = entry
            .with_context(|| format!("failed to read runs directory {}", runs_dir.display()))?;
        let file_type = entry.file_type().with_context(|| {
            format!(
                "failed to inspect run directory entry {}",
                entry.path().display()
            )
        })?;
        if !file_type.is_dir() {
            continue;
        }

        let run_dir = entry.path();
        if cached_run_artifacts_exist(&run_dir) {
            candidates.push(run_dir);
        }
    }

    candidates.sort_by_key(|run_dir| run_dir_name(run_dir));
    Ok(candidates.pop())
}

fn cached_run_artifacts_exist(run_dir: &Path) -> bool {
    run_dir.join("plan.json").is_file()
        && run_dir.join("simulation.json").is_file()
        && run_dir.join("ledger.jsonl").is_file()
        && cached_run_ledger_is_empty(run_dir)
}

fn cached_run_ledger_is_empty(run_dir: &Path) -> bool {
    fs::read_to_string(run_dir.join("ledger.jsonl")).is_ok_and(|ledger| ledger.trim().is_empty())
}

fn run_dir_name(run_dir: &Path) -> String {
    run_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned()
}

fn parse_cache_choice(input: &str) -> Option<bool> {
    match input.trim() {
        "Y" | "y" => Some(true),
        "N" | "n" => Some(false),
        _ => None,
    }
}

fn ensure_simulation_success(prepared: &PreparedRun) -> anyhow::Result<()> {
    let Some(simulation_report) = &prepared.simulation_report else {
        return Ok(());
    };

    if simulation_report.failed_batch_count() > 0 {
        anyhow::bail!(
            "simulation failed for {} planned transaction(s); see {}",
            simulation_report.failed_batch_count(),
            prepared.simulation_artifact_path.display()
        );
    }

    Ok(())
}

fn send_summary_heading(prepared: &PreparedRun) -> &'static str {
    if prepared.simulation_report.is_some() {
        "Plan and simulation OK (ready to send; no transactions sent yet)"
    } else {
        "Loaded saved run (ready to resume; no transactions sent yet)"
    }
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
    if let Some(simulation_report) = &prepared.simulation_report {
        println!(
            "Simulated transactions: {}",
            simulation_report.succeeded_batch_count()
        );
    } else {
        println!("Simulated transactions: loaded from saved run");
    }
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
    simulation_report: Option<SimulationReport>,
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
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn formats_lamports_as_sol_without_rounding() {
        assert_eq!(format_lamports_as_sol(0), "0");
        assert_eq!(format_lamports_as_sol(5_000), "0.000005");
        assert_eq!(format_lamports_as_sol(185_000), "0.000185");
        assert_eq!(format_lamports_as_sol(701_512_320), "0.70151232");
        assert_eq!(format_lamports_as_sol(1_000_000_000), "1");
        assert_eq!(format_lamports_as_sol(1_234_567_890), "1.23456789");
    }

    #[test]
    fn parses_cache_choice() {
        assert_eq!(parse_cache_choice("Y\n"), Some(true));
        assert_eq!(parse_cache_choice("y"), Some(true));
        assert_eq!(parse_cache_choice("N\n"), Some(false));
        assert_eq!(parse_cache_choice("n"), Some(false));
        assert_eq!(parse_cache_choice("yes"), None);
        assert_eq!(parse_cache_choice(""), None);
    }

    #[test]
    fn finds_latest_complete_cached_run() {
        let runs_dir = temp_runs_dir();
        let old_run = runs_dir.join("100");
        let new_run = runs_dir.join("300");
        let incomplete_newer_run = runs_dir.join("400");
        let sent_newer_run = runs_dir.join("500");

        write_cached_run_files(&old_run);
        write_cached_run_files(&new_run);
        fs::create_dir_all(&incomplete_newer_run).unwrap();
        fs::write(incomplete_newer_run.join("plan.json"), "{}").unwrap();
        write_cached_run_files(&sent_newer_run);
        fs::write(
            sent_newer_run.join("ledger.jsonl"),
            r#"{"event":"batch_confirmed"}"#,
        )
        .unwrap();

        let latest = latest_cached_run_dir(&runs_dir).unwrap();

        assert_eq!(latest, Some(new_run));
        fs::remove_dir_all(runs_dir).unwrap();
    }

    #[test]
    fn missing_runs_dir_has_no_cached_run() {
        let runs_dir = temp_runs_dir();

        let latest = latest_cached_run_dir(&runs_dir).unwrap();

        assert_eq!(latest, None);
    }

    fn write_cached_run_files(run_dir: &Path) {
        fs::create_dir_all(run_dir).unwrap();
        fs::write(run_dir.join("plan.json"), "{}").unwrap();
        fs::write(run_dir.join("simulation.json"), "{}").unwrap();
        fs::write(run_dir.join("ledger.jsonl"), "").unwrap();
    }

    fn temp_runs_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("airdrop-cli-cache-test-{nanos}"))
    }
}
