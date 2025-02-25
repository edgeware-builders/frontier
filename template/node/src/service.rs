//! Service and ServiceFactory implementation. Specialized wrapper over substrate service.

use std::{sync::{Arc, Mutex}, cell::RefCell, time::Duration, collections::{HashMap, BTreeMap}};
use fc_rpc::EthTask;
use fc_rpc_core::types::{FilterPool, PendingTransactions};
use sc_client_api::{ExecutorProvider, RemoteBackend, BlockchainEvents};
#[cfg(feature = "manual-seal")]
use sc_consensus_manual_seal::{self as manual_seal};
use fc_consensus::FrontierBlockImport;
use fc_mapping_sync::MappingSyncWorker;
use frontier_template_runtime::{self, opaque::Block, RuntimeApi, SLOT_DURATION};
use sc_service::{error::Error as ServiceError, Configuration, TaskManager, BasePath};
use sc_executor::native_executor_instance;
pub use sc_executor::NativeExecutor;
use sc_consensus_aura::{ImportQueueParams, StartAuraParams, SlotProportion};
use sp_consensus_aura::sr25519::{AuthorityPair as AuraPair};
use sc_finality_grandpa::SharedVoterState;
use sp_timestamp::InherentError;
use sc_telemetry::{Telemetry, TelemetryWorker};
use sc_cli::SubstrateCli;
use futures::StreamExt;
use sp_core::U256;
use sp_consensus::SlotData;
use sc_consensus_aura::slot_duration;

use crate::cli::Cli;
#[cfg(feature = "manual-seal")]
use crate::cli::Sealing;

// Our native executor instance.
native_executor_instance!(
	pub Executor,
	frontier_template_runtime::api::dispatch,
	frontier_template_runtime::native_version,
);

type FullClient = sc_service::TFullClient<Block, RuntimeApi, Executor>;
type FullBackend = sc_service::TFullBackend<Block>;
type FullSelectChain = sc_consensus::LongestChain<FullBackend, Block>;

#[cfg(feature = "aura")]
pub type ConsensusResult = (
	sc_consensus_aura::AuraBlockImport<
		Block,
		FullClient,
		FrontierBlockImport<
			Block,
			sc_finality_grandpa::GrandpaBlockImport<FullBackend, Block, FullClient, FullSelectChain>,
			FullClient
		>,
		AuraPair
	>,
	sc_finality_grandpa::LinkHalf<Block, FullClient, FullSelectChain>
);

#[cfg(feature = "manual-seal")]
pub type ConsensusResult = (FrontierBlockImport<Block, Arc<FullClient>, FullClient>, Sealing);

pub fn open_frontier_backend(config: &Configuration) -> Result<Arc<fc_db::Backend<Block>>, String> {
	let config_dir = config.base_path.as_ref()
		.map(|base_path| base_path.config_dir(config.chain_spec.id()))
		.unwrap_or_else(|| {
			BasePath::from_project("", "", &crate::cli::Cli::executable_name())
				.config_dir(config.chain_spec.id())
		});
	let database_dir = config_dir.join("frontier").join("db");

	Ok(Arc::new(fc_db::Backend::<Block>::new(&fc_db::DatabaseSettings {
		source: fc_db::DatabaseSettingsSrc::RocksDb {
			path: database_dir,
			cache_size: 0,
		}
	})?))
}

pub fn new_partial(config: &Configuration, #[allow(unused_variables)] cli: &Cli) -> Result<
	sc_service::PartialComponents<
		FullClient, FullBackend, FullSelectChain,
		sp_consensus::import_queue::BasicQueue<Block, sp_api::TransactionFor<FullClient, Block>>,
		sc_transaction_pool::FullPool<Block, FullClient>,
		(ConsensusResult, PendingTransactions, Option<FilterPool>, Arc<fc_db::Backend<Block>>, Option<Telemetry>),
>, ServiceError> {
	let target_gas_price = U256::from(cli.run.target_gas_price);

	let telemetry = config.telemetry_endpoints.clone()
		.filter(|x| !x.is_empty())
		.map(|endpoints| -> Result<_, sc_telemetry::Error> {
			let worker = TelemetryWorker::new(16)?;
			let telemetry = worker.handle().new_telemetry(endpoints);
			Ok((worker, telemetry))
		})
		.transpose()?;

	let (client, backend, keystore_container, task_manager) =
		sc_service::new_full_parts::<Block, RuntimeApi, Executor>(
			&config,
			telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
		)?;
	let client = Arc::new(client);

	let telemetry = telemetry
		.map(|(worker, telemetry)| {
			task_manager.spawn_handle().spawn("telemetry", worker.run());
			telemetry
		});

	let select_chain = sc_consensus::LongestChain::new(backend.clone());

	let transaction_pool = sc_transaction_pool::BasicPool::new_full(
		config.transaction_pool.clone(),
		config.role.is_authority().into(),
		config.prometheus_registry(),
		task_manager.spawn_handle(),
		client.clone(),
	);

	let pending_transactions: PendingTransactions
		= Some(Arc::new(Mutex::new(HashMap::new())));

	let filter_pool: Option<FilterPool>
		= Some(Arc::new(Mutex::new(BTreeMap::new())));

	let frontier_backend = open_frontier_backend(config)?;

	#[cfg(feature = "manual-seal")] {
		let sealing = cli.run.sealing;

		let frontier_block_import = FrontierBlockImport::new(
			client.clone(),
			client.clone(),
			frontier_backend.clone(),
		);

		let import_queue = sc_consensus_manual_seal::import_queue(
			Box::new(frontier_block_import.clone()),
			&task_manager.spawn_essential_handle(),
			config.prometheus_registry(),
		);

		Ok(sc_service::PartialComponents {
			client, backend, task_manager, import_queue, keystore_container,
			select_chain, transaction_pool,
			other: (
				(frontier_block_import, sealing),
				pending_transactions,
				filter_pool,
				frontier_backend,
				telemetry,
			)
		})
	}

	#[cfg(feature = "aura")] {
		let (grandpa_block_import, grandpa_link) = sc_finality_grandpa::block_import(
			client.clone(),
			&(client.clone() as Arc<_>),
			select_chain.clone(),
			telemetry.as_ref().map(|x| x.handle()),
		)?;

		let frontier_block_import = FrontierBlockImport::new(
			grandpa_block_import.clone(),
			client.clone(),
			frontier_backend.clone(),
		);

		let aura_block_import = sc_consensus_aura::AuraBlockImport::<_, _, _, AuraPair>::new(
			frontier_block_import, client.clone(),
		);

		let slot_duration = sc_consensus_aura::slot_duration(&*client)?;
		let raw_slot_duration = slot_duration.slot_duration();

		let import_queue = sc_consensus_aura::import_queue::<AuraPair, _, _, _, _, _, _>(
			ImportQueueParams {
				block_import: aura_block_import.clone(),
				justification_import: Some(Box::new(grandpa_block_import.clone())),
				client: client.clone(),
				create_inherent_data_providers: move |_, ()| async move {
					let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

					let slot =
						sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_duration(
							*timestamp,
							raw_slot_duration,
						);

					let fee = pallet_dynamic_fee::InherentDataProvider(target_gas_price);

					Ok((timestamp, slot, fee))
				},
				spawner: &task_manager.spawn_essential_handle(),
				can_author_with: sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone()),
				registry: config.prometheus_registry(),
				check_for_equivocation: Default::default(),
				telemetry: telemetry.as_ref().map(|x| x.handle()),
			},
		)?;

		Ok(sc_service::PartialComponents {
			client, backend, task_manager, import_queue, keystore_container,
			select_chain, transaction_pool,
			other: (
				(aura_block_import, grandpa_link),
				pending_transactions,
				filter_pool,
				frontier_backend,
				telemetry,
			)
		})
	}
}

/// Builds a new service for a full client.
pub fn new_full(
	config: Configuration,
	cli: &Cli,
) -> Result<TaskManager, ServiceError> {
	let enable_dev_signer = cli.run.enable_dev_signer;
	let target_gas_price = U256::from(cli.run.target_gas_price);

	let sc_service::PartialComponents {
		client, backend, mut task_manager, import_queue, keystore_container,
		select_chain, transaction_pool,
		other: (consensus_result, pending_transactions, filter_pool, frontier_backend, mut telemetry),
	} = new_partial(&config, cli)?;

	let (network, network_status_sinks, system_rpc_tx, network_starter) =
		sc_service::build_network(sc_service::BuildNetworkParams {
			config: &config,
			client: client.clone(),
			transaction_pool: transaction_pool.clone(),
			spawn_handle: task_manager.spawn_handle(),
			import_queue,
			on_demand: None,
			block_announce_validator_builder: None,
		})?;

	// Channel for the rpc handler to communicate with the authorship task.
	let (command_sink, commands_stream) = futures::channel::mpsc::channel(1000);

	if config.offchain_worker.enabled {
		sc_service::build_offchain_workers(
			&config, task_manager.spawn_handle(), client.clone(), network.clone(),
		);
	}

	let role = config.role.clone();
	let force_authoring = config.force_authoring;
	let backoff_authoring_blocks: Option<()> = None;
	let name = config.network.node_name.clone();
	let enable_grandpa = !config.disable_grandpa;
	let prometheus_registry = config.prometheus_registry().cloned();
	let is_authority = role.is_authority();
	let subscription_task_executor = sc_rpc::SubscriptionTaskExecutor::new(task_manager.spawn_handle());

	let rpc_extensions_builder = {
		let client = client.clone();
		let pool = transaction_pool.clone();
		let network = network.clone();
		let pending = pending_transactions.clone();
		let filter_pool = filter_pool.clone();
		let frontier_backend = frontier_backend.clone();
		let max_past_logs = cli.run.max_past_logs;

		Box::new(move |deny_unsafe, _| {
			let deps = crate::rpc::FullDeps {
				client: client.clone(),
				pool: pool.clone(),
				deny_unsafe,
				is_authority,
				enable_dev_signer,
				network: network.clone(),
				pending_transactions: pending.clone(),
				filter_pool: filter_pool.clone(),
				backend: frontier_backend.clone(),
				max_past_logs,
				command_sink: Some(command_sink.clone()),
			};
			crate::rpc::create_full(
				deps,
				subscription_task_executor.clone()
			)
		})
	};

	task_manager.spawn_essential_handle().spawn(
		"frontier-mapping-sync-worker",
		MappingSyncWorker::new(
			client.import_notification_stream(),
			Duration::new(6, 0),
			client.clone(),
			backend.clone(),
			frontier_backend.clone(),
		).for_each(|()| futures::future::ready(()))
	);

	let _rpc_handlers = sc_service::spawn_tasks(sc_service::SpawnTasksParams {
		network: network.clone(),
		client: client.clone(),
		keystore: keystore_container.sync_keystore(),
		task_manager: &mut task_manager,
		transaction_pool: transaction_pool.clone(),
		rpc_extensions_builder: rpc_extensions_builder,
		on_demand: None,
		remote_blockchain: None,
		backend, network_status_sinks, system_rpc_tx, config, telemetry: telemetry.as_mut(),
	})?;

	// Spawn Frontier EthFilterApi maintenance task.
	if let Some(filter_pool) = filter_pool {
		// Each filter is allowed to stay in the pool for 100 blocks.
		const FILTER_RETAIN_THRESHOLD: u64 = 100;
		task_manager.spawn_essential_handle().spawn(
			"frontier-filter-pool",
			EthTask::filter_pool_task(
					Arc::clone(&client),
					filter_pool,
					FILTER_RETAIN_THRESHOLD,
			)
		);
	}

	// Spawn Frontier pending transactions maintenance task (as essential, otherwise we leak).
	if let Some(pending_transactions) = pending_transactions {
		const TRANSACTION_RETAIN_THRESHOLD: u64 = 5;
		task_manager.spawn_essential_handle().spawn(
			"frontier-pending-transactions",
			EthTask::pending_transaction_task(
				Arc::clone(&client),
					pending_transactions,
					TRANSACTION_RETAIN_THRESHOLD,
				)
		);
	}

	task_manager.spawn_essential_handle().spawn(
		"frontier-schema-cache-task",
		EthTask::ethereum_schema_cache_task(Arc::clone(&client), Arc::clone(&frontier_backend)),
	);
	#[cfg(feature = "manual-seal")]
	{
		let (block_import, sealing) = consensus_result;

		if role.is_authority() {
			let env = sc_basic_authorship::ProposerFactory::new(
				task_manager.spawn_handle(),
				client.clone(),
				transaction_pool.clone(),
				prometheus_registry.as_ref(),
				telemetry.as_ref().map(|x| x.handle()),
			);

			// Background authorship future
			match sealing {
				Sealing::Manual => {
					let authorship_future = manual_seal::run_manual_seal(
						manual_seal::ManualSealParams {
							block_import,
							env,
							client,
							pool: transaction_pool.pool().clone(),
							commands_stream,
							select_chain,
							consensus_data_provider: None,
							inherent_data_providers,
						}
					);
					// we spawn the future on a background thread managed by service.
					task_manager.spawn_essential_handle().spawn_blocking("manual-seal", authorship_future);
				},
				Sealing::Instant => {
					let authorship_future = manual_seal::run_instant_seal(
						manual_seal::InstantSealParams {
							block_import,
							env,
							client: client.clone(),
							pool: transaction_pool.pool().clone(),
							select_chain,
							consensus_data_provider: None,
							inherent_data_providers,
						}
					);
					// we spawn the future on a background thread managed by service.
					task_manager.spawn_essential_handle().spawn_blocking("instant-seal", authorship_future);
				}
			};

		}
		log::info!("Manual Seal Ready");
	}

	#[cfg(feature = "aura")] {
		let (aura_block_import, grandpa_link) = consensus_result;

		if role.is_authority() {
			let proposer = sc_basic_authorship::ProposerFactory::new(
				task_manager.spawn_handle(),
				client.clone(),
				transaction_pool.clone(),
				prometheus_registry.as_ref(),
				telemetry.as_ref().map(|x| x.handle()),
			);

			let can_author_with =
				sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone());

			let slot_duration = sc_consensus_aura::slot_duration(&*client)?;
			let raw_slot_duration = slot_duration.slot_duration();

			let aura = sc_consensus_aura::start_aura::<AuraPair, _, _, _, _, _, _, _, _, _, _>(
				StartAuraParams {
					slot_duration,
					client: client.clone(),
					select_chain,
					block_import: aura_block_import,
					proposer_factory: proposer,
					create_inherent_data_providers: move |_, ()| async move {
						let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

						let slot =
							sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_duration(
								*timestamp,
								raw_slot_duration,
							);

						let fee = pallet_dynamic_fee::InherentDataProvider(target_gas_price);

						Ok((timestamp, slot, fee))
					},
					force_authoring,
					backoff_authoring_blocks,
					keystore: keystore_container.sync_keystore(),
					can_author_with,
					sync_oracle: network.clone(),
					block_proposal_slot_portion: SlotProportion::new(2f32 / 3f32),
					telemetry: telemetry.as_ref().map(|x| x.handle()),
				},
			)?;

			// the AURA authoring task is considered essential, i.e. if it
			// fails we take down the service with it.
			task_manager.spawn_essential_handle().spawn_blocking("aura", aura);

			// if the node isn't actively participating in consensus then it doesn't
			// need a keystore, regardless of which protocol we use below.
			let keystore = if role.is_authority() {
				Some(keystore_container.sync_keystore())
			} else {
				None
			};

			let grandpa_config = sc_finality_grandpa::Config {
				// FIXME #1578 make this available through chainspec
				gossip_duration: Duration::from_millis(333),
				justification_period: 512,
				name: Some(name),
				observer_enabled: false,
				keystore,
				is_authority: role.is_authority(),
				telemetry: telemetry.as_ref().map(|x| x.handle()),
			};

			if enable_grandpa {
				// start the full GRANDPA voter
				// NOTE: non-authorities could run the GRANDPA observer protocol, but at
				// this point the full voter should provide better guarantees of block
				// and vote data availability than the observer. The observer has not
				// been tested extensively yet and having most nodes in a network run it
				// could lead to finality stalls.
				let grandpa_config = sc_finality_grandpa::GrandpaParams {
					config: grandpa_config,
					link: grandpa_link,
					network,
					telemetry: telemetry.as_ref().map(|x| x.handle()),
					voting_rule: sc_finality_grandpa::VotingRulesBuilder::default().build(),
					prometheus_registry,
					shared_voter_state: SharedVoterState::empty(),
				};

				// the GRANDPA voter task is considered infallible, i.e.
				// if it fails we take down the service with it.
				task_manager.spawn_essential_handle().spawn_blocking(
					"grandpa-voter",
					sc_finality_grandpa::run_grandpa_voter(grandpa_config)?
				);
			}
		}
	}

	network_starter.start_network();
	Ok(task_manager)
}

#[cfg(feature = "aura")]
pub fn new_light(config: Configuration) -> Result<TaskManager, ServiceError> {
	let telemetry = config.telemetry_endpoints.clone()
		.filter(|x| !x.is_empty())
		.map(|endpoints| -> Result<_, sc_telemetry::Error> {
			let worker = TelemetryWorker::new(16)?;
			let telemetry = worker.handle().new_telemetry(endpoints);
			Ok((worker, telemetry))
		})
		.transpose()?;

	let (client, backend, keystore_container, mut task_manager, on_demand) =
		sc_service::new_light_parts::<Block, RuntimeApi, Executor>(
			&config,
			telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
		)?;

	let mut telemetry = telemetry
		.map(|(worker, telemetry)| {
			task_manager.spawn_handle().spawn("telemetry", worker.run());
			telemetry
		});

	let select_chain = sc_consensus::LongestChain::new(backend.clone());

	let transaction_pool = Arc::new(sc_transaction_pool::BasicPool::new_light(
		config.transaction_pool.clone(),
		config.prometheus_registry(),
		task_manager.spawn_handle(),
		client.clone(),
		on_demand.clone(),
	));

	let (grandpa_block_import, _) = sc_finality_grandpa::block_import(
		client.clone(),
		&(client.clone() as Arc<_>),
		select_chain.clone(),
		telemetry.as_ref().map(|x| x.handle()),
	)?;

	let slot_duration = sc_consensus_aura::slot_duration(&*client)?.slot_duration();

	let import_queue = sc_consensus_aura::import_queue::<AuraPair, _, _, _, _, _, _>(
		ImportQueueParams {
			block_import: grandpa_block_import.clone(),
			justification_import: Some(Box::new(grandpa_block_import.clone())),
			client: client.clone(),
			create_inherent_data_providers: move |_, ()| async move {
				let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

				let slot =
					sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_duration(
						*timestamp,
						slot_duration,
					);

				Ok((timestamp, slot))
			},
			spawner: &task_manager.spawn_essential_handle(),
			can_author_with: sp_consensus::NeverCanAuthor,
			registry: config.prometheus_registry(),
			check_for_equivocation: Default::default(),
			telemetry: telemetry.as_ref().map(|x| x.handle()),
		},
	)?;

	let light_deps = crate::rpc::LightDeps {
		remote_blockchain: backend.remote_blockchain(),
		fetcher: on_demand.clone(),
		client: client.clone(),
		pool: transaction_pool.clone(),
	};

	let rpc_extensions = crate::rpc::create_light(light_deps);

	let (network, network_status_sinks, system_rpc_tx, network_starter) =
		sc_service::build_network(sc_service::BuildNetworkParams {
			config: &config,
			client: client.clone(),
			transaction_pool: transaction_pool.clone(),
			spawn_handle: task_manager.spawn_handle(),
			import_queue,
			on_demand: Some(on_demand.clone()),
			block_announce_validator_builder: None,
		})?;

	if config.offchain_worker.enabled {
		sc_service::build_offchain_workers(
			&config, task_manager.spawn_handle(), client.clone(), network.clone(),
		);
	}

	sc_service::spawn_tasks(sc_service::SpawnTasksParams {
		remote_blockchain: Some(backend.remote_blockchain()),
		transaction_pool,
		task_manager: &mut task_manager,
		on_demand: Some(on_demand),
		rpc_extensions_builder: Box::new(sc_service::NoopRpcExtensionBuilder(rpc_extensions)),
		config,
		client,
		keystore: keystore_container.sync_keystore(),
		backend,
		network,
		network_status_sinks,
		system_rpc_tx,
		telemetry: telemetry.as_mut(),
	})?;

	network_starter.start_network();

	Ok(task_manager)
}

#[cfg(feature = "manual-seal")]
pub fn new_light(config: Configuration) -> Result<TaskManager, ServiceError> {
	return Err(ServiceError::Other("Manual seal does not support light client".to_string()))
}
