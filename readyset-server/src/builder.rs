use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{self, Duration};

use dataflow::PersistenceParameters;
use readyset::consensus::{
    Authority, LocalAuthority, LocalAuthorityStore, NodeTypeSchedulingRestriction,
    WorkerSchedulingConfig,
};

use crate::controller::replication::ReplicationStrategy;
use crate::handle::Handle;
use crate::{Config, FrontierStrategy, ReuseConfigType, VolumeId};

/// Used to construct a worker.
#[derive(Clone)]
pub struct Builder {
    config: Config,
    memory_limit: Option<usize>,
    memory_check_frequency: Option<time::Duration>,
    listen_addr: IpAddr,
    external_addr: SocketAddr,
    leader_eligible: bool,
    domain_scheduling_config: WorkerSchedulingConfig,
}

impl Default for Builder {
    fn default() -> Self {
        #[allow(clippy::unwrap_used)] // hardcoded literals
        Self {
            config: Config::default(),
            listen_addr: "127.0.0.1".parse().unwrap(),
            external_addr: "127.0.0.1:6033".parse().unwrap(),
            memory_limit: None,
            memory_check_frequency: None,
            leader_eligible: true,
            domain_scheduling_config: Default::default(),
        }
    }
}

impl Builder {
    /// Initialize a [`Builder`] from a set of command-line worker options and a deployment name.
    pub fn from_worker_options(opts: crate::WorkerOptions, deployment: &str) -> Self {
        let mut builder = Self::default();
        if opts.memory > 0 {
            builder.set_memory_limit(opts.memory, Duration::from_secs(opts.memory_check_freq));
        }
        builder.set_eviction_kind(opts.eviction_kind);

        builder.set_sharding(match opts.shards {
            0 | 1 => None,
            x => Some(x),
        });
        builder.set_quorum(opts.quorum);
        if opts.no_partial {
            builder.disable_partial();
        }
        if opts.forbid_full_materialization {
            builder.forbid_full_materialization();
        }
        if opts.enable_packet_filters {
            builder.enable_packet_filters();
        }

        // TODO(fran): Reuse will be disabled until we refactor MIR to make it serializable.
        // See `noria/server/src/controller/sql/serde.rs` for details.
        builder.set_reuse(None);

        builder.set_allow_topk(opts.enable_experimental_topk_support);
        builder.set_allow_paginate(opts.enable_experimental_paginate_support);
        builder.set_allow_mixed_comparisons(opts.enable_experimental_mixed_comparisons);

        builder.set_replication_strategy(opts.domain_replication_options.into());

        if let Some(volume_id) = opts.volume_id {
            builder.set_volume_id(volume_id);
        }

        let persistence_params = PersistenceParameters::new(
            opts.durability,
            Some(deployment.into()),
            opts.persistence_threads,
            opts.db_dir,
        );
        builder.set_persistence(persistence_params);

        builder.set_replicator_config(opts.replicator_config);

        builder
    }

    /// Construct a new [`Builder`] with configuration setup for running tests
    pub fn for_tests() -> Self {
        let mut builder = Self::default();
        builder.set_abort_on_task_failure(false);
        builder
    }

    /// Set the persistence parameters used by the system.
    pub fn set_persistence(&mut self, p: PersistenceParameters) {
        self.config.persistence = p;
    }

    /// Disable partial materialization for all subsequent migrations
    pub fn disable_partial(&mut self) {
        self.config.materialization_config.partial_enabled = false;
    }

    /// Enable the creation of [`PacketFilter`]s for egresses before readers
    ///
    /// [`PacketFilter`]: noria_dataflow::node::special::PacketFilter
    pub fn enable_packet_filters(&mut self) {
        self.config.materialization_config.packet_filters_enabled = true;
    }

    /// Which nodes should be placed beyond the materialization frontier?
    pub fn set_frontier_strategy(&mut self, f: FrontierStrategy) {
        self.config.materialization_config.frontier_strategy = f;
    }

    /// Forbid the creation of all fully materialized nodes.
    ///
    /// After this is called, any migrations that add fully materialized nodes will return
    /// [`ReadySetError::Unsupported`]
    pub fn forbid_full_materialization(&mut self) {
        self.config
            .materialization_config
            .allow_full_materialization = false;
    }

    /// Set sharding policy for all subsequent migrations; `None` or `Some(x)` where x <= 1 disables
    pub fn set_sharding(&mut self, shards: Option<usize>) {
        self.config.sharding = shards.filter(|s| *s > 1);
    }

    /// Set how many workers this worker should wait for before becoming a controller. More workers
    /// can join later, but they won't be assigned any of the initial domains.
    pub fn set_quorum(&mut self, quorum: usize) {
        assert_ne!(quorum, 0);
        self.config.quorum = quorum;
    }

    /// Set the memory limit (target) and how often we check it (in millis).
    pub fn set_memory_limit(&mut self, limit: usize, check_freq: time::Duration) {
        assert_ne!(limit, 0);
        assert_ne!(check_freq, time::Duration::from_millis(0));
        self.memory_limit = Some(limit);
        self.memory_check_frequency = Some(check_freq);
    }

    /// Set the IP address that the worker should use for listening.
    pub fn set_listen_addr(&mut self, listen_addr: IpAddr) {
        self.listen_addr = listen_addr;
    }

    /// Set the external IP address and port that the worker should advertise to
    /// other noria instances
    pub fn set_external_addr(&mut self, external_addr: SocketAddr) {
        self.external_addr = external_addr;
    }

    /// Set the reuse policy for all subsequent migrations
    pub fn set_reuse(&mut self, reuse_type: Option<ReuseConfigType>) {
        self.config.reuse = reuse_type;
    }

    /// Set the value of [`controller::sql::Config::allow_topk`]
    pub fn set_allow_topk(&mut self, allow_topk: bool) {
        self.config.mir_config.allow_topk = allow_topk;
    }

    /// Set the value of [`controller::sql::Config::allow_paginate`]
    pub fn set_allow_paginate(&mut self, allow_paginate: bool) {
        self.config.mir_config.allow_paginate = allow_paginate;
    }

    /// Set the value of [`controller::sql::Config::allow_mixed_comparisons`]
    pub fn set_allow_mixed_comparisons(&mut self, allow_mixed_comparisons: bool) {
        self.config.mir_config.allow_mixed_comparisons = allow_mixed_comparisons;
    }

    /// Set the value of [`DomainConfig::aggressively_update_state_sizes`][0]. See the documentation
    /// of that field for more information
    ///
    /// [0]: noria_dataflow::Config::aggressively_update_state_sizes.
    pub fn set_aggressively_update_state_sizes(&mut self, value: bool) {
        self.config.domain_config.aggressively_update_state_sizes = value;
    }

    /// Sets the URL for the database to replicate from
    pub fn set_replication_url(&mut self, url: String) {
        self.config.replicator_config.replication_url = Some(url.into());
    }

    /// Sets configuration for the replicator thread
    pub fn set_replicator_config(&mut self, config: replicators::Config) {
        self.config.replicator_config = config;
    }

    /// Sets the value of [`replicators::Config::disable_replication_ssl_verification`]
    pub fn set_disable_replication_ssl_verification(&mut self, value: bool) {
        self.config
            .replicator_config
            .disable_replication_ssl_verification = value;
    }

    /// Sets whether we should keep the chain of prior recipes when storing a new
    /// recipe. Setting this to false may have unexpected behavior and should be
    /// used with caution. It is currently only used in test environments.
    pub fn set_keep_prior_recipes(&mut self, value: bool) {
        self.config.keep_prior_recipes = value;
    }

    /// Sets the strategy to use to determine how many times to replicate domains
    pub fn set_replication_strategy(&mut self, replication_strategy: ReplicationStrategy) {
        self.config.replication_strategy = replication_strategy
    }

    /// Configures this Noria server to accept only domains that contain reader nodes.
    ///
    /// Overwrites any previous call to [`no_readers`]
    pub fn as_reader_only(&mut self) {
        self.domain_scheduling_config.reader_nodes =
            NodeTypeSchedulingRestriction::OnlyWithNodeType;
    }

    /// Configures this Noria server to never run domains that contain reader nodes
    ///
    /// Overwrites any previous call to [`as_reader_only`]
    pub fn no_readers(&mut self) {
        self.domain_scheduling_config.reader_nodes =
            NodeTypeSchedulingRestriction::NeverWithNodeType;
    }

    /// Configures this Noria server to be unable to become the leader
    pub fn cannot_become_leader(&mut self) {
        self.leader_eligible = false;
    }

    /// Configures the volume id associated with this server.
    pub fn set_volume_id(&mut self, volume_id: VolumeId) {
        self.domain_scheduling_config.volume_id = Some(volume_id);
    }

    /// Set the value of [`Config::abort_on_task_failure`]. See the documentation of that field for
    /// more information.
    pub fn set_abort_on_task_failure(&mut self, abort_on_task_failure: bool) {
        self.config.abort_on_task_failure = abort_on_task_failure;
    }

    /// Sets the value of [`Config::upquery_timeout`]. See documentation of that field for more
    /// information.
    pub fn set_upquery_timeout(&mut self, value: std::time::Duration) {
        self.config.upquery_timeout = value;
    }

    /// Sets the value of [`Config::domain_config::view_request_timeout`]. See documentation of
    /// that field for more information.
    pub fn set_view_request_timeout(&mut self, value: std::time::Duration) {
        self.config.domain_config.view_request_timeout = value;
    }

    /// Sets the value of [`Config::domain_config::table_request_timeout`]. See documentation of
    /// that field for more information.
    pub fn set_table_request_timeout(&mut self, value: std::time::Duration) {
        self.config.domain_config.table_request_timeout = value;
    }

    /// Sets the value of [`Config::replicator_restart_timeout`]. See documentation of
    /// that field for more information.
    pub fn set_replicator_restart_timeout(&mut self, value: std::time::Duration) {
        self.config.replicator_config.replicator_restart_timeout = value;
    }

    /// Sets the value of [`Config::domain_config::eviction_kind`]. See documentation of
    /// that field for more information.
    pub fn set_eviction_kind(&mut self, value: dataflow::EvictionKind) {
        self.config.domain_config.eviction_kind = value;
    }

    /// Start a server instance and return a handle to it.
    pub fn start(
        self,
        authority: Arc<Authority>,
    ) -> impl Future<Output = Result<Handle, anyhow::Error>> {
        let Builder {
            listen_addr,
            external_addr,
            ref config,
            memory_limit,
            memory_check_frequency,
            domain_scheduling_config,
            leader_eligible,
        } = self;

        let config = config.clone();

        crate::startup::start_instance(
            authority,
            listen_addr,
            external_addr,
            config,
            memory_limit,
            memory_check_frequency,
            domain_scheduling_config,
            leader_eligible,
        )
    }

    /// Start a server instance with readers already created and return a handle to it.
    pub fn start_with_readers(
        self,
        authority: Arc<Authority>,
        readers: dataflow::Readers,
        reader_addr: SocketAddr,
        valve: stream_cancel::Valve,
        trigger: stream_cancel::Trigger,
    ) -> impl Future<Output = Result<Handle, anyhow::Error>> {
        let Builder {
            listen_addr,
            external_addr,
            ref config,
            memory_limit,
            memory_check_frequency,
            domain_scheduling_config,
            leader_eligible,
        } = self;

        let config = config.clone();

        crate::startup::start_instance_inner(
            authority,
            listen_addr,
            external_addr,
            config,
            memory_limit,
            memory_check_frequency,
            domain_scheduling_config,
            leader_eligible,
            readers,
            reader_addr,
            valve,
            trigger,
        )
    }

    /// Start a local-only worker, and return a handle to it.
    pub fn start_local(self) -> impl Future<Output = Result<Handle, anyhow::Error>> {
        let store = Arc::new(LocalAuthorityStore::new());
        self.start_local_custom(Arc::new(Authority::from(LocalAuthority::new_with_store(
            store,
        ))))
    }

    /// Start a local-only worker using a custom authority, and return a handle to it.
    pub fn start_local_custom(
        self,
        authority: Arc<Authority>,
    ) -> impl Future<Output = Result<Handle, anyhow::Error>> {
        let fut = self.start(authority);
        async move {
            #[allow(unused_mut)]
            let mut wh = fut.await?;
            #[cfg(test)]
            wh.backend_ready().await;
            Ok(wh)
        }
    }
}