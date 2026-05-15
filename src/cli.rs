use {
    crate::{discovery::discover_holders, env::OsEnv, rpc::HttpRpcClient, runtime::RuntimeConfig},
    anyhow::Context,
    clap::{Parser, Subcommand},
    std::path::PathBuf,
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
            println!("Discovery OK");
            println!(
                "Distribution token: {}",
                report.distribution_token.token_address
            );
            println!("Target tokens: {}", report.target_tokens.len());
            println!("Recipients discovered: {}", report.recipients.len());
            println!("Skipped candidates: {}", report.skipped.len());
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
