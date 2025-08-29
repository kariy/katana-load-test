mod result;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use abigen::{Direction, SpawnAndMoveAction};
use anyhow::{Context, Result};
use clap::Parser;
use dojo_utils::TransactionWaiter as TxWaiter;
use futures::future::{join_all, try_join_all};
use katana_primitives::contract::Nonce;
use katana_primitives::transaction::TxHash;
use katana_primitives::utils::transaction::compute_invoke_v3_tx_hash;
use katana_primitives::{felt, Felt};
use result::{BenchmarkResult, ResultsStorage};
use serde::{Deserialize, Serialize};
use starknet::accounts::{
    Account, ConnectedAccount, ExecutionEncoder, ExecutionEncoding, SingleOwnerAccount,
};
use starknet::core::types::{
    BlockId, BlockTag, BroadcastedInvokeTransaction, BroadcastedInvokeTransactionV3, Call,
    DataAvailabilityMode, ResourceBounds, ResourceBoundsMapping, TransactionReceipt,
};
use starknet::providers::jsonrpc::HttpTransport;
use starknet::providers::JsonRpcClient;
use starknet::providers::Provider;
use starknet::signers::{LocalWallet, Signer};
use tokio;
use tracing::info;
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BenchmarkMetrics {
    /// transaction per second
    tps: f64,
    /// steps per second
    sps: f64,
    /// total transactions sent throughout the benchmark
    total_transactions: u64,
    /// the average latency for each transaction submission
    avg_latency: Duration,
    /// the total duration of which the benchmark routine was executed
    total_duration: Duration,
}

struct BenchmarkConfig {
    node_url: Url,
    total_transactions: u64,
    poll_interval: Duration,
    accounts: Vec<katana_node_bindings::Account>,
}

struct Benchmarker {
    config: BenchmarkConfig,
    client: Arc<JsonRpcClient<HttpTransport>>,
}

impl Benchmarker {
    async fn new(config: BenchmarkConfig) -> Result<Self> {
        let client = Arc::new(JsonRpcClient::new(HttpTransport::new(
            config.node_url.clone(),
        )));
        Ok(Self { config, client })
    }

    async fn submit_transaction(&self, tx: BroadcastedInvokeTransaction) -> Result<TxHash> {
        let response = self.client.add_invoke_transaction(tx).await?;
        Ok(response.transaction_hash)
    }

    async fn check_status(&self, tx_hash: TxHash) -> Result<TransactionReceipt> {
        let poll_interval = self.config.poll_interval.as_millis() as u64;
        let response = TxWaiter::new(tx_hash, &self.client)
            .with_interval(poll_interval)
            .await?;
        Ok(response.receipt)
    }

    async fn run_benchmark(&self) -> Result<BenchmarkMetrics> {
        let mut pending_txs: Vec<(TxHash, Instant)> =
            Vec::with_capacity(self.config.total_transactions as usize);

        let txs = prepare_txs(
            &self.client,
            &self.config.accounts,
            self.config.total_transactions,
        )
        .await?;

        info!(txs_count = txs.len(), "Benchmark started");

        let start_time = Instant::now();
        let submission_start = Instant::now();

        // Submit transactions in parallel batches
        let batch_size = self.config.accounts.len();
        for batch in txs.chunks(batch_size) {
            let futures: Vec<_> = batch
                .iter()
                .map(|tx| self.submit_transaction(tx.clone()))
                .collect();
            let results = join_all(futures).await;

            for result in results {
                let tx_hash = result?;
                pending_txs.push((tx_hash, Instant::now()));
            }
        }

        let submission_duration = submission_start.elapsed();

        // Poll for completion of last transaction only
        let last_tx = pending_txs.last();

        if let Some((tx_hash, ..)) = last_tx {
            match self.check_status(*tx_hash).await {
                Ok(..) => {}
                Err(e) => {
                    eprintln!("Failed to check status: {e}");
                }
            }
        }

        let total_duration = start_time.elapsed();
        let total_duration_secs = total_duration.as_secs_f64();

        info!(duration = total_duration_secs, "Benchmark completed");

        // Calculate steps per second
        // let tx_hashes = pending_txs.iter().map(|(h, _)| *h).collect::<Vec<TxHash>>();
        // let total_steps = self.total_steps(&tx_hashes).await?;
        // let total_steps = self.total_steps(&tx_hashes).await?;
        // let sps = total_steps as f64 / total_duration_secs;

        // Use submission duration divided by number of transactions for avg latency
        let avg_latency = if self.config.total_transactions > 0 {
            submission_duration / self.config.total_transactions as u32
        } else {
            Duration::from_secs(0)
        };

        let tps = self.config.total_transactions as f64 / total_duration_secs;

        Ok(BenchmarkMetrics {
            tps,
            sps: 0.0,
            avg_latency,
            total_duration,
            total_transactions: self.config.total_transactions,
        })
    }

    async fn total_steps(&self, txs: &[TxHash]) -> Result<u64> {
        let receipts = self.collect_receipts(txs).await?;

        let total = receipts.iter().fold(0u64, |acc, receipt| match receipt {
            TransactionReceipt::Invoke(r) => {
                acc + r.execution_resources.computation_resources.steps
            }
            TransactionReceipt::L1Handler(r) => {
                acc + r.execution_resources.computation_resources.steps
            }
            TransactionReceipt::Declare(r) => {
                acc + r.execution_resources.computation_resources.steps
            }
            TransactionReceipt::Deploy(r) => {
                acc + r.execution_resources.computation_resources.steps
            }
            TransactionReceipt::DeployAccount(r) => {
                acc + r.execution_resources.computation_resources.steps
            }
        });

        Ok(total)
    }

    async fn collect_receipts(&self, txs: &[TxHash]) -> Result<Vec<TransactionReceipt>> {
        let mut requests = Vec::with_capacity(txs.len());

        for tx in txs {
            requests.push(self.client.get_transaction_receipt(tx));
        }

        let results = try_join_all(requests).await?;
        let receipts = results.into_iter().map(|r| r.receipt).collect();

        Ok(receipts)
    }
}

#[derive(clap::Parser, Debug)]
struct Cli {
    #[arg(long)]
    url: Url,

    #[arg(long)]
    total_txs: u64,

    #[arg(long, default_value_t = 10)]
    total_accounts: u16,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use katana_node_bindings::Katana;
    use std::env;

    tracing_subscriber::fmt().init();

    let args = Cli::parse();

    let katana = if let Ok(path) = env::var("KATANA_PATH") {
        Katana::at(path)
    } else {
        Katana::new()
    };

    let node = katana.accounts(args.total_accounts).spawn();

    let config = BenchmarkConfig {
        node_url: args.url.clone(),
        total_transactions: args.total_txs,
        accounts: node.accounts().to_vec(),
        poll_interval: Duration::from_millis(5),
    };

    let benchmarker = Benchmarker::new(config).await?;
    let metrics = benchmarker.run_benchmark().await?;

    let result = BenchmarkResult { metrics };
    let storage = ResultsStorage::new(PathBuf::from("benchmark_results"));
    storage.save_result(result.clone()).await?;

    // Generate and display comparison report
    let report = storage.generate_comparison_report(&result).await?;
    println!("{report}");

    Ok(())
}

type RpcProvider = Arc<JsonRpcClient<HttpTransport>>;
type SenderAccount = SingleOwnerAccount<RpcProvider, LocalWallet>;

async fn prepare_txs(
    provider: &RpcProvider,
    accounts: &[katana_node_bindings::Account],
    total_txs: u64,
) -> Result<Vec<BroadcastedInvokeTransaction>> {
    info!("Preparing transactions");

    let batch_size = accounts.len();
    let num_batches = (total_txs as usize + batch_size - 1) / batch_size; // Round up division
    let mut transactions = Vec::with_capacity(num_batches * batch_size);

    let chain_id = provider.chain_id().await?;
    let contract = felt!("0x1455a4751d41c1daa6a672c46037c1df81d50fa9daeea653e8946026f6653e2");

    // Create SenderAccounts
    let mut sender_accounts = Vec::with_capacity(accounts.len());
    for account in accounts {
        let signer = LocalWallet::from_signing_key(account.private_key.clone().unwrap());
        let encoding = ExecutionEncoding::New;
        let mut sender_account = SenderAccount::new(
            provider.clone(),
            signer.clone(),
            account.address,
            chain_id,
            encoding,
        );

        sender_account.set_block_id(BlockId::Tag(BlockTag::Pending));

        // Do spawn first
        let spawn_call = SpawnAndMoveAction::new(contract, &sender_account)
            .spawn()
            .send()
            .await?;

        TxWaiter::new(spawn_call.transaction_hash, provider)
            .await
            .unwrap();

        sender_accounts.push((sender_account, signer));
    }

    // Track nonce per account
    let mut nonces = HashMap::new();

    // initialize account's starting nonce
    for (sender_account, _) in &sender_accounts {
        let nonce = sender_account
            .get_nonce()
            .await
            .context("failed to get account's init nonce")?;
        nonces.insert(sender_account.address(), nonce);
    }

    // Create batches of transactions
    for _ in 0..num_batches {
        // batch size == total senders
        for (sender_account, signer) in &sender_accounts {
            let nonce = nonces.get_mut(&sender_account.address()).unwrap();
            let call =
                SpawnAndMoveAction::new(contract, sender_account).move_getcall(&Direction::None);
            transactions.push(tx(sender_account, signer, vec![call], *nonce).await);
            *nonce += Felt::ONE;
        }
    }

    Ok(transactions)
}

async fn tx(
    account: &SenderAccount,
    signer: &LocalWallet,
    calls: Vec<Call>,
    nonce: Nonce,
) -> BroadcastedInvokeTransaction {
    let calldata = account.encode_calls(&calls);
    let l1_gas_bounds = katana_primitives::fee::ResourceBounds {
        max_amount: 20000005000,
        max_price_per_unit: 20000005000,
    };

    let l2_gas_bounds = katana_primitives::fee::ResourceBounds {
        max_amount: 0,
        max_price_per_unit: 0,
    };

    let nonce_da_mode = katana_primitives::da::DataAvailabilityMode::L1;
    let fee_da_mode = katana_primitives::da::DataAvailabilityMode::L1;

    let tx_hash = compute_invoke_v3_tx_hash(
        account.address().into(),
        &calldata,
        0,
        &l1_gas_bounds,
        &l2_gas_bounds,
        &[],
        account.chain_id().into(),
        nonce.into(),
        &nonce_da_mode,
        &fee_da_mode,
        &[],
        false,
    );

    let signature = signer.sign_hash(&tx_hash).await.unwrap();

    BroadcastedInvokeTransaction::V3(BroadcastedInvokeTransactionV3 {
        calldata,
        sender_address: account.address(),
        signature: vec![signature.r, signature.s],
        nonce,
        resource_bounds: ResourceBoundsMapping {
            l1_gas: ResourceBounds {
                max_amount: 20000005000,
                max_price_per_unit: 20000005000,
            },
            // L2 resources are hard-coded to 0
            l2_gas: ResourceBounds {
                max_amount: 0,
                max_price_per_unit: 0,
            },
        },
        // Fee market has not been been activated yet so it's hard-coded to be 0
        tip: 0,
        // Hard-coded empty `paymaster_data`
        paymaster_data: vec![],
        // Hard-coded empty `account_deployment_data`
        account_deployment_data: vec![],
        // Hard-coded L1 DA mode for nonce and fee
        nonce_data_availability_mode: DataAvailabilityMode::L1,
        fee_data_availability_mode: DataAvailabilityMode::L1,
        is_query: false,
    })
}

// Generated by `abigen!`
#[allow(unused)]
mod abigen {
    pub struct SpawnAndMoveAction<A: starknet::accounts::ConnectedAccount + Sync> {
        pub address: starknet::core::types::Felt,
        pub account: A,
        pub block_id: starknet::core::types::BlockId,
    }
    impl<A: starknet::accounts::ConnectedAccount + Sync> SpawnAndMoveAction<A> {
        pub fn new(address: starknet::core::types::Felt, account: A) -> Self {
            Self {
                address,
                account,
                block_id: starknet::core::types::BlockId::Tag(
                    starknet::core::types::BlockTag::Pending,
                ),
            }
        }
        pub fn set_contract_address(&mut self, address: starknet::core::types::Felt) {
            self.address = address;
        }
        pub fn provider(&self) -> &A::Provider {
            self.account.provider()
        }
        pub fn set_block(&mut self, block_id: starknet::core::types::BlockId) {
            self.block_id = block_id;
        }
        pub fn with_block(self, block_id: starknet::core::types::BlockId) -> Self {
            Self { block_id, ..self }
        }
    }
    #[derive()]
    pub struct SpawnAndMoveActionReader<P: starknet::providers::Provider + Sync> {
        pub address: starknet::core::types::Felt,
        pub provider: P,
        pub block_id: starknet::core::types::BlockId,
    }
    impl<P: starknet::providers::Provider + Sync> SpawnAndMoveActionReader<P> {
        pub fn new(address: starknet::core::types::Felt, provider: P) -> Self {
            Self {
                address,
                provider,
                block_id: starknet::core::types::BlockId::Tag(
                    starknet::core::types::BlockTag::Pending,
                ),
            }
        }
        pub fn set_contract_address(&mut self, address: starknet::core::types::Felt) {
            self.address = address;
        }
        pub fn provider(&self) -> &P {
            &self.provider
        }
        pub fn set_block(&mut self, block_id: starknet::core::types::BlockId) {
            self.block_id = block_id;
        }
        pub fn with_block(self, block_id: starknet::core::types::BlockId) -> Self {
            Self { block_id, ..self }
        }
    }
    #[derive()]
    pub enum Direction {
        None,
        Left,
        Right,
        Up,
        Down,
    }
    impl cainome::cairo_serde::CairoSerde for Direction {
        type RustType = Self;
        const SERIALIZED_SIZE: std::option::Option<usize> = std::option::Option::None;
        #[inline]
        fn cairo_serialized_size(__rust: &Self::RustType) -> usize {
            match __rust {
                Direction::None => 1,
                Direction::Left => 1,
                Direction::Right => 1,
                Direction::Up => 1,
                Direction::Down => 1,
                _ => 0,
            }
        }
        fn cairo_serialize(__rust: &Self::RustType) -> Vec<starknet::core::types::Felt> {
            match __rust {
                Direction::None => usize::cairo_serialize(&0usize),
                Direction::Left => usize::cairo_serialize(&1usize),
                Direction::Right => usize::cairo_serialize(&2usize),
                Direction::Up => usize::cairo_serialize(&3usize),
                Direction::Down => usize::cairo_serialize(&4usize),
                _ => Vec::new(),
            }
        }
        fn cairo_deserialize(
            __felts: &[starknet::core::types::Felt],
            __offset: usize,
        ) -> cainome::cairo_serde::Result<Self::RustType> {
            let __f = __felts[__offset];
            let __index = u128::from_be_bytes(__f.to_bytes_be()[16..].try_into().unwrap());
            match __index as usize {
                0usize => Ok(Direction::None),
                1usize => Ok(Direction::Left),
                2usize => Ok(Direction::Right),
                3usize => Ok(Direction::Up),
                4usize => Ok(Direction::Down),
                _ => Err(cainome::cairo_serde::Error::Deserialize(format!(
                    "Index not handle for enum {}",
                    "Direction"
                ))),
            }
        }
    }
    impl<A: starknet::accounts::ConnectedAccount + Sync> SpawnAndMoveAction<A> {
        pub fn move_getcall(&self, direction: &Direction) -> starknet::core::types::Call {
            use cainome::cairo_serde::CairoSerde;
            let mut __calldata = Vec::new();
            __calldata.extend(Direction::cairo_serialize(direction));
            starknet::core::types::Call {
                to: self.address,
                selector: ::starknet::core::types::Felt::from_raw([
                    67542746491835804,
                    12863064398400549321,
                    17438469324404153095,
                    10377031306845665431,
                ]),
                calldata: __calldata,
            }
        }

        pub fn spawn(&self) -> starknet::accounts::ExecutionV3<'_, A> {
            let mut __calldata = Vec::new();
            let __call = starknet::core::types::Call {
                to: self.address,
                selector: ::starknet::core::types::Felt::from_raw([
                    427316234342132431,
                    10119134573058282481,
                    17664319446359752539,
                    173372654641669380,
                ]),
                calldata: __calldata,
            };

            self.account.execute_v3(vec![__call])
        }
    }

    impl<P: starknet::providers::Provider + Sync> SpawnAndMoveActionReader<P> {}
}
