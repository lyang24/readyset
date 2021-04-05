use crate::controller::domain_handle::{DomainHandle, DomainShardHandle};
use crate::controller::migrate::materialization::Materializations;
use crate::controller::recipe::Schema;
use crate::controller::schema;
use crate::controller::{ControllerState, Migration, Recipe};
use crate::controller::{Worker, WorkerIdentifier};
use crate::coordination::{CoordinationMessage, CoordinationPayload, DomainDescriptor};
use crate::debug::info::{DomainKey, GraphInfo};
use crate::errors::{bad_request_err, internal_err, ReadySetResult};
use crate::{ReaderReplicationResult, ReaderReplicationSpec};
use dataflow::prelude::*;
use dataflow::{node, payload::ControlReplyPacket, prelude::Packet, DomainBuilder, DomainConfig};
use futures_util::stream::StreamExt;
use hyper::{self, Method, StatusCode};
use nom_sql::ColumnSpecification;
use noria::builders::*;
use noria::channel::tcp::{SendError, TcpSender};
use noria::consensus::{Authority, Epoch, STATE_KEY};
use noria::debug::stats::{DomainStats, GraphStats, NodeStats};
use noria::metrics::recorded;
use noria::{internal, invariant_eq, ActivationResult, ReadySetError};
use petgraph::visit::Bfs;
use slog::Logger;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::mem;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{cell, io, time};

/// `Controller` is the core component of the alternate Soup implementation.
///
/// It keeps track of the structure of the underlying data flow graph and its domains. `Controller`
/// does not allow direct manipulation of the graph. Instead, changes must be instigated through a
/// `Migration`, which can be performed using `ControllerInner::migrate`. Only one `Migration` can
/// occur at any given point in time.
pub(super) struct ControllerInner {
    pub(super) ingredients: Graph,
    pub(super) source: NodeIndex,
    pub(super) ndomains: usize,
    pub(super) sharding: Option<usize>,

    pub(super) domain_config: DomainConfig,

    /// Parameters for persistence code.
    pub(super) persistence: PersistenceParameters,
    pub(super) materializations: Materializations,

    /// Current recipe
    recipe: Recipe,

    pub(super) domains: HashMap<DomainIndex, DomainHandle>,
    pub(in crate::controller) domain_nodes: HashMap<DomainIndex, Vec<NodeIndex>>,
    pub(super) channel_coordinator: Arc<ChannelCoordinator>,

    /// Map from worker address to the address the worker is listening on for reads.
    read_addrs: HashMap<WorkerIdentifier, SocketAddr>,
    /// Map from wokrer address to the address the worker is listening on for
    /// external http requests.
    external_addrs: HashMap<WorkerIdentifier, SocketAddr>,
    pub(super) workers: HashMap<WorkerIdentifier, Worker>,

    /// State between migrations
    pub(super) remap: HashMap<DomainIndex, HashMap<NodeIndex, IndexPair>>,

    pub(super) epoch: Epoch,

    pending_recovery: Option<(Vec<String>, usize)>,

    quorum: usize,
    heartbeat_every: Duration,
    healthcheck_every: Duration,
    last_checked_workers: Instant,

    log: slog::Logger,

    pub(in crate::controller) replies: DomainReplies,
}

pub(in crate::controller) struct DomainReplies(
    tokio::sync::mpsc::UnboundedReceiver<ControlReplyPacket>,
);

impl DomainReplies {
    async fn read_n_domain_replies(&mut self, n: usize) -> Vec<ControlReplyPacket> {
        let crps: Vec<_> = (&mut self.0).take(n).collect().await;

        if crps.len() != n {
            unreachable!(
                "got unexpected EOF from domain reply channel after {} replies",
                crps.len()
            );
        }

        crps
    }

    pub(in crate::controller) async fn wait_for_acks(&mut self, d: &DomainHandle) {
        for r in self.read_n_domain_replies(d.shards()).await {
            match r {
                ControlReplyPacket::Ack(_) => {}
                r => unreachable!("got unexpected non-ack control reply: {:?}", r),
            }
        }
    }

    async fn wait_for_statistics(
        &mut self,
        d: &DomainHandle,
    ) -> Vec<(DomainStats, HashMap<NodeIndex, NodeStats>)> {
        let mut stats = Vec::with_capacity(d.shards());
        for r in self.read_n_domain_replies(d.shards()).await {
            match r {
                ControlReplyPacket::Statistics(d, s) => stats.push((d, s)),
                r => unreachable!("got unexpected non-stats control reply: {:?}", r),
            }
        }
        stats
    }
}

pub(super) fn graphviz(
    graph: &Graph,
    detailed: bool,
    materializations: &Materializations,
) -> String {
    let mut s = String::new();

    let indentln = |s: &mut String| s.push_str("    ");

    // header.
    s.push_str("digraph {{\n");

    // global formatting.
    indentln(&mut s);
    if detailed {
        s.push_str("node [shape=record, fontsize=10]\n");
    } else {
        s.push_str("graph [ fontsize=24 fontcolor=\"#0C6fA9\", outputorder=edgesfirst ]\n");
        s.push_str("edge [ color=\"#0C6fA9\", style=bold ]\n");
        s.push_str("node [ color=\"#0C6fA9\", shape=box, style=\"rounded,bold\" ]\n");
    }

    // node descriptions.
    for index in graph.node_indices() {
        let node = &graph[index];
        let materialization_status = materializations.get_status(index, node);
        indentln(&mut s);
        s.push_str(&format!("n{}", index.index()));
        s.push_str(&node.describe(index, detailed, materialization_status));
    }

    // edges.
    for (_, edge) in graph.raw_edges().iter().enumerate() {
        indentln(&mut s);
        s.push_str(&format!(
            "n{} -> n{} [ {} ]",
            edge.source().index(),
            edge.target().index(),
            if graph[edge.source()].is_egress() {
                "color=\"#CCCCCC\""
            } else if graph[edge.source()].is_source() {
                "style=invis"
            } else {
                ""
            }
        ));
        s.push_str("\n");
    }

    // footer.
    s.push_str("}}");

    s
}

impl ControllerInner {
    pub(in crate::controller) fn topo_order(&self, new: &HashSet<NodeIndex>) -> Vec<NodeIndex> {
        let mut topo_list = Vec::with_capacity(new.len());
        let mut topo = petgraph::visit::Topo::new(&self.ingredients);
        while let Some(node) = topo.next(&self.ingredients) {
            if node == self.source {
                continue;
            }
            if self.ingredients[node].is_dropped() {
                continue;
            }
            if !new.contains(&node) {
                continue;
            }
            topo_list.push(node);
        }
        topo_list
    }

    pub(super) fn external_request<A: Authority + 'static>(
        &mut self,
        method: hyper::Method,
        path: String,
        query: Option<String>,
        body: hyper::body::Bytes,
        authority: &Arc<A>,
    ) -> Result<Result<String, ReadySetError>, StatusCode> {
        // TODO(eta): the error handling / general serialization inside this function is really
        // confusing, and has been the source of at least 1 hard-to-track-down bug
        metrics::increment_counter!(recorded::SERVER_EXTERNAL_REQUESTS);
        use serde_json as json;

        match (&method, path.as_ref()) {
            (&Method::GET, "/simple_graph") => return Ok(Ok(self.graphviz(false))),
            (&Method::POST, "/simple_graphviz") => {
                return Ok(Ok(json::to_string(&self.graphviz(false)).unwrap()));
            }
            (&Method::GET, "/graph") => return Ok(Ok(self.graphviz(true))),
            (&Method::POST, "/graphviz") => {
                return Ok(Ok(json::to_string(&self.graphviz(true)).unwrap()));
            }
            (&Method::GET | &Method::POST, "/get_statistics") => {
                return Ok(Ok(json::to_string(&self.get_statistics()).unwrap()));
            }
            _ => {}
        }

        if self.pending_recovery.is_some() || self.workers.len() < self.quorum {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }

        match (method, path.as_ref()) {
            (Method::GET, "/flush_partial") => {
                Ok(Ok(json::to_string(&self.flush_partial()).unwrap()))
            }
            (Method::POST, "/inputs") => Ok(Ok(json::to_string(&self.inputs()).unwrap())),
            (Method::POST, "/outputs") => Ok(Ok(json::to_string(&self.outputs()).unwrap())),
            (Method::GET | Method::POST, "/instances") => {
                Ok(Ok(json::to_string(&self.get_instances()).unwrap()))
            }
            (Method::GET, "external_addrs") | (Method::POST, "/external_addrs") => {
                Ok(Ok(json::to_string(&self.external_addrs).unwrap()))
            }
            (Method::GET, "/nodes") => {
                // TODO(malte): this is a pretty yucky hack, but hyper doesn't provide easy access
                // to individual query variables unfortunately. We'll probably want to factor this
                // out into a helper method.
                let nodes = if let Some(query) = query {
                    let vars: Vec<_> = query.split('&').map(String::from).collect();
                    if let Some(n) = &vars.into_iter().find(|v| v.starts_with("w=")) {
                        self.nodes_on_worker(Some(&n[2..].parse().unwrap()))
                    } else {
                        self.nodes_on_worker(None)
                    }
                } else {
                    // all data-flow nodes
                    self.nodes_on_worker(None)
                };
                Ok(Ok(json::to_string(
                    &nodes
                        .into_iter()
                        .filter_map(|ni| {
                            let n = &self.ingredients[ni];
                            if n.is_internal() {
                                Some((ni, n.name(), n.description(true)))
                            } else if n.is_base() {
                                Some((ni, n.name(), "Base table".to_owned()))
                            } else if n.is_reader() {
                                Some((ni, n.name(), "Leaf view".to_owned()))
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>(),
                )
                .unwrap()))
            }
            (Method::POST, "/table_builder") => json::from_slice(&body)
                .map_err(|_| StatusCode::BAD_REQUEST)
                .map(|args| Ok(json::to_string(&self.table_builder(args)).unwrap())),
            (Method::POST, "/view_builder") => json::from_slice(&body)
                .map_err(|_| StatusCode::BAD_REQUEST)
                .map(|args| self.view_builder(args))
                .map(|view| Ok(json::to_string(&view).unwrap())),
            (Method::POST, "/extend_recipe") => json::from_slice(&body)
                .map_err(|_| StatusCode::BAD_REQUEST)
                .map(|args| {
                    self.extend_recipe(authority, args)
                        .map(|r| json::to_string(&r).unwrap())
                }),
            (Method::POST, "/install_recipe") => json::from_slice(&body)
                .map_err(|_| StatusCode::BAD_REQUEST)
                .map(|args| {
                    self.install_recipe(authority, args)
                        .map(|r| json::to_string(&r).unwrap())
                }),
            (Method::POST, "/set_security_config") => json::from_slice(&body)
                .map_err(|_| StatusCode::BAD_REQUEST)
                .map(|args| {
                    self.set_security_config(args)
                        .map(|r| json::to_string(&r).unwrap())
                }),
            (Method::POST, "/create_universe") => json::from_slice(&body)
                .map_err(|_| StatusCode::BAD_REQUEST)
                .map(|args| self.create_universe(args))
                .map(|universe| Ok(json::to_string(&universe).unwrap())),
            (Method::POST, "/replicate_readers") => json::from_slice(&body)
                .map_err(|_| StatusCode::BAD_REQUEST)
                .map(|args| {
                    self.replicate_readers(args)
                        .map(|readers| json::to_string(&readers).unwrap())
                }),
            (Method::POST, "/get_info") => Ok(Ok(json::to_string(&self.get_info()).unwrap())),
            (Method::POST, "/remove_node") => json::from_slice(&body)
                .map_err(|_| StatusCode::BAD_REQUEST)
                .map(|args| {
                    self.remove_nodes(vec![args].as_slice())
                        .map(|r| json::to_string(&r).unwrap())
                }),
            _ => Err(StatusCode::NOT_FOUND),
        }
    }

    pub(super) fn handle_register(&mut self, msg: CoordinationMessage) -> Result<(), io::Error> {
        let (remote, read_listen_addr, controller_addr) = if let CoordinationPayload::Register {
            addr: remote,
            read_listen_addr,
            controller_addr,
            ..
        } = msg.payload
        {
            (remote, read_listen_addr, controller_addr)
        } else {
            unreachable!();
        };

        info!(
            self.log,
            "new worker registered from {:?}, which listens on {:?}", msg.source, remote
        );

        let sender = TcpSender::connect(&remote)?;
        let ws = Worker::new(sender);
        self.workers.insert(msg.source, ws);
        self.read_addrs.insert(msg.source, read_listen_addr);
        self.external_addrs.insert(msg.source, controller_addr);

        if self.workers.len() >= self.quorum {
            if let Some((recipes, mut recipe_version)) = self.pending_recovery.take() {
                assert_eq!(self.workers.len(), self.quorum);
                assert_eq!(self.recipe.version(), 0);
                if recipes.len() > recipe_version + 1 {
                    // TODO(eta): this is a terrible stopgap hack
                    crit!(
                        self.log,
                        "{} recipes but recipe version is at {}",
                        recipes.len(),
                        recipe_version
                    );
                    recipe_version = recipes.len() + 1;
                }

                info!(self.log, "Restoring graph configuration");
                self.recipe = Recipe::with_version(
                    recipe_version + 1 - recipes.len(),
                    Some(self.log.clone()),
                );
                for r in recipes {
                    if let Err(e) = self.apply_recipe(self.recipe.clone().extend(&r).unwrap()) {
                        // TODO(eta): is this the best thing to do?
                        crit!(self.log, "Failed to restore recipe: {}", e);
                    }
                }
            }
        }

        Ok(())
    }

    fn check_worker_liveness(&mut self) -> ReadySetResult<()> {
        let mut any_failed = false;

        // check if there are any newly failed workers
        if self.last_checked_workers.elapsed() > self.healthcheck_every {
            for (_addr, ws) in self.workers.iter() {
                if ws.healthy && ws.last_heartbeat.elapsed() > self.heartbeat_every * 4 {
                    any_failed = true;
                }
            }
            self.last_checked_workers = Instant::now();
        }

        // if we have newly failed workers, iterate again to find all workers that have missed >= 3
        // heartbeats. This is necessary so that we correctly handle correlated failures of
        // workers.
        if any_failed {
            let mut failed = Vec::new();
            for (addr, ws) in self.workers.iter_mut() {
                if ws.healthy && ws.last_heartbeat.elapsed() > self.heartbeat_every * 3 {
                    error!(self.log, "worker at {:?} has failed!", addr);
                    ws.healthy = false;
                    failed.push(addr.clone());
                }
            }
            self.handle_failed_workers(failed)?;
        }
        Ok(())
    }

    fn handle_failed_workers(&mut self, failed: Vec<WorkerIdentifier>) -> ReadySetResult<()> {
        // first, translate from the affected workers to affected data-flow nodes
        let mut affected_nodes = Vec::new();
        for wi in failed {
            info!(self.log, "handling failure of worker {:?}", wi);
            affected_nodes.extend(self.get_failed_nodes(&wi));
        }

        // then, figure out which queries are affected (and thus must be removed and added again in
        // a migration)
        let affected_queries = self.recipe.queries_for_nodes(affected_nodes);
        let (recovery, mut original) = self.recipe.make_recovery(affected_queries);

        // activate recipe
        self.apply_recipe(recovery.clone())?;

        // we must do this *after* the migration, since the migration itself modifies the recipe in
        // `recovery`, and we currently need to clone it here.
        let tmp = self.recipe.clone();
        original.set_prior(tmp.clone());
        // somewhat awkward, but we must replace the stale `SqlIncorporator` state in `original`
        original.set_sql_inc(tmp.sql_inc().clone());

        // back to original recipe, which should add the query again
        self.apply_recipe(original)?;
        Ok(())
    }

    pub(super) fn handle_heartbeat(&mut self, msg: CoordinationMessage) -> ReadySetResult<()> {
        match self.workers.get_mut(&msg.source) {
            None => crit!(
                self.log,
                "got heartbeat for unknown worker {:?}",
                msg.source
            ),
            Some(ref mut ws) => {
                ws.last_heartbeat = Instant::now();
            }
        }

        self.check_worker_liveness()?;
        Ok(())
    }

    pub(super) fn get_info(&self) -> GraphInfo {
        let mut worker_info = HashMap::new();
        for (di, dh) in self.domains.iter() {
            for (i, shard) in dh.shards.iter().enumerate() {
                worker_info
                    .entry(shard.worker)
                    .or_insert_with(HashMap::new)
                    .entry(DomainKey(*di, i))
                    .or_insert_with(Vec::new)
                    .extend(self.domain_nodes[di].iter())
            }
        }
        GraphInfo {
            workers: worker_info,
        }
    }

    pub(super) fn replicate_readers(
        &mut self,
        spec: ReaderReplicationSpec,
    ) -> ReadySetResult<ReaderReplicationResult> {
        let mut reader_nodes = Vec::new();
        let worker_addr = spec.worker_addr;
        // If we've been specified to replicate readers into a specific worker,
        // we must then check that the worker is registered in the Controller.
        // Otherwise, we return a BAD_REQUEST error.
        if let Some(addr) = worker_addr {
            if !self.workers.contains_key(&addr) {
                let c = self
                    .workers
                    .keys()
                    .map(|wi| wi.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(bad_request_err(format!(
                    "The specified worker '{:?}' is not registered. Workers: {:?}",
                    &addr, c
                )));
            }
        }

        // We then proceed to retrieve the node indexes of each
        // query.
        let mut node_indexes = Vec::new();
        for query_name in &spec.queries {
            node_indexes.push((
                query_name,
                self.recipe
                    .node_addr_for(query_name)
                    .map_err(|e| {
                        warn!(self.log, "Reader replication failed: no node was found for query '{:?}'. Error: {:?}",
                        query_name, e);
                        bad_request_err(format!("Reader replication failed: no node was found for query '{:?}'", query_name))
                    })?,
            ));
        }

        // Now we look for the reader nodes of each of the query nodes.
        let mut new_readers = HashMap::new();
        for (query_name, node_index) in node_indexes {
            // The logic to find the reader nodes is the same as [`self::find_view_for(NodeIndex,&str)`],
            // but we perform some extra operations here.
            // TODO(Fran): In the future we should try to find a good abstraction to avoid duplicating the logic.
            let mut bfs = Bfs::new(&self.ingredients, node_index);
            while let Some(child_index) = bfs.next(&self.ingredients) {
                let child: &Node = &self.ingredients[child_index];
                if child
                    .with_reader(|r| r.is_for() == node_index)
                    .unwrap_or(false)
                    && child.name() == query_name
                {
                    // Now the child is the reader node of the query we are looking at.
                    // Here, we extract its [`BinaryOperator`] and use it to create a new
                    // mirror node.
                    let op = child.with_reader(|r| r.operator).unwrap();
                    let mut reader_node = self.ingredients[node_index].named_mirror(
                        node::special::Reader::new(node_index, op),
                        child.name().to_string(),
                    );
                    // We also take the associated keys of the original reader node.
                    let keys_opt = child.with_reader(|r| r.key()).unwrap_or(None);
                    if let Some(keys) = keys_opt {
                        // And set the keys to the replicated reader.
                        reader_node.with_reader_mut(|r| r.set_key(keys)).unwrap();
                    }
                    // We add the replicated reader to the graph.
                    let reader_index = self.ingredients.add_node(reader_node);
                    self.ingredients.add_edge(node_index, reader_index, ());
                    // We keep track of the replicated reader and query node indexes, so
                    // we can use them to run a migration.
                    reader_nodes.push((node_index, reader_index));
                    // We store the reader indexes by query, to use as a reply
                    // to the user.
                    new_readers
                        .entry(query_name)
                        .or_insert(Vec::new())
                        .push(reader_index);
                    break;
                }
            }
        }

        // We run a migration with the new reader nodes.
        // The migration will take care of creating the domains and
        // sending them to the specified worker (or distribute them along all
        // workers if no worker was specified).
        self.migrate(move |mig| {
            mig.worker = worker_addr;
            for (node_index, reader_index) in reader_nodes {
                mig.added.insert(reader_index);
                mig.readers.insert(node_index, reader_index);
            }
        })?;

        // We retrieve the domain of the replicated readers.
        let mut query_information = HashMap::new();
        for (query_name, reader_indexes) in new_readers {
            let mut domain_mappings = HashMap::new();
            for reader_index in reader_indexes {
                let reader = &self.ingredients[reader_index];
                domain_mappings
                    .entry(reader.domain())
                    .or_insert(Vec::new())
                    .push(reader_index)
            }
            query_information.insert(query_name.clone(), domain_mappings);
        }

        // We return information about which replicated readers got in which domain,
        // for which query.
        Ok(ReaderReplicationResult {
            new_readers: query_information,
        })
    }

    /// Construct `ControllerInner` with a specified listening interface
    pub(super) fn new(
        log: slog::Logger,
        state: ControllerState,
        drx: tokio::sync::mpsc::UnboundedReceiver<ControlReplyPacket>,
    ) -> Self {
        let mut g = petgraph::Graph::new();
        let source = g.add_node(node::Node::new(
            "source",
            &["because-type-inference"],
            node::special::Source,
        ));

        let mut materializations = Materializations::new(&log);
        if !state.config.partial_enabled {
            materializations.disable_partial()
        }
        materializations.set_frontier_strategy(state.config.frontier_strategy);

        let cc = Arc::new(ChannelCoordinator::new());
        assert_ne!(state.config.quorum, 0);

        let pending_recovery = if !state.recipes.is_empty() {
            Some((state.recipes, state.recipe_version))
        } else {
            None
        };

        let mut recipe = Recipe::blank(Some(log.clone()));
        recipe.enable_reuse(state.config.reuse);

        ControllerInner {
            ingredients: g,
            source,
            ndomains: 0,

            materializations,
            sharding: state.config.sharding,
            domain_config: state.config.domain_config,
            persistence: state.config.persistence,
            heartbeat_every: state.config.heartbeat_every,
            healthcheck_every: state.config.healthcheck_every,
            recipe,
            quorum: state.config.quorum,
            log,

            domains: Default::default(),
            domain_nodes: Default::default(),
            channel_coordinator: cc,
            epoch: state.epoch,

            remap: HashMap::default(),

            read_addrs: HashMap::default(),
            external_addrs: HashMap::default(),
            workers: HashMap::default(),

            pending_recovery,
            last_checked_workers: Instant::now(),

            replies: DomainReplies(drx),
        }
    }

    /// Controls the persistence mode, and parameters related to persistence.
    ///
    /// Three modes are available:
    ///
    ///  1. `DurabilityMode::Permanent`: all writes to base nodes should be written to disk.
    ///  2. `DurabilityMode::DeleteOnExit`: all writes are written to disk, but the log is
    ///     deleted once the `Controller` is dropped. Useful for tests.
    ///  3. `DurabilityMode::MemoryOnly`: no writes to disk, store all writes in memory.
    ///     Useful for baseline numbers.
    ///
    /// `queue_capacity` indicates the number of packets that should be buffered until
    /// flushing, and `flush_timeout` indicates the length of time to wait before flushing
    /// anyway.
    ///
    /// Must be called before any domains have been created.
    #[allow(unused)]
    fn with_persistence_options(&mut self, params: PersistenceParameters) {
        assert_eq!(self.ndomains, 0);
        self.persistence = params;
    }

    pub(in crate::controller) fn place_domain(
        &mut self,
        idx: DomainIndex,
        num_shards: Option<usize>,
        log: &Logger,
        nodes: Vec<(NodeIndex, bool)>,
        worker_id_opt: Option<WorkerIdentifier>,
    ) -> ReadySetResult<DomainHandle> {
        // TODO: can we just redirect all domain traffic through the worker's connection?
        let mut assignments = Vec::new();
        let mut nodes = Some(
            nodes
                .into_iter()
                .map(|(ni, _)| {
                    let node = self.ingredients.node_weight_mut(ni).unwrap().take();
                    node.finalize(&self.ingredients)
                })
                .map(|nd| (nd.local_addr(), cell::RefCell::new(nd)))
                .collect(),
        );

        // Use the specific worker that's been assigned. If that worker
        // is not valid, log a warning and fallback to using
        // all the available workers in a simple round-robin fashion.
        let selected_worker = match worker_id_opt {
            Some(worker_id)
                if self
                    .workers
                    .get(&worker_id)
                    .filter(|worker| worker.healthy)
                    .is_some() =>
            {
                worker_id_opt
            }
            Some(worker_id) => {
                warn!(
                    log,
                    "Attempted to make a migration with a worker node that it's \
                    either not present or not healthy. Will fallback to use all workers instead.\
                    Worker ID: {:?}",
                    worker_id
                );
                None
            }
            _ => None,
        };
        let worker_selector = |(worker_id, _): &(&WorkerIdentifier, &mut Worker)| {
            (selected_worker.is_none())
                || (selected_worker
                    .filter(|s_worker_id| **worker_id == *s_worker_id)
                    .is_some())
        };
        let mut wi = self.workers.iter_mut().filter(&worker_selector);

        // Send `AssignDomain` to each shard of the given domain
        for i in 0..num_shards.unwrap_or(1) {
            let nodes = if i == num_shards.unwrap_or(1) - 1 {
                nodes.take().unwrap()
            } else {
                nodes.clone().unwrap()
            };

            let domain = DomainBuilder {
                index: idx,
                shard: if num_shards.is_some() { Some(i) } else { None },
                nshards: num_shards.unwrap_or(1),
                config: self.domain_config.clone(),
                nodes,
                persistence_parameters: self.persistence.clone(),
            };

            let (identifier, w) = loop {
                if let Some((i, w)) = wi.next() {
                    if w.healthy {
                        break (*i, w);
                    }
                } else {
                    wi = self.workers.iter_mut().filter(&worker_selector);
                }
            };

            // send domain to worker
            info!(
                log,
                "sending domain {}.{} to worker {:?}",
                domain.index.index(),
                domain.shard.unwrap_or(0),
                w.sender.peer_addr()
            );
            let src = w.sender.local_addr().unwrap();
            w.sender.send(CoordinationMessage {
                epoch: self.epoch,
                source: src,
                payload: CoordinationPayload::AssignDomain(domain),
            })?;

            assignments.push(identifier);
        }

        // Wait for all the domains to acknowledge.
        let mut txs = HashMap::new();
        let mut announce = Vec::new();
        let fut = self.replies.read_n_domain_replies(num_shards.unwrap_or(1));
        let replies = futures_executor::block_on(fut);
        for r in replies {
            match r {
                ControlReplyPacket::Booted(shard, addr) => {
                    self.channel_coordinator.insert_remote((idx, shard), addr);
                    announce.push(DomainDescriptor::new(idx, shard, addr));
                    txs.insert(
                        shard,
                        self.channel_coordinator
                            .builder_for(&(idx, shard))
                            .ok_or_else(|| internal_err("could not get domain connection builder"))?
                            .build_sync()
                            .map_err(|e| {
                                internal_err(format!("failed to build domain sender: {:?}", e))
                            })?,
                    );
                }
                crp => {
                    internal!("got unexpected control reply packet: {:?}", crp);
                }
            }
        }

        // Tell all workers about the new domain(s)
        // TODO(jon): figure out how much of the below is still true
        // TODO(malte): this is a hack, and not an especially neat one. In response to a
        // domain boot message, we broadcast information about this new domain to all
        // workers, which inform their ChannelCoordinators about it. This is required so
        // that domains can find each other when starting up.
        // Moreover, it is required for us to do this *here*, since this code runs on
        // the thread that initiated the migration, and which will query domains to ask
        // if they're ready. No domain will be ready until it has found its neighbours,
        // so by sending out the information here, we ensure that we cannot deadlock
        // with the migration waiting for a domain to become ready when trying to send
        // the information. (We used to do this in the controller thread, with the
        // result of a nasty deadlock.)
        for (address, endpoint) in self.workers.iter_mut() {
            for &dd in &announce {
                if let Err(e) = endpoint.sender.send(CoordinationMessage {
                    epoch: self.epoch,
                    source: endpoint.sender.local_addr().unwrap(),
                    payload: CoordinationPayload::DomainBooted(dd),
                }) {
                    // TODO(Fran): We need better error handling for workers
                    //   that failed before the controller noticed.
                    error!(
                        log,
                        "Worker could not be reached and will be ignored. Address: {:?} | Error: {:?}",
                        address.to_string(),
                        e
                    );
                }
            }
        }

        let shards = assignments
            .into_iter()
            .enumerate()
            .map(|(i, worker)| {
                let tx = txs.remove(&i).unwrap();
                DomainShardHandle { worker, tx }
            })
            .collect();

        Ok(DomainHandle {
            idx,
            shards,
            log: log.clone(),
        })
    }

    /// Set the `Logger` to use for internal log messages.
    ///
    /// By default, all log messages are discarded.
    #[allow(unused)]
    fn log_with(&mut self, log: slog::Logger) {
        self.log = log;
        self.materializations.set_logger(&self.log);
    }

    /// Adds a new user universe.
    /// User universes automatically enforce security policies.
    fn add_universe<F, T>(
        &mut self,
        context: HashMap<String, DataType>,
        f: F,
    ) -> Result<T, ReadySetError>
    where
        F: FnOnce(&mut Migration) -> ReadySetResult<T>,
    {
        info!(self.log, "starting migration: new soup universe");
        let miglog = self.log.new(o!());
        let mut m = Migration {
            mainline: self,
            added: Default::default(),
            columns: Default::default(),
            readers: Default::default(),
            worker: None,
            context,
            start: time::Instant::now(),
            log: miglog,
        };
        let r = f(&mut m)?;
        m.commit()?;
        Ok(r)
    }

    /// Perform a new query schema migration.
    // crate viz for tests
    pub(crate) fn migrate<F, T>(&mut self, f: F) -> Result<T, ReadySetError>
    where
        F: FnOnce(&mut Migration) -> T,
    {
        info!(self.log, "starting migration");
        let miglog = self.log.new(o!());
        let mut m = Migration {
            mainline: self,
            added: Default::default(),
            columns: Default::default(),
            readers: Default::default(),
            context: Default::default(),
            worker: None,
            start: time::Instant::now(),
            log: miglog,
        };
        let r = f(&mut m);
        m.commit()?;
        Ok(r)
    }

    #[cfg(test)]
    pub(crate) fn graph(&self) -> &Graph {
        &self.ingredients
    }

    /// Get a Vec of all known input nodes.
    ///
    /// Input nodes are here all nodes of type `Table`. The addresses returned by this function will
    /// all have been returned as a key in the map from `commit` at some point in the past.
    fn inputs(&self) -> BTreeMap<String, NodeIndex> {
        self.ingredients
            .neighbors_directed(self.source, petgraph::EdgeDirection::Outgoing)
            .map(|n| {
                let base = &self.ingredients[n];
                assert!(base.is_base());
                (base.name().to_owned(), n)
            })
            .collect()
    }

    /// Get a Vec of all known output nodes.
    ///
    /// Output nodes here refers to nodes of type `Reader`, which is the nodes created in response
    /// to calling `.maintain` or `.stream` for a node during a migration.
    fn outputs(&self) -> BTreeMap<String, NodeIndex> {
        self.ingredients
            .externals(petgraph::EdgeDirection::Outgoing)
            .filter_map(|n| {
                let name = self.ingredients[n].name().to_owned();
                self.ingredients[n]
                    .with_reader(|r| {
                        // we want to give the the node address that is being materialized not that of
                        // the reader node itself.
                        (name, r.is_for())
                    })
                    .ok()
            })
            .collect()
    }

    fn find_view_for(&self, node: NodeIndex, name: &str) -> Option<NodeIndex> {
        // reader should be a child of the given node. however, due to sharding, it may not be an
        // *immediate* child. furthermore, once we go beyond depth 1, we may accidentally hit an
        // *unrelated* reader node. to account for this, readers keep track of what node they are
        // "for", and we simply search for the appropriate reader by that metric. since we know
        // that the reader must be relatively close, a BFS search is the way to go.
        let mut bfs = Bfs::new(&self.ingredients, node);
        while let Some(child) = bfs.next(&self.ingredients) {
            if self.ingredients[child]
                .with_reader(|r| r.is_for() == node)
                .unwrap_or(false)
                && self.ingredients[child].name() == name
            {
                return Some(child);
            }
        }
        None
    }

    /// Obtain a `ViewBuilder` that can be sent to a client and then used to query a given
    /// (already maintained) reader node called `name`.
    fn view_builder(&self, name: &str) -> Result<Option<ViewBuilder>, ReadySetError> {
        // first try to resolve the node via the recipe, which handles aliasing between identical
        // queries.
        let node = match self.recipe.node_addr_for(name) {
            Ok(ni) => ni,
            Err(_) => {
                // if the recipe doesn't know about this query, traverse the graph.
                // we need this do deal with manually constructed graphs (e.g., in tests).
                if let Some(res) = self.outputs().get(name) {
                    *res
                } else {
                    return Ok(None);
                }
            }
        };

        let name = match self.recipe.resolve_alias(name) {
            None => name,
            Some(alias) => alias,
        };
        if let Some(r) = self.find_view_for(node, name) {
            let domain = self.ingredients[r].domain();
            let columns = self.ingredients[r].fields().to_vec();
            let schema = self.view_schema(r)?;
            let shards = (0..self.domains[&domain].shards())
                .map(|i| self.read_addrs[&self.domains[&domain].assignment(i)])
                .collect();

            Ok(Some(ViewBuilder {
                node: r,
                columns,
                schema,
                shards,
            }))
        } else {
            Ok(None)
        }
    }

    fn view_schema(
        &self,
        view_ni: NodeIndex,
    ) -> Result<Option<Vec<ColumnSpecification>>, ReadySetError> {
        let n = &self.ingredients[view_ni];
        let schema: Vec<_> = (0..n.fields().len())
            .map(|i| {
                Ok(schema::column_schema(
                    &self.ingredients,
                    view_ni,
                    &self.recipe,
                    i,
                    &self.log,
                )?)
            })
            .collect::<Result<Vec<_>, ReadySetError>>()?;

        if schema.iter().any(Option::is_none) {
            Ok(None)
        } else {
            Ok(Some(schema.into_iter().map(Option::unwrap).collect()))
        }
    }

    /// Obtain a TableBuilder that can be used to construct a Table to perform writes and deletes
    /// from the given named base node.
    fn table_builder(&self, base: &str) -> ReadySetResult<Option<TableBuilder>> {
        let ni = match self.recipe.node_addr_for(base) {
            Ok(ni) => ni,
            Err(_) => *self
                .inputs()
                .get(base)
                .ok_or_else(|| ReadySetError::TableNotFound(base.into()))?,
        };
        let node = &self.ingredients[ni];

        trace!(self.log, "creating table"; "for" => base);

        let mut key = self.ingredients[ni]
            .suggest_indexes(ni)
            .remove(&ni)
            .map(|index| index.columns)
            .unwrap_or_else(Vec::new);
        let mut is_primary = false;
        if key.is_empty() {
            if let Sharding::ByColumn(col, _) = self.ingredients[ni].sharded_by() {
                key = vec![col];
            }
        } else {
            is_primary = true;
        }

        let txs = (0..self.domains[&node.domain()].shards())
            .map(|i| {
                self.channel_coordinator
                    .get_addr(&(node.domain(), i))
                    .ok_or_else(|| {
                        internal_err(format!(
                            "failed to get channel coordinator for {}.{}",
                            node.domain().index(),
                            i
                        ))
                    })
            })
            .collect::<ReadySetResult<Vec<_>>>()?;

        let base_operator = node
            .get_base()
            .ok_or_else(|| internal_err("asked to get table for non-base node"))?;
        let columns: Vec<String> = node
            .fields()
            .iter()
            .enumerate()
            .filter(|&(n, _)| !base_operator.get_dropped().contains_key(n))
            .map(|(_, s)| s.clone())
            .collect();
        invariant_eq!(
            columns.len(),
            node.fields().len() - base_operator.get_dropped().len()
        );
        let schema = self
            .recipe
            .schema_for(base)
            .map(|s| -> ReadySetResult<_> {
                match s {
                    Schema::Table(s) => Ok(s),
                    _ => internal!("non-base schema {:?} returned for table '{}'", s, base),
                }
            })
            .transpose()?;

        Ok(Some(TableBuilder {
            txs,
            ni: node.global_addr(),
            addr: node.local_addr(),
            key,
            key_is_primary: is_primary,
            dropped: base_operator.get_dropped(),
            table_name: node.name().to_owned(),
            columns,
            schema,
        }))
    }

    /// Get statistics about the time spent processing different parts of the graph.
    fn get_statistics(&mut self) -> GraphStats {
        trace!(self.log, "asked to get statistics");
        let log = &self.log;
        let workers = &self.workers;
        let replies = &mut self.replies;
        // TODO: request stats from domains in parallel.
        let domains = self
            .domains
            .iter_mut()
            .flat_map(|(&di, s)| {
                trace!(log, "requesting stats from domain"; "di" => di.index());
                s.send_to_healthy(Box::new(Packet::GetStatistics), workers)
                    .unwrap();
                futures_executor::block_on(replies.wait_for_statistics(&s))
                    .into_iter()
                    .enumerate()
                    .map(move |(i, s)| ((di, i), s))
            })
            .collect();

        GraphStats { domains }
    }

    fn get_instances(&self) -> Vec<(WorkerIdentifier, bool, Duration)> {
        self.workers
            .iter()
            .map(|(&id, ref status)| (id, status.healthy, status.last_heartbeat.elapsed()))
            .collect()
    }

    fn flush_partial(&mut self) -> u64 {
        // get statistics for current domain sizes
        // and evict all state from partial nodes
        let workers = &self.workers;
        let replies = &mut self.replies;
        let to_evict: Vec<_> = self
            .domains
            .iter_mut()
            .map(|(di, s)| {
                s.send_to_healthy(Box::new(Packet::GetStatistics), workers)
                    .unwrap();
                let to_evict: Vec<(NodeIndex, u64)> =
                    futures_executor::block_on(replies.wait_for_statistics(&s))
                        .into_iter()
                        .flat_map(move |(_, node_stats)| {
                            node_stats
                                .into_iter()
                                .filter_map(|(ni, ns)| match ns.materialized {
                                    MaterializationStatus::Partial { .. } => {
                                        Some((ni, ns.mem_size))
                                    }
                                    _ => None,
                                })
                        })
                        .collect();
                (*di, to_evict)
            })
            .collect();

        let mut total_evicted = 0;
        for (di, nodes) in to_evict {
            for (ni, bytes) in nodes {
                let na = self.ingredients[ni].local_addr();
                self.domains
                    .get_mut(&di)
                    .unwrap()
                    .send_to_healthy(
                        Box::new(Packet::Evict {
                            node: Some(na),
                            num_bytes: bytes as usize,
                        }),
                        workers,
                    )
                    .expect("failed to send domain flush message");
                total_evicted += bytes;
            }
        }

        warn!(
            self.log,
            "flushed {} bytes of partial domain state", total_evicted
        );

        total_evicted
    }

    pub(super) fn create_universe(
        &mut self,
        context: HashMap<String, DataType>,
    ) -> Result<(), ReadySetError> {
        let log = self.log.clone();
        let mut r = self.recipe.clone();
        let groups = self.recipe.security_groups();

        let mut universe_groups = HashMap::new();

        let uid = context
            .get("id")
            .ok_or_else(|| bad_request_err("Universe context must have id"))?
            .clone();
        let uid = &[uid];
        if context.get("group").is_none() {
            let x = Arc::new(Mutex::new(HashMap::new()));
            for g in groups {
                // TODO: this should use external APIs through noria::ControllerHandle
                // TODO: can this move to the client entirely?
                let rgb: Option<ViewBuilder> = self.view_builder(&g)?;
                // TODO: using block_on here _only_ works because View::lookup just waits on a
                // channel, which doesn't use anything except the pure executor
                let mut view = rgb.map(|rgb| rgb.build(x.clone())).unwrap();
                let my_groups: Vec<DataType> = futures_executor::block_on(view.lookup(uid, true))
                    .unwrap()
                    .iter()
                    .map(|v| v[1].clone())
                    .collect();
                universe_groups.insert(g, my_groups);
            }
        }

        self.add_universe(context.clone(), |mut mig| {
            r.next();
            let ar = r.create_universe(&mut mig, universe_groups)?;
            info!(log, "{} expressions added", ar.expressions_added);
            info!(log, "{} expressions removed", ar.expressions_removed);
            Ok(())
        })?;

        self.recipe = r;
        Ok(())
    }

    fn set_security_config(&mut self, p: String) -> Result<(), ReadySetError> {
        self.recipe.set_security_config(&p);
        Ok(())
    }

    fn apply_recipe(&mut self, mut new: Recipe) -> Result<ActivationResult, ReadySetError> {
        // TODO(eta): if this fails, apply the old one?
        let r = self.migrate(|mig| new.activate(mig))?;

        match r {
            Ok(ref ra) => {
                let (removed_bases, removed_other): (Vec<_>, Vec<_>) = ra
                    .removed_leaves
                    .iter()
                    .cloned()
                    .partition(|ni| self.ingredients[*ni].is_base());

                // first remove query nodes in reverse topological order
                let mut topo_removals = Vec::with_capacity(removed_other.len());
                let mut topo = petgraph::visit::Topo::new(&self.ingredients);
                while let Some(node) = topo.next(&self.ingredients) {
                    if removed_other.contains(&node) {
                        topo_removals.push(node);
                    }
                }
                topo_removals.reverse();

                for leaf in topo_removals {
                    self.remove_leaf(leaf)?;
                }

                // now remove bases
                for base in removed_bases {
                    // TODO(malte): support removing bases that still have children?
                    let children: Vec<NodeIndex> = self
                        .ingredients
                        .neighbors_directed(base, petgraph::EdgeDirection::Outgoing)
                        .collect();
                    // TODO(malte): what about domain crossings? can ingress/egress nodes be left
                    // behind?
                    assert_eq!(children.len(), 0);
                    debug!(
                        self.log,
                        "Removing base \"{}\"",
                        self.ingredients[base].name();
                        "node" => base.index(),
                    );
                    // now drop the (orphaned) base
                    self.remove_nodes(vec![base].as_slice())?;
                }

                self.recipe = new;
            }
            Err(ref e) => {
                crit!(self.log, "failed to apply recipe: {}", e);
                // TODO(malte): a little yucky, since we don't really need the blank recipe
                let recipe = mem::replace(&mut self.recipe, Recipe::blank(None));
                self.recipe = recipe.revert();
            }
        }

        r
    }

    fn extend_recipe<A: Authority + 'static>(
        &mut self,
        authority: &Arc<A>,
        add_txt: String,
    ) -> Result<ActivationResult, ReadySetError> {
        let old = self.recipe.clone();
        // needed because self.apply_recipe needs to mutate self.recipe, so can't have it borrowed
        let new = mem::replace(&mut self.recipe, Recipe::blank(None));
        match new.extend(&add_txt) {
            Ok(new) => match self.apply_recipe(new) {
                Ok(x) => {
                    if authority
                        .read_modify_write(
                            STATE_KEY,
                            |state: Option<ControllerState>| match state {
                                None => unreachable!(),
                                Some(ref state) if state.epoch > self.epoch => Err(()),
                                Some(mut state) => {
                                    state.recipe_version = self.recipe.version();
                                    state.recipes.push(add_txt.clone());
                                    Ok(state)
                                }
                            },
                        )
                        .is_err()
                    {
                        noria::internal!("failed to persist recipe extension");
                    }
                    Ok(x)
                }
                Err(e) => {
                    self.recipe = old;
                    Err(e)
                }
            },
            Err((old, e)) => {
                // need to restore the old recipe
                crit!(self.log, "failed to extend recipe: {:?}", e);
                self.recipe = old;
                noria::internal!("failed to extend recipe: {:?}", e);
            }
        }
    }

    fn install_recipe<A: Authority + 'static>(
        &mut self,
        authority: &Arc<A>,
        r_txt: String,
    ) -> Result<ActivationResult, ReadySetError> {
        match Recipe::from_str(&r_txt, Some(self.log.clone())) {
            Ok(r) => {
                let _old = self.recipe.clone();
                let old = mem::replace(&mut self.recipe, Recipe::blank(None));
                let new = old.replace(r).unwrap();
                match self.apply_recipe(new) {
                    Ok(x) => {
                        if authority
                            .read_modify_write(STATE_KEY, |state: Option<ControllerState>| {
                                match state {
                                    None => unreachable!(),
                                    Some(ref state) if state.epoch > self.epoch => Err(()),
                                    Some(mut state) => {
                                        state.recipe_version = self.recipe.version();
                                        state.recipes = vec![r_txt.clone()];
                                        Ok(state)
                                    }
                                }
                            })
                            .is_err()
                        {
                            noria::internal!("failed to persist recipe installation")
                        }
                        Ok(x)
                    }
                    Err(e) => {
                        self.recipe = _old;
                        Err(e)
                    }
                }
            }
            Err(e) => {
                crit!(self.log, "failed to parse recipe: {:?}", e);
                noria::internal!("failed to parse recipe: {:?}", e);
            }
        }
    }

    fn graphviz(&self, detailed: bool) -> String {
        graphviz(&self.ingredients, detailed, &self.materializations)
    }

    fn remove_leaf(&mut self, mut leaf: NodeIndex) -> Result<(), ReadySetError> {
        let mut removals = vec![];
        let start = leaf;
        assert!(!self.ingredients[leaf].is_source());

        info!(
            self.log,
            "Computing removals for removing node {}",
            leaf.index()
        );

        let nchildren = self
            .ingredients
            .neighbors_directed(leaf, petgraph::EdgeDirection::Outgoing)
            .count();
        if nchildren > 0 {
            // This query leaf node has children -- typically, these are readers, but they can also
            // include egress nodes or other, dependent queries. We need to find the actual reader,
            // and remove that.
            if nchildren != 1 {
                crit!(
                    self.log,
                    "cannot remove node {}, as it still has multiple children",
                    leaf.index()
                );
                unreachable!();
            }

            let mut readers = Vec::new();
            let mut bfs = Bfs::new(&self.ingredients, leaf);
            while let Some(child) = bfs.next(&self.ingredients) {
                let n = &self.ingredients[child];
                if n.with_reader(|r| r.is_for() == leaf) == Ok(true) {
                    readers.push(child);
                }
            }

            // nodes can have only one reader attached
            assert_eq!(readers.len(), 1);
            let reader = readers[0];
            debug!(
                self.log,
                "Removing query leaf \"{}\"", self.ingredients[leaf].name();
                "node" => leaf.index(),
                "really" => reader.index(),
            );
            removals.push(reader);
            leaf = reader;
        }

        // `node` now does not have any children any more
        assert_eq!(
            self.ingredients
                .neighbors_directed(leaf, petgraph::EdgeDirection::Outgoing)
                .count(),
            0
        );

        let mut nodes = vec![leaf];
        while let Some(node) = nodes.pop() {
            let mut parents = self
                .ingredients
                .neighbors_directed(node, petgraph::EdgeDirection::Incoming)
                .detach();
            while let Some(parent) = parents.next_node(&self.ingredients) {
                let edge = self.ingredients.find_edge(parent, node).unwrap();
                self.ingredients.remove_edge(edge);

                if !self.ingredients[parent].is_source()
                    && !self.ingredients[parent].is_base()
                    // ok to remove original start leaf
                    && (parent == start || !self.recipe.sql_inc().is_leaf_address(parent))
                    && self
                    .ingredients
                    .neighbors_directed(parent, petgraph::EdgeDirection::Outgoing)
                    .count() == 0
                {
                    nodes.push(parent);
                }
            }

            removals.push(node);
        }

        self.remove_nodes(removals.as_slice())
    }

    fn remove_nodes(&mut self, removals: &[NodeIndex]) -> Result<(), ReadySetError> {
        // Remove node from controller local state
        let mut domain_removals: HashMap<DomainIndex, Vec<LocalNodeIndex>> = HashMap::default();
        for ni in removals {
            self.ingredients[*ni].remove();
            debug!(self.log, "Removed node {}", ni.index());
            domain_removals
                .entry(self.ingredients[*ni].domain())
                .or_insert_with(Vec::new)
                .push(self.ingredients[*ni].local_addr())
        }

        // Send messages to domains
        for (domain, nodes) in domain_removals {
            trace!(
                self.log,
                "Notifying domain {} of node removals",
                domain.index(),
            );

            match self
                .domains
                .get_mut(&domain)
                .unwrap()
                .send_to_healthy(Box::new(Packet::RemoveNodes { nodes }), &self.workers)
            {
                Ok(_) => (),
                Err(e) => match e {
                    SendError::IoError(ref ioe) => {
                        if ioe.kind() == io::ErrorKind::BrokenPipe
                            && ioe.get_ref().unwrap().to_string() == "worker failed"
                        {
                            // message would have gone to a failed worker, so ignore error
                        } else {
                            internal!("failed to remove nodes: {:?}", e);
                        }
                    }
                    _ => {
                        internal!("failed to remove nodes: {:?}", e);
                    }
                },
            }
        }

        Ok(())
    }

    fn get_failed_nodes(&self, lost_worker: &WorkerIdentifier) -> Vec<NodeIndex> {
        // Find nodes directly impacted by worker failure.
        let mut nodes: Vec<NodeIndex> = self.nodes_on_worker(Some(lost_worker));

        // Add any other downstream nodes.
        let mut failed_nodes = Vec::new();
        while let Some(node) = nodes.pop() {
            failed_nodes.push(node);
            for child in self
                .ingredients
                .neighbors_directed(node, petgraph::EdgeDirection::Outgoing)
            {
                if !nodes.contains(&child) {
                    nodes.push(child);
                }
            }
        }
        failed_nodes
    }

    /// List data-flow nodes, on a specific worker if `worker` specified.
    fn nodes_on_worker(&self, worker: Option<&WorkerIdentifier>) -> Vec<NodeIndex> {
        // NOTE(malte): this traverses all graph vertices in order to find those assigned to a
        // domain. We do this to avoid keeping separate state that may get out of sync, but it
        // could become a performance bottleneck in the future (e.g., when recovering large
        // graphs).
        let domain_nodes = |i: DomainIndex| -> Vec<NodeIndex> {
            self.ingredients
                .node_indices()
                .filter(|&ni| ni != self.source)
                .filter(|&ni| !self.ingredients[ni].is_dropped())
                .filter(|&ni| self.ingredients[ni].domain() == i)
                .collect()
        };

        if worker.is_some() {
            self.domains
                .values()
                .filter(|dh| dh.assigned_to_worker(worker.unwrap()))
                .fold(Vec::new(), |mut acc, dh| {
                    acc.extend(domain_nodes(dh.index()));
                    acc
                })
        } else {
            self.domains.values().fold(Vec::new(), |mut acc, dh| {
                acc.extend(domain_nodes(dh.index()));
                acc
            })
        }
    }
}

impl Drop for ControllerInner {
    fn drop(&mut self) {
        for d in self.domains.values_mut() {
            // XXX: this is a terrible ugly hack to ensure that all workers exit
            for _ in 0..100 {
                // don't unwrap, because given domain may already have terminated
                drop(d.send_to_healthy(Box::new(Packet::Quit), &self.workers));
            }
        }
    }
}
