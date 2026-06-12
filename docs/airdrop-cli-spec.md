# Airdrop CLI Specification

Status: draft v0.9
Date: 2026-05-19
Repository: `StealthDobie/airdrop`

## Purpose

Build an interactive Rust CLI that distributes a configured Token-2022 token from one funded source wallet to targeted wallets on Solana mainnet-beta.

Target wallets are discovered from holders of one or more configured target token addresses. The distribution token is the token being sent. The target token addresses are the tokens whose holder lists are used to find recipients.

The tool is an operator-run distribution workflow. It should preview the plan, require explicit confirmation, send transactions, support resume, and produce an audit trail.

## Terminology

- Token address: the public address that identifies a Solana token.
- Mint: Solana's technical term for a token address. In code and RPC APIs this is usually called a `mint`.
- Distribution token: the Token-2022 token the CLI sends from the source wallet.
- Target token: a token whose holders should be considered as possible recipients; target tokens may be legacy SPL or Token-2022 because they are read-only discovery inputs.
- ATA: associated token account, the standard token account for one wallet and one token address.

## V0 Decisions

- Use Rust and direct Solana RPC/SPL libraries, not shell calls to `solana` or `spl-token`.
- Use Solana RPC for default v0 holder discovery.
- Support Solscan as an optional holder-discovery provider when an API key is configured.
- Use `confirmed` commitment for all reads, simulations, sends, and confirmations.
- Use `TransferChecked` for token transfers.
- Support only Token-2022 for the distribution token in v0. Target tokens may be legacy SPL or Token-2022 because they are only used for read-only holder discovery. If the distribution token is not Token-2022, or a target token is not owned by a supported token program, fail early with a clear unsupported-token-program message.
- Automatically create missing recipient ATAs when needed and include the estimated SOL rent exposure in the confirmation summary.
- Automatically pack as many recipients as safely fit in each transaction.
- Do not add Phantom-specific send delays. Phantom does not publish a safe cadence, and batching changes the shape of the traffic anyway.
- Do not expose compute budget, priority fee, commitment, batch size, retry-delay, or ledger-path knobs in v0.
- Do not support minting, burning, authority changes, freeze/thaw, or liquidity-pool interaction.

## Transaction Limits

There is a real limit to how many recipients can fit in one Solana transaction.

Relevant Solana limits:

- Maximum serialized transaction size: 1,232 bytes.
- Maximum accounts per transaction: 64 account locks.
- Maximum executed instructions: 64 across top-level instructions and CPIs.

For an SPL token transfer to many existing recipient ATAs, serialized size is usually the first practical limit. With one signer, one source token account, one distribution token address, and one destination ATA per recipient, a legacy transaction is expected to fit roughly 18-20 `TransferChecked` recipients.

When some recipients need ATA creation, each recipient needs more accounts and instructions, so the expected batch size is lower, often around 8-10 recipients. The implementation should not hard-code those estimates. It should build candidate transactions, measure serialized size, simulate them, and split batches before any limit is hit.

V0 should use legacy transactions first. Versioned transactions and address lookup tables can be considered later if we need larger batches, but they add setup and operational complexity.

## Security Rules

- `.env` is local-only and ignored by git.
- The source wallet must be a purpose-funded hot wallet, not a treasury wallet.
- The CLI must never print private key material, API keys, RPC auth tokens, or full `.env` contents.
- Mainnet-beta sends require an explicit confirmation prompt that includes cluster, source wallet, distribution token address, total amount, recipient count, amount per recipient, transaction count, and estimated SOL fees/rent.
- Every run begins as a plan/dry-run. No transaction is sent until the operator confirms the final plan.
- Failed, skipped, submitted, and completed recipients must be written to a resumable run ledger.

## Configuration

The CLI reads `config.toml` from the repo root by default, with `--config <path>` override.

Draft v0 shape:

```toml
[cluster]
name = "mainnet-beta"
rpc_url_env = "SOLANA_RPC_URL"

[distribution]
token_address = "J3mfHoQb27xHL1xUYsoPfU1vZHbzCeK7fZYvsWeYdoge"
total_amount_ui = "1000000"

[targeting]
target_token_addresses = [
  "TARGET_TOKEN_ADDRESS_1",
  "TARGET_TOKEN_ADDRESS_2"
]
max_recipients = 100
manual_exclude_wallets = []

[providers.solscan]
enabled = false
api_key_env = "SOLSCAN_API_KEY"
holder_fetch_limit = 100
```

Settings intentionally not configurable in v0:

- Commitment: always `confirmed`.
- Batch size: automatically packed to transaction limits.
- Transaction delay: no Phantom-specific delay.
- Compute budget and priority fees: Solana defaults.
- Source wallet env names: fixed as `SOURCE_PRIVATE_KEY_BASE58` and `SOURCE_PRIVATE_KEY_BASE64`.
- Fee payer: always the source wallet.
- Source token account: derived as the source wallet ATA for the distribution token.
- Distribution token decimals: read from chain.
- Missing recipient ATAs: created automatically.
- Existing distribution-token holders: always excluded.
- Liquidity pools, executable accounts, program-owned accounts, and off-curve owners: always excluded when detectable.
- Run ledger directory: always `runs/`.
- Recipient cap: `max_recipients` applies globally after merging all target-token holder lists.

## Environment Variables

The root `.env` should support:

- `SOLANA_RPC_URL`: mainnet-beta RPC endpoint.
- `SOURCE_PRIVATE_KEY_BASE58`: source wallet private key encoded as base58.
- `SOURCE_PRIVATE_KEY_BASE64`: source wallet private key encoded as base64.
- `SOLSCAN_API_KEY`: optional Solscan Pro API key for future indexed discovery.

Exactly one of `SOURCE_PRIVATE_KEY_BASE58` or `SOURCE_PRIVATE_KEY_BASE64` must be set. V0 should not support Solana CLI keypair paths or JSON keypair arrays.

The decoded source private key should be the full Solana keypair secret bytes expected by the Rust SDK. If the decoded value is not a valid source keypair, the CLI must fail before any RPC or send work begins.

## Token Program Policy

Solana has two major token programs:

- Legacy SPL Token Program: the older token program for basic mint, transfer, and token account behavior.
- Token Extension Program, often called Token-2022: the token program used by `$dobermann`; it includes the original token features and adds optional extensions such as metadata on the mint, transfer fees, non-transferability, confidential-transfer behavior, and other account or mint-level features.

Token-2022 matters because the token program ID, ATA derivation, account layout, and transfer behavior can differ from the original token program. Some extensions can change or block transfers entirely.

V0 should keep this simple: detect the owner program for each configured token address. The distribution token must be Token-2022 because it is the token being sent. Target tokens may be legacy SPL or Token-2022 because they are read-only discovery inputs. Associated token account derivation and transfer/simulation instructions for the distribution token must use the Token-2022 program id.

## Target Discovery

For each configured `target_token_addresses` entry:

1. Treat the configured token address as a Solana mint address.
2. Call RPC `getTokenLargestAccounts` for that target token address.
3. Resolve each returned token account to its owner wallet.
4. Verify token account owner, token address, amount, and executable/program-owned status through RPC.
5. Discard zero-balance rows and malformed account data.
6. Merge candidates across target token addresses.

RPC v0 discovery limit:

- RPC returns up to 20 token accounts per target token address.
- If exclusions remove candidates, v0 does not backfill past those 20.
- Larger campaigns should use the optional Solscan provider or another indexed provider later.

Solscan discovery:

- When `[providers.solscan].enabled = true`, load the API key from `SOLSCAN_API_KEY` by default.
- Fetch holder pages from Solscan's `token/holders` endpoint up to `providers.solscan.holder_fetch_limit` ranked holders per target token. The default is `100`.
- Treat Solscan as a candidate source only. The CLI still reads mint metadata, owner-account exclusions, source ATA state, and recipient ATA state through Solana RPC; when Solscan provides holder owners, the CLI can avoid per-candidate token-account lookups.

Default recipient selection:

- Include wallets that hold at least one configured target token.
- Deduplicate by owner wallet.
- Rank by best per-token holder rank, then aggregate normalized target-token balance as a tie-breaker.
- Stop at the global `max_recipients` after exclusions.

## Exclusion Rules

The first version always excludes:

- The source wallet.
- Wallets in `manual_exclude_wallets`.
- Existing holders of the distribution token.
- Liquidity pool vaults and pool authorities when detectable.
- Executable accounts, program-owned accounts, and off-curve owners.
- Token accounts whose owner cannot be resolved to a wallet-like owner.

Detection should be conservative. If a candidate cannot be safely classified, skip it and record the reason in the plan.

## Distribution Plan

Before sending, the CLI writes a plan and prints a confirmation summary:

- Cluster and redacted RPC endpoint label.
- Source wallet public key.
- Distribution token address and token program.
- Source ATA and current token balance.
- Total distribution amount.
- Recipient count.
- Amount per recipient.
- Remainder amount, if total cannot divide evenly.
- Estimated transaction count after auto-packing.
- Estimated SOL fee and ATA rent exposure.
- Skipped candidate counts by reason.
- Output path for plan artifacts.

Suggested artifacts:

- `runs/<timestamp>/plan.json`
- `runs/<timestamp>/recipients.csv`
- `runs/<timestamp>/skipped.csv`
- `runs/<timestamp>/ledger.jsonl`

## Interactive Flow

Dry-run command:

```bash
airdrop run --config config.toml
```

Send command:

```bash
airdrop send --config config.toml
```

If `runs/<run-id>/plan.json`, `simulation.json`, and an empty `ledger.jsonl` exist from a previous dry run, `send` should offer the newest complete cached run before repeating discovery. The operator must type `Y` to use the cached run or `N` to run the full scan. Cached plans must include a config snapshot that matches the current config, and accepted cached plans must be re-simulated before the final send confirmation. Runs with send progress in `ledger.jsonl` should use explicit `--resume`.

Resume command:

```bash
airdrop send --config config.toml --resume runs/<run-id>
```

Flow:

1. Load `.env` and `config.toml`.
2. Validate cluster is mainnet-beta.
3. Load the source wallet and print only its public key.
4. Read distribution token metadata, decimals, token program, source ATA, and source balance.
5. Discover, verify, merge, rank, and exclude target holders.
6. Calculate per-recipient amount.
7. Build recipient batches by transaction size/account/instruction limits.
8. Simulate every planned batch, including batches with ATA creation if applicable. The dry-run command uses unsigned transactions with `sigVerify=false`; the send command signs fresh transactions before submission and uses RPC preflight.
9. Save plan artifacts.
10. Print confirmation summary.
11. Require the operator to type a confirmation phrase, for example `SEND <recipient_count>`.
12. Send each packed transaction batch.
13. Persist every transaction and recipient result immediately.
14. Print final summary with signatures and failures.

Resume flow:

- Load the saved `plan.json` and `ledger.jsonl` from the run directory.
- Verify the saved plan cluster, source wallet, and distribution token match the current config and source key.
- Reconcile prepared or submitted batch signatures through RPC before sending anything new.
- Skip confirmed batches and preserve the original per-recipient amount for remaining batches.
- Refuse to rebroadcast a prepared or submitted signature that has not reached a terminal status.

## Transaction Execution

For each packed batch:

1. Derive each recipient ATA for the distribution token address and token program.
2. Add idempotent ATA creation instructions for missing ATAs.
3. Add one `TransferChecked` instruction per recipient.
4. Ensure the serialized transaction is below 1,232 bytes.
5. Ensure account and instruction limits are respected.
6. Simulate with `confirmed` commitment.
7. Write a durable prepared-batch ledger entry containing the locally signed transaction signature.
8. Submit signed transaction with `confirmed` preflight.
9. Reject RPC responses whose returned signature differs from the locally signed transaction signature.
10. Confirm with `confirmed` commitment. Treat explicit `confirmed`/`finalized` success, or a successful status object with nullable confirmation status, as terminal success.
11. Record signature, slot, per-recipient status, amount, recipient, and ATA creation flag.

Transactions are atomic. If one recipient instruction in a batch fails, the whole transaction fails. V0 resume may retry terminally failed batches with the original packing after the operator fixes the underlying issue. Smaller retry batches and single-recipient fallback can be added later.

## Resume and Audit

The runner must be resumable.

Each recipient should have one of these statuses:

- `planned`
- `batched`
- `simulated`
- `submitted`
- `confirmed`
- `failed`
- `skipped`

Resume behavior:

- Never resend to a `confirmed` recipient.
- For `submitted` without confirmation, check signature status before retry.
- For failed batches, retry smaller batches before single-recipient attempts.
- For `skipped`, require a config change and a new plan.

## Validation Plan

Before implementation is considered ready:

- Unit tests for config validation, amount parsing, dedupe, ranking, exclusion reasons, and secret redaction.
- Unit tests for transaction auto-packing against serialized-size, account, and instruction limits.
- Unit tests for plan math, including uneven totals and decimal conversion.
- Mock RPC tests for target holder discovery and existing distribution-token holder exclusion.
- Unit tests for Solscan holder parsing and external-provider discovery.
- Dry-run integration test against devnet or local validator with a test token.
- Mainnet-beta read-only smoke test for holder discovery and source balance.

No mainnet send test should be run without explicit user confirmation.

## Resolved Decisions

- `max_recipients` is global after merging and excluding target holders.
- The source wallet and fee payer are always the same keypair.
- V0 supports only base58/base64 private-key environment variables, not keypair files or JSON keypair arrays.
- The distribution mint must be Token-2022; target mints may be legacy SPL or Token-2022 for holder discovery.

## Deferred

- Additional indexed discovery providers beyond Solscan.
- Versioned transactions and address lookup tables for larger batches.
- Priority fee and compute budget tuning.
- Per-target-token quota strategies.
- Manual include overrides.
- Non-mainnet operational mode.
- Legacy SPL distribution-token support.

## Source Links

- Solana transaction limits: https://solana.com/docs/core/transactions
- Solana transaction structure and 1,232-byte limit: https://solana.com/docs/core/transactions/transaction-structure
- Solana batch payments: https://solana.com/docs/payments/send-payments/payment-processing/batch-payments
- Solana token transfer docs: https://solana.com/docs/tokens/basics/transfer-tokens
- Solana token account and ATA docs: https://solana.com/docs/tokens/basics/create-token-account
- Solana token program overview: https://solana.com/docs/tokens
- Solana token extensions docs: https://solana.com/docs/tokens/extensions
- Solscan token holders API docs: https://pro-api.solscan.io/pro-api-docs/v2.0/reference/token-holders
- Phantom token display docs: https://docs.phantom.com/best-practices/tokens/README
- Phantom spam flag guidance: https://help.phantom.com/hc/en-us/articles/48230217309587-A-token-I-created-is-flagged-as-spam-in-Phantom
