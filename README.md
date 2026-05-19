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

`run` is still dry-run only. It reads mainnet-beta state, requires configured distribution and target tokens to be Token-2022 mints, discovers recipients, checks the source distribution-token ATA and balance, calculates the per-recipient amount, plans legacy transaction batches, and writes artifacts under `runs/<run-id>/`. It does not sign or send transactions.

## Development

```bash
cargo fmt -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```
