use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    num::NonZeroUsize,
    time::Duration,
};

use iroh_bytes::{Hash, HashAndFormat};
use iroh_net::NodeId;
use iroh_sync::NamespaceId;
use tracing::debug;

use super::{FailureAction, IDLE_PEER_TIMEOUT, INITIAL_RETRY_COUNT};

use self::util::{IdGenerator, IndexSet};

mod util;

/// Concurrency limits for the [`Downloader`].
#[derive(Debug)]
pub struct ConcurrencyLimits {
    /// Maximum number of requests the service performs concurrently.
    pub max_concurrent_requests: usize,
    /// Maximum number of requests performed by a single node concurrently.
    pub max_concurrent_requests_per_node: usize,
    /// Maximum number of open connections the service maintains.
    pub max_open_connections: usize,
}

impl Default for ConcurrencyLimits {
    fn default() -> Self {
        // these numbers should be checked against a running node and might depend on platform
        ConcurrencyLimits {
            max_concurrent_requests: 50,
            max_concurrent_requests_per_node: 4,
            max_open_connections: 25,
        }
    }
}

impl ConcurrencyLimits {
    /// Checks if the maximum number of concurrent requests has been reached.
    pub fn at_requests_capacity(&self, active_requests: usize) -> bool {
        active_requests >= self.max_concurrent_requests
    }

    pub fn node_remaining_requests(&self, active_requests: usize) -> Option<NonZeroUsize> {
        NonZeroUsize::new(
            self.max_concurrent_requests_per_node
                .saturating_sub(active_requests),
        )
    }

    /// Checks if the maximum number of concurrent requests per node has been reached.
    pub fn node_at_request_capacity(&self, active_node_requests: usize) -> bool {
        active_node_requests >= self.max_concurrent_requests_per_node
    }

    /// Checks if the maximum number of connections has been reached.
    pub fn at_connections_capacity(&self, active_connections: usize) -> bool {
        active_connections >= self.max_open_connections
    }

    pub fn remaining_connections(&self, active_connections: usize) -> Option<NonZeroUsize> {
        NonZeroUsize::new(self.max_open_connections.saturating_sub(active_connections))
    }
}

/// Info on what to find on a node
#[derive(Debug, Default)]
pub struct NodeHints {
    /// Resources that can be found at this node
    pub resources: Vec<Resource>,
    /// Groups that this node belongs to
    pub groups: Vec<Group>,
}
impl NodeHints {
    /// Create with a single group
    pub fn with_group(group: Group) -> Self {
        Self {
            groups: vec![group],
            ..Default::default()
        }
    }
    /// Create with a single resource
    pub fn with_resource(resource: Resource) -> Self {
        Self {
            resources: vec![resource],
            ..Default::default()
        }
    }
}

/// Info on where to get a resource
#[derive(Debug, Default)]
pub struct ResourceHints {
    /// Nodes where we think the content is available.
    pub nodes: Vec<NodeId>,
    /// Nodes where we think the content is not available.
    pub skip_nodes: Vec<NodeId>,
    /// Node group where the content is likely available.
    pub groups: Vec<Group>,
}

impl ResourceHints {
    /// Add a provider node for this resource.
    pub fn add_node(mut self, node: NodeId) -> Self {
        self.nodes.push(node);
        self
    }
    /// Add a node to skip for this resource.
    pub fn skip_node(mut self, node: NodeId) -> Self {
        self.skip_nodes.push(node);
        self
    }
    /// Add a group where to look for this resource.
    pub fn add_group(mut self, group: Group) -> Self {
        self.groups.push(group);
        self
    }

    /// Create a [`ProviderHints`] with a single node.
    pub fn with_node(self, node: NodeId) -> Self {
        Self {
            nodes: vec![node],
            ..Default::default()
        }
    }

    /// Create a [`ProviderHints`] with a single group.
    pub fn with_group(group: Group) -> Self {
        Self {
            groups: vec![group],
            ..Default::default()
        }
    }
}

/// A resource to be downloaded
#[derive(Eq, PartialEq, Hash, Clone, Copy, PartialOrd, Ord)]
pub struct Resource {
    /// Hash
    pub hash: Hash,
    /// Kind
    pub kind: ResourceKind,
}

impl fmt::Debug for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}({})", self.kind, &self.hash.to_hex()[..8])
    }
}

impl From<Hash> for Resource {
    fn from(hash: Hash) -> Self {
        Self {
            hash,
            kind: ResourceKind::Blob,
        }
    }
}

impl Resource {
    /// New blob resource
    pub fn blob(hash: Hash) -> Self {
        Self {
            hash,
            kind: ResourceKind::Blob,
        }
    }
    /// New HashSeq resource
    pub fn hash_seq(hash: Hash) -> Self {
        Self {
            hash,
            kind: ResourceKind::HashSeq,
        }
    }

    /// Convert to [`HashAndformat`].
    pub fn hash_and_format(&self) -> HashAndFormat {
        match self.kind {
            ResourceKind::Blob => HashAndFormat::raw(self.hash),
            ResourceKind::HashSeq => HashAndFormat::hash_seq(self.hash),
        }
    }
}

/// Resource kind
#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy, PartialOrd, Ord)]
pub enum ResourceKind {
    /// A raw blob
    Blob,
    /// A collection of blobs
    HashSeq,
}

/// A group of hashes
#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy, PartialOrd, Ord)]
pub enum Group {
    /// Document group
    Doc(NamespaceId),
}

/// Downloader state
#[derive(Debug, Default)]
pub struct State {
    groups: BTreeMap<Group, GroupState>,
    resources: BTreeMap<Resource, ResourceState>,
    nodes: BTreeMap<NodeId, NodeInfo>,

    limits: ConcurrencyLimits,

    active_transfers: BTreeMap<TransferId, Transfer>,
    transfer_id: IdGenerator<TransferId>,

    actions: Vec<OutEvent>,
}

#[derive(Debug, Default)]
pub struct NodeInfo {
    groups: IndexSet<Group>,
    resources: IndexSet<Resource>,

    active_transfers: HashSet<TransferId>,

    state: NodeState,
    in_disconnect_timeout: bool,
}

#[derive(Debug)]
pub enum NodeState {
    Disconnected {
        failed: bool,
    },
    Pending {
        state: PendingState,
        remaining_retries: u8,
    },
    Connected,
}

impl Default for NodeState {
    fn default() -> Self {
        Self::Disconnected { failed: false }
    }
}

impl NodeState {
    fn is_active(&self) -> bool {
        matches!(
            self,
            NodeState::Connected { .. }
                | NodeState::Pending {
                    state: PendingState::Connecting,
                    ..
                }
        )
    }

    fn may_connect(&self) -> bool {
        matches!(self, NodeState::Disconnected { failed: false })
    }

    fn connecting() -> Self {
        Self::Pending {
            state: PendingState::Connecting,
            remaining_retries: INITIAL_RETRY_COUNT,
        }
    }
}

#[derive(Debug)]
pub enum PendingState {
    Connecting,
    RetryTimeout,
}

impl NodeInfo {
    fn remaining_retries(&self) -> u8 {
        match self.state {
            NodeState::Pending {
                remaining_retries, ..
            } => remaining_retries - 1,
            _ => INITIAL_RETRY_COUNT,
        }
    }
    fn should_reconnect(&self) -> bool {
        self.remaining_retries() > 0
    }

    fn connect(&mut self, self_id: NodeId) -> OutEvent {
        self.state = NodeState::connecting();
        OutEvent::StartDial(self_id)
    }

    fn is_connected(&self) -> bool {
        matches!(self.state, NodeState::Connected { .. })
    }
}

#[derive(Debug, Default)]
pub struct ResourceState {
    groups: IndexSet<Group>,
    nodes: IndexSet<NodeId>,

    skip_nodes: HashSet<NodeId>,
    active_transfer: Option<TransferId>,
}

impl ResourceState {
    fn is_transfering(&self) -> bool {
        self.active_transfer.is_some()
    }

    fn can_start_transfer(&self, node: &NodeId) -> bool {
        !self.is_transfering() && !self.skip_nodes.contains(node)
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, derive_more::From, Hash)]
pub struct TransferId(u64);

#[derive(Debug, Default)]
pub struct GroupState {
    resources: IndexSet<Resource>,
    nodes: IndexSet<NodeId>,
}

#[derive(Clone)]
pub struct Transfer {
    pub id: TransferId,
    pub resource: Resource,
    pub node: NodeId,
}

impl fmt::Debug for Transfer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Transfer(id:{} node:{} {:?}",
            self.id.0,
            self.node.fmt_short(),
            self.resource,
        )
    }
}

#[must_use = "OutEvents must be handled"]
#[derive(Debug)]
pub enum OutEvent {
    StartTransfer(Transfer),
    StartDial(NodeId),
    RegisterTimer(Duration, Timer),
    DropConnection(NodeId),
}

#[derive(Debug)]
pub enum Timer {
    RetryNode(NodeId),
    DropConnection(NodeId),
}

#[derive(Debug)]
pub enum InEvent {
    AddNode {
        node: NodeId,
        hints: NodeHints,
    },
    AddResource {
        resource: Resource,
        hints: ResourceHints,
    },
    TransferReady {
        id: TransferId,
    },
    TransferFailed {
        id: TransferId,
        failure: FailureAction,
    },
    NodeConnected {
        node: NodeId,
    },
    NodeFailed {
        node: NodeId,
    },
    TimerExpired {
        timer: Timer,
    },
}

impl State {
    pub fn new(concurrency_limits: ConcurrencyLimits) -> Self {
        Self {
            limits: concurrency_limits,
            ..Default::default()
        }
    }
    pub fn handle(&mut self, in_event: InEvent) {
        debug!("in_event {in_event:?}");
        match in_event {
            InEvent::AddNode { node, hints } => self.add_node(node, hints),
            InEvent::AddResource { resource, hints } => self.add_resource(resource, hints),
            InEvent::TransferReady { id } => self.on_transfer_ready(id),
            InEvent::TransferFailed { id, failure } => self.on_transfer_failed(id, failure),
            InEvent::NodeConnected { node } => self.on_node_connected(node),
            InEvent::NodeFailed { node } => self.on_node_failed(node, true),
            InEvent::TimerExpired { timer } => self.on_timer(timer),
        }
    }

    pub fn events(&mut self) -> impl Iterator<Item = OutEvent> + '_ {
        self.actions.drain(..)
    }

    fn add_node(&mut self, node: NodeId, hints: NodeHints) {
        let at_connections_capacity = self.at_connections_capacity();
        let node_info = self.nodes.entry(node).or_default();
        for group in hints.groups {
            if node_info.groups.insert(group) {
                self.groups.entry(group).or_default().nodes.insert(node);
            }
        }
        for resource in hints.resources {
            // TODO: I think if we add the resource *later*, then it will not be associated to the
            // node..
            if node_info.resources.insert(resource) {
                if let Some(resource_state) = self.resources.get_mut(&resource) {
                    resource_state.nodes.insert(node);
                    resource_state.skip_nodes.remove(&node);
                }
            }
        }
        match node_info.state {
            NodeState::Pending { .. } => {
                // node is pending - nothing to do
            }
            NodeState::Connected { .. } => {
                // add more transfers if needed
                // todo: could be optimized likely to look at the new things only
                self.node_fill_transfers(node);
            }
            NodeState::Disconnected { .. } => {
                if !at_connections_capacity
                    && node_should_connect(&self.resources, &self.groups, &node, &node_info)
                {
                    self.actions.push(node_info.connect(node))
                }
            }
        }
    }

    fn connection_count(&self) -> usize {
        self.nodes.values().filter(|n| n.state.is_active()).count()
    }

    fn at_connections_capacity(&self) -> bool {
        self.limits.at_connections_capacity(self.connection_count())
    }

    fn add_resource(&mut self, resource: Resource, hints: ResourceHints) {
        let state = self.resources.entry(resource).or_default();
        state.skip_nodes.extend(hints.skip_nodes.into_iter());
        for group in hints.groups {
            if state.groups.insert(group) {
                let group_state = self.groups.entry(group).or_default();
                group_state.resources.insert(resource);
            }
        }
        for node in hints.nodes {
            self.add_node(node, NodeHints::with_resource(resource));
        }
    }

    fn node_fill_transfers(&mut self, node: NodeId) {
        let Some(node_info) = self.nodes.get_mut(&node) else {
            return;
        };

        if !node_info.is_connected()
            || self
                .limits
                .node_at_request_capacity(node_info.active_transfers.len())
            || self
                .limits
                .at_requests_capacity(self.active_transfers.len())
        {
            return;
        }

        if let Some(remaining) = self
            .limits
            .node_remaining_requests(node_info.active_transfers.len())
        {
            let remaining: usize = remaining.into();
            let candidates = node_resource_iter(&self.resources, &self.groups, node_info);
            let mut next_resources = HashSet::new();
            for (resource, state) in candidates {
                if !state.can_start_transfer(&node) {
                    continue;
                }
                next_resources.insert(*resource);
                if next_resources.len() == remaining {
                    break;
                }
            }

            for resource in next_resources {
                let resource_state = self.resources.get_mut(&resource).expect("just checked");

                let id = self.transfer_id.next();
                let transfer = Transfer { id, resource, node };
                self.actions.push(OutEvent::StartTransfer(transfer.clone()));

                self.active_transfers.insert(id, transfer);
                node_info.active_transfers.insert(id);
                resource_state.active_transfer = Some(id);
            }
        }

        if node_info.active_transfers.is_empty() && !node_info.in_disconnect_timeout {
            self.actions.push(OutEvent::RegisterTimer(
                IDLE_PEER_TIMEOUT,
                Timer::DropConnection(node),
            ));
            node_info.in_disconnect_timeout = true;
        } else {
            node_info.in_disconnect_timeout = false;
        }
    }

    fn on_node_connected(&mut self, node: NodeId) {
        let Some(node_info) = self.nodes.get_mut(&node) else {
            return;
        };
        node_info.state = NodeState::Connected;
        self.node_fill_transfers(node)
    }

    fn on_node_failed(&mut self, node: NodeId, may_reconnect: bool) {
        let Some(node_info) = self.nodes.get_mut(&node) else {
            return;
        };
        node_info.state = if may_reconnect && !node_info.should_reconnect() {
            // TODO: timeout
            let timeout = Duration::from_secs(1);
            self.actions
                .push(OutEvent::RegisterTimer(timeout, Timer::RetryNode(node)));
            NodeState::Pending {
                state: PendingState::RetryTimeout,
                remaining_retries: node_info.remaining_retries(),
            }
        } else {
            // todo: remove failed nodes?
            // self.remove_node(node);
            self.actions.push(OutEvent::DropConnection(node));
            let mut removed_resources = IndexSet::default();
            std::mem::swap(&mut removed_resources, &mut node_info.resources);
            for r in removed_resources.iter() {
                if let Some(resource_state) = self.resources.get_mut(r) {
                    resource_state.nodes.remove(&node);
                }
            }
            NodeState::Disconnected { failed: true }
        };

        // queue reconnects
        if let Some(remaining) = self.limits.remaining_connections(self.connection_count()) {
            for (node, node_info) in self
                .nodes
                .iter_mut()
                .filter(|(node, node_info)| {
                    node_should_connect(&self.resources, &self.groups, node, node_info)
                })
                .take(remaining.into())
            {
                self.actions.push(node_info.connect(*node))
            }
        }
    }

    fn on_transfer_ready(&mut self, id: TransferId) {
        let Some(transfer) = self.active_transfers.remove(&id) else {
            debug_assert!(
                false,
                "transfer_ready called but TransferId not in active_transfers"
            );
            return;
        };
        let Transfer { id, resource, node } = transfer;
        if let Some(resource_state) = self.resources.remove(&resource) {
            for node in resource_state.nodes.iter() {
                if let Some(node_state) = self.nodes.get_mut(node) {
                    node_state.resources.remove(&resource);
                }
            }
            for group in resource_state.groups.iter() {
                if let Some(group_state) = self.groups.get_mut(group) {
                    group_state.resources.remove(&resource);
                }
            }
        }
        if let Some(node_state) = self.nodes.get_mut(&node) {
            node_state.active_transfers.remove(&id);
            self.node_fill_transfers(node);
        }
    }

    fn on_transfer_failed(&mut self, id: TransferId, action: FailureAction) {
        let Some(transfer) = self.active_transfers.remove(&id) else {
            debug_assert!(
                false,
                "transfer_failed called but TransferId not in active_transfers"
            );
            return;
        };
        let Transfer { id, resource, node } = transfer;
        let resource_state = self.resources.entry(resource).or_default();
        resource_state.skip_nodes.insert(node);
        resource_state.active_transfer = None;
        if let Some(node_state) = self.nodes.get_mut(&node) {
            node_state.active_transfers.remove(&id);
            match action {
                FailureAction::NotFound | FailureAction::AbortRequest(_) => {
                    self.node_fill_transfers(node)
                }
                FailureAction::DropPeer(_) => self.on_node_failed(node, false),
                FailureAction::RetryLater(_) => self.on_node_failed(node, true),
            }
        }
    }

    fn on_timer(&mut self, timer: Timer) {
        match timer {
            Timer::RetryNode(node) => {
                if let Some(state) = self.nodes.get_mut(&node) {
                    state.state = NodeState::connecting();
                    self.actions.push(OutEvent::StartDial(node))
                }
            }
            Timer::DropConnection(node) => {
                if let Some(state) = self.nodes.get_mut(&node) {
                    if state.in_disconnect_timeout {
                        state.in_disconnect_timeout = false;
                        if state.active_transfers.is_empty() {
                            state.state = NodeState::Disconnected { failed: false };
                            self.actions.push(OutEvent::DropConnection(node));
                        }
                    }
                }
            }
        }
    }
}

fn node_should_connect<'a>(
    resources: &'a BTreeMap<Resource, ResourceState>,
    groups: &'a BTreeMap<Group, GroupState>,
    node: &'a NodeId,
    node_info: &'a NodeInfo,
) -> bool {
    node_info.state.may_connect() && node_is_needed(resources, groups, &node, &node_info)
}

fn node_is_needed<'a>(
    resources: &'a BTreeMap<Resource, ResourceState>,
    groups: &'a BTreeMap<Group, GroupState>,
    node: &'a NodeId,
    node_info: &'a NodeInfo,
) -> bool {
    node_resource_iter(&resources, &groups, &node_info)
        .any(|(_resource, state)| state.can_start_transfer(node))
}

fn node_resource_iter<'a>(
    resources: &'a BTreeMap<Resource, ResourceState>,
    groups: &'a BTreeMap<Group, GroupState>,
    node_info: &'a NodeInfo,
) -> impl Iterator<Item = (&'a Resource, &'a ResourceState)> {
    resource_iter(
        resources,
        groups,
        node_info.resources.iter(),
        node_info.groups.iter(),
    )
}

fn resource_iter<'a>(
    resources: &'a BTreeMap<Resource, ResourceState>,
    groups: &'a BTreeMap<Group, GroupState>,
    match_resources: impl Iterator<Item = &'a Resource>,
    match_groups: impl Iterator<Item = &'a Group>,
) -> impl Iterator<Item = (&'a Resource, &'a ResourceState)> {
    let resources_via_group = match_groups
        .filter_map(|g| groups.get(g))
        .map(|g| g.resources.iter())
        .flatten();
    match_resources
        .chain(resources_via_group)
        .filter_map(|r| resources.get(r).map(|state| (r, state)))
}
