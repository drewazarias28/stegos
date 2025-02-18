// Copyright 2019 Stegos AG
// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use super::addresses::Addresses;
use super::handler::{KademliaHandler, KademliaHandlerEvent, KademliaHandlerIn, KademliaRequestId};
use super::kbucket::{KBucketsTable, Update};
use super::metrics::{KBUCKET_TABLE_SIZE, PEER_TABLE_SIZE};
use super::protocol::{KadConnectionType, KadPeer};
use super::query::{QueryConfig, QueryState, QueryStatePollOut, QueryTarget};
use fnv::{FnvHashMap, FnvHashSet};
use futures::{prelude::*, stream};
use libp2p::core::swarm::{
    ConnectedPoint, NetworkBehaviour, NetworkBehaviourAction, PollParameters,
};
use libp2p::core::{protocols_handler::ProtocolsHandler, Multiaddr, PeerId};
use libp2p::multihash::Multihash;
use log::{debug, trace};
use lru_time_cache::LruCache;
use rand;
use smallvec::SmallVec;
use std::vec::IntoIter as VecIntoIter;
use std::{cmp::Ordering, error, marker::PhantomData, time::Duration, time::Instant};
use stegos_crypto::pbc;
use stegos_crypto::utils::u8v_to_hexstr;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::timer::Interval;

use crate::utils::IntoMultihash;

// Buckets will be treated as expired, if they weren't touch during 5 minutes
const BUCKET_EXPIRATION_PERIOD: u64 = 5 * 60;
// At which interval update metrics (secs)
const METRICS_UPDATE_INTERVAL: u64 = 1;

/// Network behaviour that handles Kademlia.
pub struct Kademlia<TSubstream> {
    /// NodeId of this node
    my_id: pbc::PublicKey,
    /// Storage for the nodes. Contains the known multiaddresses for this node.
    kbuckets: KBucketsTable<pbc::PublicKey, NodeInfo>,

    /// Mapping PeerId -> pbc::PublicKey (we use Vec<u8> here, 'cause PeerId doesn't implement Ord)
    known_peers: LruCache<Vec<u8>, pbc::PublicKey>,

    /// All the iterative queries we are currently performing, with their ID. The last parameter
    /// is the list of accumulated providers for `GET_PROVIDERS` queries.
    active_queries: FnvHashMap<QueryId, (QueryState, QueryPurpose, Vec<pbc::PublicKey>)>,

    /// List of queries to start once we are inside `poll()`.
    queries_to_starts: SmallVec<[(QueryId, QueryTarget, QueryPurpose); 8]>,

    /// List of peers the swarm is connected to.
    connected_peers: FnvHashSet<PeerId>,

    /// Contains a list of peer IDs which we are not connected to, and an RPC query to send to them
    /// once they connect.
    pending_rpcs: SmallVec<[(pbc::PublicKey, KademliaHandlerIn<QueryId>); 8]>,

    /// Identifier for the next query that we start.
    next_query_id: QueryId,

    /// Requests received by a remote that we should fulfill as soon as possible.
    remote_requests: SmallVec<[(PeerId, KademliaRequestId, QueryTarget); 4]>,

    /// List of values and peers that are providing them.
    ///
    /// Our local peer ID can be in this container.
    // TODO: Note that in reality the value is a SHA-256 of the actual value (https://github.com/libp2p/rust-libp2p/issues/694)
    values_providers: FnvHashMap<Multihash, SmallVec<[pbc::PublicKey; 20]>>,

    /// List of values that we are providing ourselves. Must be kept in sync with
    /// `values_providers`.
    providing_keys: FnvHashSet<Multihash>,

    /// Interval to send `ADD_PROVIDER` messages to everyone.
    refresh_add_providers: stream::Fuse<Interval>,

    /// `α` in the Kademlia reference papers. Designates the maximum number of queries that we
    /// perform in parallel.
    parallelism: usize,

    /// `k` in the Kademlia reference papers. Number of results in a find node query.
    num_results: usize,

    /// Timeout for each individual RPC query.
    rpc_timeout: Duration,

    /// Events to return when polling.
    queued_events: SmallVec<[NetworkBehaviourAction<KademliaHandlerIn<QueryId>, KademliaOut>; 32]>,

    /// List of providers to add to the topology as soon as we are in `poll()`.
    add_provider: SmallVec<[(Multihash, pbc::PublicKey); 32]>,

    /// When metrics were updated last time
    metrics_last_update: Instant,

    /// Marker to pin the generics.
    marker: PhantomData<TSubstream>,
}

#[derive(Clone, Debug)]
pub struct NodeInfo {
    peer_id: Option<PeerId>,
    addresses: Addresses,
}

impl Default for NodeInfo {
    fn default() -> Self {
        NodeInfo {
            peer_id: None,
            addresses: Addresses::default(),
        }
    }
}

impl NodeInfo {
    pub fn has_peer_id(&self) -> bool {
        self.peer_id.is_some()
    }
    pub fn peer_id(&self) -> Option<PeerId> {
        self.peer_id.clone()
    }
    pub fn has_addresses(&self) -> bool {
        self.addresses.size() > 0
    }
}

/// Opaque type. Each query that we start gets a unique number.
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub struct QueryId(usize);

/// Reason why we have this query in the list of queries.
#[derive(Debug, Clone, PartialEq, Eq)]
enum QueryPurpose {
    /// The query was created for the Kademlia initialization process.
    Initialization,
    /// The user requested this query to be performed. It should be reported when finished.
    UserRequest,
    /// We should add an `ADD_PROVIDER` message to the peers of the outcome.
    AddProvider(Multihash),
}

impl<TSubstream> Kademlia<TSubstream> {
    /// Creates a `Kademlia`.
    #[inline]
    pub fn new(local_node_id: pbc::PublicKey) -> Self {
        Self::new_inner(local_node_id, true)
    }

    /// Creates a `Kademlia`.
    ///
    /// Contrary to `new`, doesn't perform the initialization queries that store our local ID into
    /// the DHT.
    #[inline]
    pub fn without_init(local_node_id: pbc::PublicKey) -> Self {
        Self::new_inner(local_node_id, false)
    }

    /// Returns local node's id (pbc::PublicKey)
    #[inline]
    pub fn my_id(&self) -> &pbc::PublicKey {
        &self.my_id
    }

    /// Change node's id (pbc::PublicKey)
    pub fn change_id(&mut self, new_id: pbc::PublicKey) {
        self.kbuckets = self.kbuckets.new_table(new_id.clone());
        self.my_id = new_id;
    }

    #[inline]
    pub fn find_closest(&mut self, id: &pbc::PublicKey) -> VecIntoIter<pbc::PublicKey> {
        self.kbuckets.find_closest(id)
    }

    #[inline]
    pub fn find_closest_with_self(&mut self, id: &pbc::PublicKey) -> VecIntoIter<pbc::PublicKey> {
        self.kbuckets.find_closest_with_self(id)
    }

    #[inline]
    pub fn get_node(&self, node_id: &pbc::PublicKey) -> Option<NodeInfo> {
        match self.kbuckets.get(node_id) {
            Some(n) => Some(n.clone()),
            None => None,
        }
    }

    /// Sets peer_id to the corresponging node_id
    pub fn set_peer_id(&mut self, node_id: &pbc::PublicKey, peer_id: PeerId) {
        if let Some(node_info) = self.kbuckets.entry_mut(node_id) {
            node_info.peer_id = Some(peer_id.clone());
        }
        self.known_peers
            .insert(peer_id.as_bytes().to_vec(), node_id.clone());
    }

    /// Adds a known address for the given `PeerId`. We are connected to this address.
    pub fn add_connected_address(&mut self, node_id: &pbc::PublicKey, address: Multiaddr) {
        if let Some(node_info) = self.kbuckets.entry_mut(node_id) {
            node_info.addresses.insert_connected(address);
        }
    }

    /// Adds a known address for the given `PeerId`. We are not connected or don't know whether we
    /// are connected to this address.
    pub fn add_not_connected_address(&mut self, node_id: &pbc::PublicKey, address: Multiaddr) {
        if let Some(node_info) = self.kbuckets.entry_mut(node_id) {
            node_info.addresses.insert_not_connected(address);
        }
    }

    /// Inner implementation of the constructors.
    fn new_inner(local_node_id: pbc::PublicKey, initialize: bool) -> Self {
        let parallelism = 3;

        let mut behaviour = Kademlia {
            my_id: local_node_id.clone(),
            kbuckets: KBucketsTable::new(
                local_node_id,
                Duration::from_secs(BUCKET_EXPIRATION_PERIOD),
            ),
            known_peers: LruCache::<Vec<u8>, pbc::PublicKey>::with_capacity(512 * (20 + 1)), // Total size of kBucketsTable
            queued_events: SmallVec::new(),
            queries_to_starts: SmallVec::new(),
            active_queries: Default::default(),
            connected_peers: Default::default(),
            pending_rpcs: SmallVec::with_capacity(parallelism),
            next_query_id: QueryId(0),
            remote_requests: SmallVec::new(),
            values_providers: FnvHashMap::default(),
            providing_keys: FnvHashSet::default(),
            refresh_add_providers: Interval::new_interval(Duration::from_secs(60)).fuse(), // TODO: constant
            parallelism,
            num_results: 20,
            rpc_timeout: Duration::from_secs(8),
            add_provider: SmallVec::new(),
            metrics_last_update: Instant::now(),
            marker: PhantomData,
        };

        if initialize {
            // As part of the initialization process, we start one `FIND_NODE` for each bit of the
            // possible range of node IDs.
            let my_hash = behaviour.kbuckets.my_id().into_multihash();
            for n in 0..512 {
                let random_hash = match gen_random_hash(&my_hash, n) {
                    Ok(p) => p,
                    Err(()) => continue,
                };

                behaviour.start_query(
                    QueryTarget::FindPeer(random_hash),
                    QueryPurpose::Initialization,
                );
            }
        }

        behaviour
    }

    /// Builds the answer to a request.
    fn build_result<TUserData>(
        &mut self,
        query: QueryTarget,
        request_id: KademliaRequestId,
        parameters: &mut PollParameters<'_>,
    ) -> KademliaHandlerIn<TUserData> {
        match query {
            QueryTarget::FindPeer(key) => {
                let closer_peers = self
                    .kbuckets
                    .find_closest_with_self(&key)
                    .take(self.num_results)
                    .map(|node_id| build_kad_peer(node_id, parameters, &self.kbuckets))
                    .collect();
                trace!(target: "stegos_network::kad", "sending FindNodeRes with: {:#?}", closer_peers);
                KademliaHandlerIn::FindNodeRes {
                    closer_peers,
                    request_id,
                }
            }
            QueryTarget::GetProviders(key) => {
                let closer_peers = self
                    .kbuckets
                    .find_closest_with_self(&key)
                    .take(self.num_results)
                    .map(|node_id| build_kad_peer(node_id, parameters, &self.kbuckets))
                    .collect();

                let provider_peers = self
                    .values_providers
                    .get(&key)
                    .into_iter()
                    .flat_map(|peers| peers)
                    .map(|node_id| build_kad_peer(node_id.clone(), parameters, &self.kbuckets))
                    .collect();

                KademliaHandlerIn::GetProvidersRes {
                    closer_peers,
                    provider_peers,
                    request_id,
                }
            }
        }
    }
}

impl<TSubstream> Kademlia<TSubstream> {
    /// Starts an iterative `FIND_NODE` request.
    ///
    /// This will eventually produce an event containing the nodes of the DHT closest to the
    /// requested `PeerId`.
    #[inline]
    pub fn find_node(&mut self, node_id: pbc::PublicKey) {
        self.start_query(
            QueryTarget::FindPeer(node_id.into_multihash()),
            QueryPurpose::UserRequest,
        );
    }

    /// Size of internal KBucketsTable
    #[inline]
    pub fn ktable_size(&self) -> usize {
        self.kbuckets.size()
    }

    /// Starts an iterative `GET_PROVIDERS` request.
    #[inline]
    pub fn get_providers(&mut self, key: Multihash) {
        self.start_query(QueryTarget::GetProviders(key), QueryPurpose::UserRequest);
    }

    /// Register the local node as the provider for the given key.
    ///
    /// This will periodically send `ADD_PROVIDER` messages to the nodes closest to the key. When
    /// someone performs a `GET_PROVIDERS` iterative request on the DHT, our local node will be
    /// returned as part of the results.
    ///
    /// The actual meaning of *providing* the value of a key is not defined, and is specific to
    /// the value whose key is the hash.
    pub fn add_providing(&mut self, key: pbc::PublicKey) {
        self.providing_keys.insert(key.clone().into_multihash());
        let providers = self
            .values_providers
            .entry(key.into_multihash())
            .or_insert_with(Default::default);
        let my_id = self.kbuckets.my_id();
        if !providers.iter().any(|k| k == my_id) {
            providers.push(my_id.clone());
        }

        // Trigger the next refresh now.
        self.refresh_add_providers = Interval::new(Instant::now(), Duration::from_secs(60)).fuse();
    }

    /// Cancels a registration done with `add_providing`.
    ///
    /// There doesn't exist any "remove provider" message to broadcast on the network, therefore we
    /// will still be registered as a provider in the DHT for as long as the timeout doesn't expire.
    pub fn remove_providing(&mut self, key: &Multihash) {
        self.providing_keys.remove(key);

        let providers = match self.values_providers.get_mut(key) {
            Some(p) => p,
            None => return,
        };

        // remove outselves from list of peers providing the key
        let my_id = self.my_id;
        if let Some(position) = providers.iter().position(|k| *k == my_id) {
            providers.remove(position);
            providers.shrink_to_fit();
        }
    }

    /// Internal function that starts a query.
    fn start_query(&mut self, target: QueryTarget, purpose: QueryPurpose) {
        let query_id = self.next_query_id;
        self.next_query_id.0 += 1;
        self.queries_to_starts.push((query_id, target, purpose));
    }
}

impl<TSubstream> NetworkBehaviour for Kademlia<TSubstream>
where
    TSubstream: AsyncRead + AsyncWrite,
{
    type ProtocolsHandler = KademliaHandler<TSubstream, QueryId>;
    type OutEvent = KademliaOut;

    fn new_handler(&mut self) -> Self::ProtocolsHandler {
        KademliaHandler::dial_and_listen()
    }

    fn addresses_of_peer(&mut self, peer_id: &PeerId) -> Vec<Multiaddr> {
        let peer = peer_id.clone();
        if let Some(node_id) = self.known_peers.get(&peer.into_bytes()) {
            self.kbuckets
                .get(node_id)
                .map(|l| l.addresses.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_else(Vec::new)
        } else {
            Vec::new()
        }
    }

    fn inject_connected(&mut self, id: PeerId, endpoint: ConnectedPoint) {
        let peer_id = id.clone().into_bytes();
        self.connected_peers.insert(id.clone());

        let node_id = match self.known_peers.get(&peer_id) {
            Some(id) => id,
            None => return,
        };

        if let Some(pos) = self.pending_rpcs.iter().position(|(p, _)| p == node_id) {
            let (_, rpc) = self.pending_rpcs.remove(pos);
            self.queued_events.push(NetworkBehaviourAction::SendEvent {
                peer_id: id.clone(),
                event: rpc,
            });
        }

        if let Update::Pending(to_ping) = self.kbuckets.set_connected(&node_id) {
            let target_node = to_ping.clone();
            if let Some(ref node_info) = self.kbuckets.get(&target_node) {
                if let Some(ref peer_id) = node_info.peer_id {
                    self.queued_events.push(NetworkBehaviourAction::DialPeer {
                        peer_id: peer_id.clone(),
                    })
                }
            }
        }

        if let ConnectedPoint::Dialer { address } = endpoint {
            if let Some(node_info) = self.kbuckets.entry_mut(&node_id) {
                node_info.addresses.insert_connected(address);
            }
        }
    }

    fn inject_addr_reach_failure(
        &mut self,
        peer_id: Option<&PeerId>,
        addr: &Multiaddr,
        e: &dyn error::Error,
    ) {
        debug!(target: "stegos_network::kad", "dialout failure: error={}", e);
        if let Some(peer_id) = peer_id {
            let peer_id = peer_id.clone().into_bytes();
            let node_id = match self.known_peers.get(&peer_id) {
                Some(id) => id,
                None => return,
            };

            if let Some(node_info) = self.kbuckets.get_mut(&node_id) {
                // TODO: don't remove the address if the error is that we are already connected
                //       to this peer
                node_info.addresses.remove_addr(addr);
            }
        }
    }

    fn inject_dial_failure(&mut self, peer_id: &PeerId) {
        let peer_id = peer_id.clone().into_bytes();
        let node_id = match self.known_peers.get(&peer_id) {
            Some(id) => id,
            None => return,
        };
        for query in self.active_queries.values_mut() {
            query.0.inject_rpc_error(node_id);
        }
    }

    fn inject_disconnected(&mut self, id: &PeerId, old_endpoint: ConnectedPoint) {
        let was_in = self.connected_peers.remove(id);
        debug_assert!(was_in);
        let peer_id = id.clone().into_bytes();
        let node_id = match self.known_peers.get(&peer_id) {
            Some(id) => id,
            None => return,
        };

        for (query, _, _) in self.active_queries.values_mut() {
            query.inject_rpc_error(&node_id);
        }

        if let ConnectedPoint::Dialer { address } = old_endpoint {
            if let Some(node_info) = self.kbuckets.get_mut(&node_id) {
                node_info.addresses.set_disconnected(&address);
            }
        }

        self.kbuckets.set_disconnected(&node_id);
    }

    fn inject_replaced(
        &mut self,
        peer_id: PeerId,
        old_endpoint: ConnectedPoint,
        new_endpoint: ConnectedPoint,
    ) {
        let peer = peer_id.clone().into_bytes();
        let node_id = match self.known_peers.get(&peer) {
            Some(id) => id,
            None => return,
        };
        // We need to re-send the active queries.
        for (query_id, (query, _, _)) in self.active_queries.iter() {
            if query.is_waiting(&node_id) {
                self.queued_events.push(NetworkBehaviourAction::SendEvent {
                    peer_id: peer_id.clone(),
                    event: query.target().to_rpc_request(*query_id),
                });
            }
        }

        if let ConnectedPoint::Dialer { address } = old_endpoint {
            if let Some(node_info) = self.kbuckets.get_mut(&node_id) {
                node_info.addresses.set_disconnected(&address);
            }
        }

        if let ConnectedPoint::Dialer { address } = new_endpoint {
            if let Some(node_info) = self.kbuckets.entry_mut(&node_id) {
                node_info.addresses.insert_connected(address);
            }
        }
    }

    fn inject_node_event(&mut self, source: PeerId, event: KademliaHandlerEvent<QueryId>) {
        match event {
            KademliaHandlerEvent::FindNodeReq { key, request_id } => {
                self.remote_requests
                    .push((source, request_id, QueryTarget::FindPeer(key)));
                return;
            }
            KademliaHandlerEvent::FindNodeRes {
                closer_peers,
                user_data,
            } => {
                // It is possible that we obtain a response for a query that has finished, which is
                // why we may not find an entry in `self.active_queries`.
                for peer in closer_peers.iter() {
                    let peer_id = match &peer.peer_id {
                        Some(p) => Some(p.clone()),
                        None => None,
                    };
                    self.queued_events
                        .push(NetworkBehaviourAction::GenerateEvent(
                            KademliaOut::Discovered {
                                node_id: peer.node_id.clone(),
                                peer_id,
                                addresses: peer.multiaddrs.clone(),
                                ty: peer.connection_ty,
                            },
                        ));
                }
                if let Some((query, _, _)) = self.active_queries.get_mut(&user_data) {
                    let peer_key = source.into_bytes();
                    let my_id = self.my_id;
                    if let Some(node_id) = self.known_peers.get(&peer_key) {
                        query.inject_rpc_result(
                            &node_id,
                            closer_peers.into_iter().filter_map(|kp| {
                                if kp.node_id == my_id {
                                    None
                                } else {
                                    Some(kp.node_id)
                                }
                            }),
                        )
                    }
                }
            }
            KademliaHandlerEvent::GetProvidersReq { key, request_id } => {
                self.remote_requests
                    .push((source, request_id, QueryTarget::GetProviders(key)));
                return;
            }
            KademliaHandlerEvent::GetProvidersRes {
                closer_peers,
                provider_peers,
                user_data,
            } => {
                for peer in closer_peers.iter().chain(provider_peers.iter()) {
                    let peer_id = match &peer.peer_id {
                        Some(p) => Some(p.clone()),
                        None => None,
                    };
                    self.queued_events
                        .push(NetworkBehaviourAction::GenerateEvent(
                            KademliaOut::Discovered {
                                node_id: peer.node_id.clone(),
                                peer_id,
                                addresses: peer.multiaddrs.clone(),
                                ty: peer.connection_ty,
                            },
                        ));
                }

                // It is possible that we obtain a response for a query that has finished, which is
                // why we may not find an entry in `self.active_queries`.
                if let Some((query, _, providers)) = self.active_queries.get_mut(&user_data) {
                    for peer in provider_peers {
                        providers.push(peer.node_id);
                    }
                    let peer_key = source.into_bytes();
                    if let Some(node_id) = self.known_peers.get(&peer_key) {
                        query.inject_rpc_result(
                            &node_id,
                            closer_peers.into_iter().map(|kp| kp.node_id),
                        )
                    }
                }
            }
            KademliaHandlerEvent::QueryError { user_data, .. } => {
                // It is possible that we obtain a response for a query that has finished, which is
                // why we may not find an entry in `self.active_queries`.
                if let Some((query, _, _)) = self.active_queries.get_mut(&user_data) {
                    let peer_key = source.into_bytes();
                    if let Some(node_id) = self.known_peers.get(&peer_key) {
                        query.inject_rpc_error(&node_id)
                    }
                }
            }
            KademliaHandlerEvent::AddProvider { key, provider_peer } => {
                let peer_id = match provider_peer.peer_id {
                    Some(p) => Some(p.clone()),
                    None => None,
                };
                self.queued_events
                    .push(NetworkBehaviourAction::GenerateEvent(
                        KademliaOut::Discovered {
                            node_id: provider_peer.node_id.clone(),
                            peer_id,
                            addresses: provider_peer.multiaddrs.clone(),
                            ty: provider_peer.connection_ty,
                        },
                    ));
                self.add_provider.push((key, provider_peer.node_id));
                return;
            }
        };
    }

    fn poll(
        &mut self,
        parameters: &mut PollParameters<'_>,
    ) -> Async<
        NetworkBehaviourAction<
            <Self::ProtocolsHandler as ProtocolsHandler>::InEvent,
            Self::OutEvent,
        >,
    > {
        // Update metrics
        if self.metrics_last_update.elapsed() > Duration::from_secs(METRICS_UPDATE_INTERVAL) {
            self.metrics_last_update = Instant::now();
            KBUCKET_TABLE_SIZE.set(self.kbuckets.size() as i64);
            PEER_TABLE_SIZE.set(self.known_peers.len() as i64);
        }
        // Flush the changes to the topology that we want to make.
        for (key, provider) in self.add_provider.drain() {
            // Don't add ourselves to the providers.
            if provider == *self.kbuckets.my_id() {
                continue;
            }
            let providers = self
                .values_providers
                .entry(key)
                .or_insert_with(Default::default);
            if !providers.iter().any(|k| k == &provider) {
                providers.push(provider);
            }
        }
        self.add_provider.shrink_to_fit();

        // Handle `refresh_add_providers`.
        match self.refresh_add_providers.poll() {
            Ok(Async::NotReady) => {}
            Ok(Async::Ready(Some(_))) => {
                for provided in self.providing_keys.clone().into_iter() {
                    let purpose = QueryPurpose::AddProvider(provided.clone());
                    self.start_query(QueryTarget::FindPeer(provided), purpose);
                }
            }
            // Ignore errors.
            Ok(Async::Ready(None)) | Err(_) => {}
        }

        // Start queries that are waiting to start.
        let table_size = self.ktable_size();
        for (query_id, query_target, query_purpose) in self.queries_to_starts.drain() {
            debug!(target: "stegos_network::kad", "Starting query: query_id={:?}, target={}, table_size={}", query_id, u8v_to_hexstr(query_target.as_hash().as_bytes()), table_size);
            let known_closest_peers = self
                .kbuckets
                .find_closest(&query_target.as_hash())
                .take(self.num_results);
            trace!(target: "stegos_network::kad", "Known peers for query: query_id={:?}, known_closest_peers={:#?}", query_id, known_closest_peers);
            self.active_queries.insert(
                query_id,
                (
                    QueryState::new(QueryConfig {
                        target: query_target,
                        parallelism: self.parallelism,
                        num_results: self.num_results,
                        rpc_timeout: self.rpc_timeout,
                        known_closest_peers,
                    }),
                    query_purpose,
                    Vec::new(), // TODO: insert ourselves if we provide the data?
                ),
            );
        }
        self.queries_to_starts.shrink_to_fit();

        // Handle remote queries.
        if !self.remote_requests.is_empty() {
            let (peer_id, request_id, query) = self.remote_requests.remove(0);
            let result = self.build_result(query, request_id, parameters);
            return Async::Ready(NetworkBehaviourAction::SendEvent {
                peer_id,
                event: result,
            });
        }

        loop {
            // Handle events queued by other parts of this struct
            if !self.queued_events.is_empty() {
                return Async::Ready(self.queued_events.remove(0));
            }
            self.queued_events.shrink_to_fit();

            // If iterating finds a query that is finished, stores it here and stops looping.
            let mut finished_query = None;
            let mut nodes_without_peerids: Vec<pbc::PublicKey> = Vec::new();

            'queries_iter: for (&query_id, (query, _, _)) in self.active_queries.iter_mut() {
                loop {
                    match query.poll() {
                        Async::Ready(QueryStatePollOut::Finished) => {
                            finished_query = Some(query_id);
                            break 'queries_iter;
                        }
                        Async::Ready(QueryStatePollOut::SendRpc {
                            node_id,
                            query_target,
                        }) => {
                            debug!(target: "stegos_network::kad", "got request to connect: node_id={}, target={}",
                                node_id, u8v_to_hexstr(query_target.as_hash().as_bytes()));
                            let rpc = query_target.to_rpc_request(query_id);
                            let target_peer = {
                                match self.kbuckets.get(&node_id) {
                                    Some(node_info) => match &node_info.peer_id {
                                        Some(p) => Some(p.clone()),
                                        None => None,
                                    },
                                    None => None,
                                }
                            };
                            if let Some(peer_id) = target_peer {
                                if self.connected_peers.contains(&peer_id) {
                                    debug!(target: "stegos_network::kad", "sending event to node: node_id={}, peer_id={}", node_id, peer_id);
                                    return Async::Ready(NetworkBehaviourAction::SendEvent {
                                        peer_id: peer_id.clone(),
                                        event: rpc,
                                    });
                                } else {
                                    debug!(target: "stegos_network::kad", "dialing node: node_id={}, peer_id={}", node_id, peer_id);
                                    self.pending_rpcs.push((node_id.clone(), rpc));
                                    return Async::Ready(NetworkBehaviourAction::DialPeer {
                                        peer_id: peer_id.clone(),
                                    });
                                }
                            } else {
                                debug!(target: "stegos_network::kad", "Can't find peer_id for node: node_id={}", node_id);
                                nodes_without_peerids.push(node_id.clone());
                            }
                        }
                        Async::Ready(QueryStatePollOut::CancelRpc { node_id }) => {
                            // We don't cancel if the RPC has already been sent out.
                            self.pending_rpcs.retain(|(id, _)| id != node_id);
                        }
                        Async::NotReady => break,
                    }
                }
            }

            if !nodes_without_peerids.is_empty() {
                for node in nodes_without_peerids.iter() {
                    for (query, _, _) in self.active_queries.values_mut() {
                        query.inject_rpc_error(&node);
                    }
                }
            }

            if let Some(finished_query) = finished_query {
                let (query, purpose, provider_peers) = self
                    .active_queries
                    .remove(&finished_query)
                    .expect("finished_query was gathered when iterating active_queries; QED.");
                match purpose {
                    QueryPurpose::Initialization => {}
                    QueryPurpose::UserRequest => {
                        let event = match query.target().clone() {
                            QueryTarget::FindPeer(key) => {
                                debug_assert!(provider_peers.is_empty());
                                KademliaOut::FindNodeResult {
                                    key,
                                    closer_peers: query.into_closest_peers().collect(),
                                }
                            }
                            QueryTarget::GetProviders(key) => KademliaOut::GetProvidersResult {
                                key,
                                closer_peers: query.into_closest_peers().collect(),
                                provider_peers,
                            },
                        };

                        break Async::Ready(NetworkBehaviourAction::GenerateEvent(event));
                    }
                    QueryPurpose::AddProvider(key) => {
                        for closest in query.into_closest_peers() {
                            let node_info = match self.kbuckets.get(&closest) {
                                Some(n) => n,
                                None => continue,
                            };
                            if let Some(peer_id) = &node_info.peer_id {
                                let event = NetworkBehaviourAction::SendEvent {
                                    peer_id: peer_id.clone(),
                                    event: KademliaHandlerIn::AddProvider {
                                        key: key.clone(),
                                        provider_peer: build_kad_peer(
                                            self.my_id.clone(),
                                            parameters,
                                            &self.kbuckets,
                                        ),
                                    },
                                };
                                self.queued_events.push(event);
                            }
                        }
                    }
                }
            } else {
                break Async::NotReady;
            }
        }
    }
}

/// Output event of the `Kademlia` behaviour.
#[derive(Debug, Clone)]
pub enum KademliaOut {
    /// We have discovered a node.
    Discovered {
        /// PBC PublicKey of the Node
        node_id: pbc::PublicKey,
        /// Id of the node that was discovered.
        peer_id: Option<PeerId>,
        /// Addresses of the node.
        addresses: Vec<Multiaddr>,
        /// How the reporter is connected to the reported.
        ty: KadConnectionType,
    },

    /// Result of a `FIND_NODE` iterative query.
    FindNodeResult {
        /// The key that we looked for in the query.
        key: Multihash,
        /// List of peers ordered from closest to furthest away.
        closer_peers: Vec<pbc::PublicKey>,
    },

    /// Result of a `GET_PROVIDERS` iterative query.
    GetProvidersResult {
        /// The key that we looked for in the query.
        key: Multihash,
        /// The peers that are providing the requested key.
        provider_peers: Vec<pbc::PublicKey>,
        /// List of peers ordered from closest to furthest away.
        closer_peers: Vec<pbc::PublicKey>,
    },
}

// Generates a random `Multihash (SHA3-512)` that belongs to the given bucket.
//
// Returns an error if `bucket_num` is out of range.
fn gen_random_hash(my_id: &Multihash, bucket_num: usize) -> Result<Multihash, ()> {
    let my_id_len = my_id.as_bytes().len();

    // TODO: this 2 is magic here; it is the length of the hash of the multihash
    let bits_diff = bucket_num + 1;
    if bits_diff > 8 * (my_id_len - 2) {
        return Err(());
    }

    let mut random_id = [0; 128];
    for byte in 0..my_id_len {
        match byte.cmp(&(my_id_len - bits_diff / 8 - 1)) {
            Ordering::Less => {
                random_id[byte] = my_id.as_bytes()[byte];
            }
            Ordering::Equal => {
                let mask: u8 = (1 << (bits_diff % 8)) - 1;
                random_id[byte] = (my_id.as_bytes()[byte] & !mask) | (rand::random::<u8>() & mask);
            }
            Ordering::Greater => {
                random_id[byte] = rand::random();
            }
        }
    }

    let random_hash = Multihash::from_bytes(random_id[..my_id_len].to_owned())
        .expect("randomly-generated Multihash should always be valid");
    Ok(random_hash)
}

/// Builds a `KadPeer` struct corresponding to the given `NodeId`.
/// The `PeerId` can be the same as the local one.
///
/// > **Note**: This is just a convenience function that doesn't do anything note-worthy.
fn build_kad_peer(
    node_id: pbc::PublicKey,
    parameters: &mut PollParameters<'_>,
    kbuckets: &KBucketsTable<pbc::PublicKey, NodeInfo>,
) -> KadPeer {
    let is_self = node_id == *kbuckets.my_id();

    let (peer_id, multiaddrs, connection_ty) = if is_self {
        let addrs = parameters.external_addresses().map(|v| v.clone()).collect();
        (
            Some(parameters.local_peer_id().clone()),
            addrs,
            KadConnectionType::Connected,
        )
    } else if let Some(node_info) = kbuckets.get(&node_id) {
        let connected = if node_info.addresses.is_connected() {
            KadConnectionType::Connected
        } else {
            // TODO: there's also pending connection
            KadConnectionType::NotConnected
        };

        let peer_id = if let Some(peer) = &node_info.peer_id {
            Some(peer.clone())
        } else {
            None
        };

        (
            peer_id,
            node_info.addresses.iter().cloned().collect(),
            connected,
        )
    } else {
        // TODO: there's also pending connection
        (None, Vec::new(), KadConnectionType::NotConnected)
    };

    KadPeer {
        node_id: node_id.clone(),
        peer_id,
        multiaddrs,
        connection_ty,
    }
}
