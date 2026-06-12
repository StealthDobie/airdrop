# airdrop

Interactive Rust CLI for planning and executing targeted Solana Token-2022 distributions.

Current status: implementation in progress.

- [Airdrop CLI specification](docs/airdrop-cli-spec.md)
- [Example environment file](.env.example)
- [Example config](config.example.toml)

## Local dry run

```bash
cp .env.example .env
cp config.example.toml config.toml
```

Fill `.env` with `SOLANA_RPC_URL` and exactly one source wallet key:

- `SOURCE_PRIVATE_KEY_BASE58`
- `SOURCE_PRIVATE_KEY_BASE64`

Then populate `config.toml` with real target token addresses and run:

```bash
cargo run -- validate
cargo run -- run
```

`run` is no-send. It reads mainnet-beta state, requires the distribution token to be Token-2022, supports legacy SPL or Token-2022 target mints for read-only holder discovery, checks the source distribution-token ATA and balance, calculates the per-recipient amount, plans legacy transaction batches, simulates every planned transaction, and writes artifacts under `runs/<run-id>/`. The simulation path uses unsigned transactions with RPC signature verification disabled.

To submit the airdrop, run:

```bash
cargo run -- send
```

If a complete dry-run cache exists under `runs/<run-id>/` with an empty `ledger.jsonl`, `send` asks whether to use the newest cached plan. Type `Y` to use it without repeating holder discovery and planning, or `N` to run the full scan again. Cached plans must match the current config, and `send` re-simulates the cached transactions before the final confirmation. It refuses to submit if the source wallet SOL balance is below the estimated signature fees plus recipient ATA rent deposits. If funding is sufficient, it prints the final mainnet-beta summary and requires the exact confirmation phrase before signing and submitting transactions. Prepared, submitted, confirmed, and failed batches are appended to `runs/<run-id>/ledger.jsonl`.

If a send is interrupted or stops after some batches land, resume the saved plan instead of starting over:

```bash
cargo run -- send --resume runs/<run-id>
```

Resume mode loads the original `plan.json`, reconciles any prepared or submitted batch signatures from `ledger.jsonl`, skips confirmed batches, and refuses to rebroadcast an in-flight signature that has not reached a terminal status yet.

Run artifacts include:

- `plan.json`
- `recipients.csv`
- `skipped.csv`
- `simulation.json`
- `ledger.jsonl`

Progress is logged to stderr during network-heavy phases such as Solscan pagination, mint validation, recipient verification, ATA planning, and transaction simulation.

Holder discovery uses RPC by default. If public RPC rate limits `getTokenLargestAccounts`, enable Solscan in `config.toml` and provide `SOLSCAN_API_KEY` in `.env`:

```toml
[providers.solscan]
enabled = true
api_key_env = "SOLSCAN_API_KEY"
holder_fetch_limit = 100
```

## Development

```bash
cargo fmt -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```
