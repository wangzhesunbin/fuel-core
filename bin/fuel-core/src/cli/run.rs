#![allow(unused_variables)]

use crate::{
    cli::{
        default_db_path,
        run::{
            consensus::PoATriggerArgs,
            graphql::GraphQLArgs,
            tx_pool::TxPoolArgs,
        },
        ShutdownListener,
    },
    FuelService,
};
use anyhow::Context;
use clap::Parser;
use fuel_core::{
    chain_config::default_consensus_dev_key,
    combined_database::{
        CombinedDatabase,
        CombinedDatabaseConfig,
    },
    fuel_core_graphql_api::ServiceConfig as GraphQLConfig,
    producer::Config as ProducerConfig,
    service::{
        config::Trigger,
        genesis::NotifyCancel,
        Config,
        DbType,
        RelayerConsensusConfig,
        ServiceTrait,
        VMConfig,
    },
    txpool::{
        config::BlackList,
        Config as TxPoolConfig,
    },
    types::{
        blockchain::primitives::SecretKeyWrapper,
        fuel_tx::ContractId,
        fuel_vm::SecretKey,
        secrecy::Secret,
    },
};
use fuel_core_chain_config::{
    SnapshotMetadata,
    SnapshotReader,
};
use fuel_core_types::blockchain::header::StateTransitionBytecodeVersion;
use pyroscope::{
    pyroscope::PyroscopeAgentRunning,
    PyroscopeAgent,
};
use pyroscope_pprofrs::{
    pprof_backend,
    PprofConfig,
};
use std::{
    env,
    net,
    num::NonZeroU64,
    path::PathBuf,
    str::FromStr,
};
use tracing::{
    info,
    trace,
    warn,
};

#[cfg(feature = "rocksdb")]
use fuel_core::state::historical_rocksdb::StateRewindPolicy;

use super::DEFAULT_DATABASE_CACHE_SIZE;

pub const CONSENSUS_KEY_ENV: &str = "CONSENSUS_KEY_SECRET";

#[cfg(feature = "p2p")]
mod p2p;

mod consensus;
mod graphql;
mod profiling;
#[cfg(feature = "relayer")]
mod relayer;
mod tx_pool;

/// Run the Fuel client node locally.
#[derive(Debug, Clone, Parser)]
pub struct Command {
    /// Vanity name for node, used in telemetry
    #[clap(long = "service-name", default_value = "fuel-core", value_parser, env)]
    pub service_name: String,

    /// The maximum database cache size in bytes.
    #[arg(
        long = "max-database-cache-size",
        default_value_t = DEFAULT_DATABASE_CACHE_SIZE,
        env
    )]
    pub max_database_cache_size: usize,

    #[clap(
        name = "DB_PATH",
        long = "db-path",
        value_parser,
        default_value = default_db_path().into_os_string(),
        env
    )]
    pub database_path: PathBuf,

    #[clap(
        long = "db-type",
        default_value = "rocks-db",
        value_enum,
        ignore_case = true,
        env
    )]
    pub database_type: DbType,

    #[cfg(feature = "rocksdb")]
    /// Defines the state rewind policy for the database when RocksDB is enabled.
    ///
    /// The duration defines how many blocks back the rewind feature works.
    /// Assuming each block requires one second to produce.
    ///
    /// The default value is 7 days = 604800 blocks.
    ///
    /// The `BlockHeight` is `u32`, meaning the maximum possible number of blocks
    /// is less than 137 years. `2^32 / 24 / 60 / 60 / 365` = `136.1925195332` years.
    /// If the value is 136 years or more, the rewind feature is enabled for all blocks.
    #[clap(long = "state-rewind-duration", default_value = "7d", env)]
    pub state_rewind_duration: humantime::Duration,

    /// Snapshot from which to do (re)genesis. Defaults to local testnet configuration.
    #[arg(name = "SNAPSHOT", long = "snapshot", env)]
    pub snapshot: Option<PathBuf>,

    /// Prunes the db. Genesis is done from the provided snapshot or the local testnet
    /// configuration.
    #[arg(name = "DB_PRUNE", long = "db-prune", env, default_value = "false")]
    pub db_prune: bool,

    /// The determines whether to continue the services on internal error or not.
    #[clap(long = "continue-services-on-error", default_value = "false", env)]
    pub continue_on_error: bool,

    /// Should be used for local development only. Enabling debug mode:
    /// - Allows GraphQL Endpoints to arbitrarily advance blocks.
    /// - Enables debugger GraphQL Endpoints.
    /// - Allows setting `utxo_validation` to `false`.
    #[arg(long = "debug", env)]
    pub debug: bool,

    /// Enable logging of backtraces from vm errors
    #[arg(long = "vm-backtrace", env)]
    pub vm_backtrace: bool,

    /// Enable full utxo stateful validation
    /// disabled by default until downstream consumers stabilize
    #[arg(long = "utxo-validation", env)]
    pub utxo_validation: bool,

    /// Overrides the version of the native executor.
    #[arg(long = "native-executor-version", env)]
    pub native_executor_version: Option<StateTransitionBytecodeVersion>,

    /// The minimum allowed gas price
    #[arg(long = "min-gas-price", default_value = "0", env)]
    pub min_gas_price: u64,

    /// The signing key used when producing blocks.
    /// Setting via the `CONSENSUS_KEY_SECRET` ENV var is preferred.
    #[arg(long = "consensus-key", env)]
    pub consensus_key: Option<String>,

    /// A new block is produced instantly when transactions are available.
    #[clap(flatten)]
    pub poa_trigger: PoATriggerArgs,

    /// The block's fee recipient public key.
    ///
    /// If not set, `consensus_key` is used as the provider of the `Address`.
    #[arg(long = "coinbase-recipient", env)]
    pub coinbase_recipient: Option<ContractId>,

    /// The cli arguments supported by the `TxPool`.
    #[clap(flatten)]
    pub tx_pool: TxPoolArgs,

    /// The cli arguments supported by the GraphQL API service.
    #[clap(flatten)]
    pub graphql: GraphQLArgs,

    #[cfg_attr(feature = "relayer", clap(flatten))]
    #[cfg(feature = "relayer")]
    pub relayer_args: relayer::RelayerArgs,

    #[cfg_attr(feature = "p2p", clap(flatten))]
    #[cfg(feature = "p2p")]
    pub p2p_args: p2p::P2PArgs,

    #[cfg_attr(feature = "p2p", clap(flatten))]
    #[cfg(feature = "p2p")]
    pub sync_args: p2p::SyncArgs,

    #[arg(long = "metrics", env)]
    pub metrics: bool,

    #[clap(long = "verify-max-da-lag", default_value = "10", env)]
    pub max_da_lag: u64,

    #[clap(long = "verify-max-relayer-wait", default_value = "30s", env)]
    pub max_wait_time: humantime::Duration,

    /// The number of reserved peers to connect to before starting to sync.
    #[clap(long = "min-connected-reserved-peers", default_value = "0", env)]
    pub min_connected_reserved_peers: usize,

    /// Time to wait after receiving the latest block before considered to be Synced.
    #[clap(long = "time-until-synced", default_value = "0s", env)]
    pub time_until_synced: humantime::Duration,

    /// The size of the memory pool in number of `MemoryInstance`s.
    #[clap(long = "memory-pool-size", default_value = "32", env)]
    pub memory_pool_size: usize,

    #[clap(flatten)]
    pub profiling: profiling::ProfilingArgs,
}

impl Command {
    pub fn get_config(self) -> anyhow::Result<Config> {
        let Command {
            service_name: name,
            max_database_cache_size,
            database_path,
            database_type,
            #[cfg(feature = "rocksdb")]
            state_rewind_duration,
            db_prune,
            snapshot,
            continue_on_error,
            vm_backtrace,
            debug,
            utxo_validation,
            native_executor_version,
            min_gas_price,
            consensus_key,
            poa_trigger,
            coinbase_recipient,
            #[cfg(feature = "relayer")]
            relayer_args,
            #[cfg(feature = "p2p")]
            p2p_args,
            #[cfg(feature = "p2p")]
            sync_args,
            metrics,
            max_da_lag,
            max_wait_time,
            tx_pool,
            graphql,
            min_connected_reserved_peers,
            time_until_synced,
            memory_pool_size,
            profiling: _,
        } = self;

        let addr = net::SocketAddr::new(graphql.ip, graphql.port);

        let snapshot_reader = match snapshot.as_ref() {
            None => crate::cli::local_testnet_reader(),
            Some(path) => {
                let metadata = SnapshotMetadata::read(path)?;
                SnapshotReader::open(metadata)?
            }
        };
        let chain_config = snapshot_reader.chain_config();

        #[cfg(feature = "relayer")]
        let relayer_cfg = relayer_args.into_config();

        #[cfg(feature = "p2p")]
        let p2p_cfg = p2p_args.into_config(chain_config.chain_name.clone(), metrics)?;

        let trigger: Trigger = poa_trigger.into();

        if trigger != Trigger::Never {
            info!("Block production mode: {:?}", &trigger);
        } else {
            info!("Block production disabled");
        }

        let consensus_key = load_consensus_key(consensus_key)?;
        if consensus_key.is_some() && trigger == Trigger::Never {
            warn!("Consensus key configured but block production is disabled!");
        }

        // if consensus key is not configured, fallback to dev consensus key
        let consensus_key = consensus_key.or_else(|| {
            if debug {
                let key = default_consensus_dev_key();
                warn!(
                    "Fuel Core is using an insecure test key for consensus. Public key: {}",
                    key.public_key()
                );
                Some(Secret::new(key.into()))
            } else {
                None
            }
        });

        let coinbase_recipient = if let Some(coinbase_recipient) = coinbase_recipient {
            Some(coinbase_recipient)
        } else {
            tracing::warn!("The coinbase recipient `ContractId` is not set!");
            None
        };

        let verifier = RelayerConsensusConfig {
            max_da_lag: max_da_lag.into(),
            max_wait_time: max_wait_time.into(),
        };

        #[cfg(feature = "rocksdb")]
        let state_rewind_policy = {
            if database_type != DbType::RocksDb {
                tracing::warn!("State rewind policy is only supported with RocksDB");
            }

            let blocks = state_rewind_duration.as_secs();

            if blocks == 0 {
                StateRewindPolicy::NoRewind
            } else {
                let maximum_blocks: humantime::Duration = "136y".parse()?;

                if blocks >= maximum_blocks.as_secs() {
                    StateRewindPolicy::RewindFullRange
                } else {
                    StateRewindPolicy::RewindRange {
                        size: NonZeroU64::new(blocks)
                            .expect("The value is not zero above"),
                    }
                }
            }
        };

        let combined_db_config = CombinedDatabaseConfig {
            database_path,
            database_type,
            max_database_cache_size,
            #[cfg(feature = "rocksdb")]
            state_rewind_policy,
        };

        let block_importer =
            fuel_core::service::config::fuel_core_importer::Config::new();

        let TxPoolArgs {
            tx_pool_ttl,
            tx_max_number,
            tx_max_depth,
            tx_number_active_subscriptions,
            tx_blacklist_addresses,
            tx_blacklist_coins,
            tx_blacklist_messages,
            tx_blacklist_contracts,
        } = tx_pool;

        let blacklist = BlackList::new(
            tx_blacklist_addresses,
            tx_blacklist_coins,
            tx_blacklist_messages,
            tx_blacklist_contracts,
        );

        let config = Config {
            graphql_config: GraphQLConfig {
                addr,
                max_queries_depth: graphql.graphql_max_depth,
                max_queries_complexity: graphql.graphql_max_complexity,
                max_queries_recursive_depth: graphql.graphql_max_recursive_depth,
                request_body_bytes_limit: graphql.graphql_request_body_bytes_limit,
                api_request_timeout: graphql.api_request_timeout.into(),
                query_log_threshold_time: graphql.query_log_threshold_time.into(),
            },
            combined_db_config,
            snapshot_reader,
            debug,
            native_executor_version,
            continue_on_error,
            utxo_validation,
            block_production: trigger,
            vm: VMConfig {
                backtrace: vm_backtrace,
            },
            txpool: TxPoolConfig::new(
                tx_max_number,
                tx_max_depth,
                utxo_validation,
                metrics,
                tx_pool_ttl.into(),
                tx_number_active_subscriptions,
                blacklist,
            ),
            block_producer: ProducerConfig {
                coinbase_recipient,
                metrics,
            },
            static_gas_price: min_gas_price,
            block_importer,
            #[cfg(feature = "relayer")]
            relayer: relayer_cfg,
            #[cfg(feature = "p2p")]
            p2p: p2p_cfg,
            #[cfg(feature = "p2p")]
            sync: sync_args.into(),
            consensus_key,
            name,
            relayer_consensus_config: verifier,
            min_connected_reserved_peers,
            time_until_synced: time_until_synced.into(),
            memory_pool_size,
        };
        Ok(config)
    }
}

pub fn get_service(command: Command) -> anyhow::Result<FuelService> {
    #[cfg(feature = "rocksdb")]
    if command.db_prune && command.database_path.exists() {
        fuel_core::combined_database::CombinedDatabase::prune(&command.database_path)?;
    }

    let profiling = command.profiling.clone();
    let config = command.get_config()?;

    // start profiling agent if url is configured
    let _profiling_agent = start_pyroscope_agent(profiling, &config)?;

    // log fuel-core version
    info!("Fuel Core version v{}", env!("CARGO_PKG_VERSION"));
    trace!("Initializing in TRACE mode.");
    // initialize the server
    let combined_database = CombinedDatabase::from_config(&config.combined_db_config)?;

    FuelService::new(combined_database, config)
}

pub async fn exec(command: Command) -> anyhow::Result<()> {
    let service = get_service(command)?;

    let shutdown_listener = ShutdownListener::spawn();
    // Genesis could take a long time depending on the snapshot size. Start needs to be
    // interruptible by the shutdown_signal
    tokio::select! {
        result = service.start_and_await() => {
            result?;
        }
        _ = shutdown_listener.wait_until_cancelled() => {
            service.stop();
        }
    }

    // pause the main task while service is running
    tokio::select! {
        result = service.await_stop() => {
            result?;
        }
        _ = shutdown_listener.wait_until_cancelled() => {}
    }

    service.stop_and_await().await?;

    Ok(())
}

// Attempt to load the consensus key from cli arg first, otherwise check the env.
fn load_consensus_key(
    cli_arg: Option<String>,
) -> anyhow::Result<Option<Secret<SecretKeyWrapper>>> {
    let secret_string = if let Some(cli_arg) = cli_arg {
        warn!("Consensus key configured insecurely using cli args. Consider setting the {} env var instead.", CONSENSUS_KEY_ENV);
        Some(cli_arg)
    } else {
        env::var(CONSENSUS_KEY_ENV).ok()
    };

    if let Some(key) = secret_string {
        let key =
            SecretKey::from_str(&key).context("failed to parse consensus signing key")?;
        Ok(Some(Secret::new(key.into())))
    } else {
        Ok(None)
    }
}

fn start_pyroscope_agent(
    profiling_args: profiling::ProfilingArgs,
    config: &Config,
) -> anyhow::Result<Option<PyroscopeAgent<PyroscopeAgentRunning>>> {
    profiling_args
        .pyroscope_url
        .as_ref()
        .map(|url| -> anyhow::Result<_> {
            let chain_config = config.snapshot_reader.chain_config();
            let agent = PyroscopeAgent::builder(url, &"fuel-core".to_string())
                .tags(vec![
                    ("service", config.name.as_str()),
                    ("network", chain_config.chain_name.as_str()),
                ])
                .backend(pprof_backend(
                    PprofConfig::new().sample_rate(profiling_args.pprof_sample_rate),
                ))
                .build()
                .context("failed to start profiler")?;
            let agent_running = agent.start().unwrap();
            Ok(agent_running)
        })
        .transpose()
}
