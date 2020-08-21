/*
 copyright: (c) 2013-2019 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/
use std::mem;

use net::PeerAddress;
use net::Neighbor;
use net::NeighborKey;
use net::Error as net_error;
use net::db::PeerDB;
use net::asn::ASEntry4;

use net::*;

use net::connection::ConnectionOptions;
use net::connection::NetworkReplyHandle;
use net::connection::ReplyHandleP2P;
use net::connection::ReplyHandleHttp;

use net::chat::ConversationP2P;
use net::chat::NeighborStats;

use net::relay::RelayerStats;

use net::download::BlockDownloader;

use net::poll::NetworkState;
use net::poll::NetworkPollState;

use net::db::LocalPeer;

use net::neighbors::*;

use net::prune::*;

use net::server::*;

use net::relay::*;

use util::db::Error as db_error;
use util::db::DBConn;

use util::secp256k1::Secp256k1PublicKey;
use util::hash::to_hex;

use std::sync::mpsc::SyncSender;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::sync_channel;
use std::sync::mpsc::SendError;
use std::sync::mpsc::TrySendError;
use std::sync::mpsc::RecvError;
use std::sync::mpsc::TryRecvError;

use std::net::SocketAddr;

use std::collections::VecDeque;
use std::collections::HashMap;
use std::collections::HashSet;
use std::cmp::Ordering;

use burnchains::Address;
use burnchains::PublicKey;
use burnchains::Burnchain;
use burnchains::BurnchainView;

use chainstate::burn::db::sortdb::{
    SortitionDB, SortitionId
};

use chainstate::stacks::db::StacksChainState;

use chainstate::stacks::{MAX_BLOCK_LEN, MAX_TRANSACTION_LEN};

use util::log;
use util::get_epoch_time_secs;

use rand::prelude::*;
use rand::thread_rng;

use mio;
use mio::net as mio_net;

use net::inv::*;
use net::relay::*;
use net::rpc::RPCHandlerArgs;

/// inter-thread request to send a p2p message from another thread in this program.
#[derive(Debug)]
pub enum NetworkRequest {
    Ban(Vec<NeighborKey>),
    AdvertizeBlocks(BlocksAvailableMap),            // announce to all wanting neighbors that we have these blocks
    AdvertizeMicroblocks(BlocksAvailableMap),       // announce to all wanting neighbors that we have these confirmed microblock streams
    Relay(NeighborKey, StacksMessage),
    Broadcast(Vec<RelayData>, StacksMessageType)
}

/// Handle for other threads to use to issue p2p network requests.
/// The "main loop" for sending/receiving data is a select/poll loop, and runs outside of other
/// threads that need a synchronous RPC or a multi-RPC interface.  This object gives those threads
/// a way to issue commands and hear back replies from them.
pub struct NetworkHandle {
    chan_in: SyncSender<NetworkRequest>,
}

/// Internal handle for receiving requests from a NetworkHandle.
/// This is the 'other end' of a NetworkHandle inside the peer network struct.
struct NetworkHandleServer {
    chan_in: Receiver<NetworkRequest>,
}

impl NetworkHandle {
    pub fn new(chan_in: SyncSender<NetworkRequest>) -> NetworkHandle {
        NetworkHandle {
            chan_in: chan_in,
        }
    }

    /// Send out a command to the p2p thread.  Do not bother waiting for the response.
    /// Error out if the channel buffer is out of space
    fn send_request(&mut self, req: NetworkRequest) -> Result<(), net_error> {
        match self.chan_in.try_send(req) {
            Ok(_) => Ok(()),
            Err(TrySendError::Full(_)) => {
                warn!("P2P handle channel is full");
                Err(net_error::FullHandle)
            }
            Err(TrySendError::Disconnected(_)) => {
                warn!("P2P handle channel is disconnected");
                Err(net_error::InvalidHandle)
            }
        }
    }

    /// Ban a peer
    pub fn ban_peers(&mut self, neighbor_keys: Vec<NeighborKey>) -> Result<(), net_error> {
        let req = NetworkRequest::Ban(neighbor_keys);
        self.send_request(req)
    }

    /// Advertize blocks
    pub fn advertize_blocks(&mut self, blocks: BlocksAvailableMap) -> Result<(), net_error> {
        let req = NetworkRequest::AdvertizeBlocks(blocks);
        self.send_request(req)
    }
    
    /// Advertize microblocks
    pub fn advertize_microblocks(&mut self, blocks: BlocksAvailableMap) -> Result<(), net_error> {
        let req = NetworkRequest::AdvertizeMicroblocks(blocks);
        self.send_request(req)
    }

    /// Relay a message to a peer via the p2p network thread, expecting no reply.
    /// Called from outside the p2p thread by other threads.
    pub fn relay_signed_message(&mut self, neighbor_key: NeighborKey, msg: StacksMessage) -> Result<(), net_error> {
        let req = NetworkRequest::Relay(neighbor_key, msg);
        self.send_request(req)
    }

    /// Broadcast a message to our neighbors via the p2p network thread.
    /// Add relay information for each one.
    pub fn broadcast_message(&mut self, relay_hints: Vec<RelayData>, msg: StacksMessageType) -> Result<(), net_error> {
        let req = NetworkRequest::Broadcast(relay_hints, msg);
        self.send_request(req)
    }
}

impl NetworkHandleServer {
    pub fn new(chan_in: Receiver<NetworkRequest>) -> NetworkHandleServer {
        NetworkHandleServer {
            chan_in: chan_in,
        }
    }

    pub fn pair(bufsz: usize) -> (NetworkHandleServer, NetworkHandle) {
        let (msg_send, msg_recv) = sync_channel(bufsz);
        let server = NetworkHandleServer::new(msg_recv);
        let client = NetworkHandle::new(msg_send);
        (server, client)
    }
}

#[derive(Debug, Clone, PartialEq, Copy)]
pub enum PeerNetworkWorkState {
    GetPublicIP,
    ConfirmPublicIP,
    BlockInvSync,
    BlockDownload,
    Prune
}

pub type PeerMap = HashMap<usize, ConversationP2P>;

pub struct PeerNetwork {
    pub local_peer: LocalPeer,
    pub peer_version: u32,
    pub chain_view: BurnchainView,

    pub peerdb: PeerDB,

    // ongoing p2p conversations (either they reached out to us, or we to them)
    pub peers: PeerMap,
    pub sockets: HashMap<usize, mio_net::TcpStream>,
    pub events: HashMap<NeighborKey, usize>,
    pub connecting: HashMap<usize, (mio_net::TcpStream, bool, u64)>,   // (socket, outbound?, connection sent timestamp)
    pub bans: HashSet<usize>,

    // ongoing messages the network is sending via the p2p interface (not bound to a specific
    // conversation).
    pub relay_handles: HashMap<usize, VecDeque<ReplyHandleP2P>>,
    pub relayer_stats: RelayerStats,

    // handles for other threads to send/receive data to peers
    handles: VecDeque<NetworkHandleServer>,

    // network I/O
    network: Option<NetworkState>,
    p2p_network_handle: usize,
    http_network_handle: usize,

    // info on the burn chain we're tracking 
    pub burnchain: Burnchain,

    // connection options
    pub connection_opts: ConnectionOptions,

    // work state -- we can be walking, fetching block inventories, fetching blocks, pruning, etc.
    pub work_state: PeerNetworkWorkState,

    // neighbor walk state 
    pub walk: Option<NeighborWalk>,
    pub walk_deadline: u64,
    pub walk_count: u64,
    pub walk_attempts: u64,
    pub walk_retries: u64,
    pub walk_total_step_count: u64,
    pub walk_pingbacks: HashMap<NeighborAddress, NeighborPingback>,   // inbound peers for us to try to ping back and add to our frontier, mapped to (peer_version, network_id, timeout, pubkey)
    pub walk_result: NeighborWalkResult,        // last successful neighbor walk result
    
    // peer block inventory state
    pub inv_state: Option<InvState>,

    // peer block download state
    pub block_downloader: Option<BlockDownloader>,

    // do we need to do a prune at the end of the work state cycle?
    pub do_prune: bool,

    // prune state
    pub prune_deadline: u64,

    // how often we pruned a given inbound/outbound peer
    pub prune_outbound_counts: HashMap<NeighborKey, u64>,
    pub prune_inbound_counts: HashMap<NeighborKey, u64>,

    // http endpoint, used for driving HTTP conversations (some of which we initiate)
    pub http: HttpPeer,

    // our own neighbor address that we bind on
    bind_nk: NeighborKey,

    // our public IP address that we give out in our handshakes
    pub public_ip_learned: bool,        // was the IP address given to us, or did we have to go learn it?
    pub public_ip_confirmed: bool,      // once we learned the IP address, were we able to confirm it by self-connecting?
    public_ip_address_unconfirmed: Option<(PeerAddress, u16)>,
    public_ip_requested_at: u64,
    public_ip_learned_at: u64,
    public_ip_reply_handle: Option<ReplyHandleP2P>,
    public_ip_self_event_id: usize,
    public_ip_ping_nonce: u32,
    public_ip_retries: u64,
}

impl PeerNetwork {
    pub fn new(peerdb: PeerDB, mut local_peer: LocalPeer, peer_version: u32, burnchain: Burnchain, chain_view: BurnchainView, connection_opts: ConnectionOptions) -> PeerNetwork {
        let http = HttpPeer::new(local_peer.network_id, burnchain.clone(), chain_view.clone(), connection_opts.clone(), 0);
        let pub_ip = connection_opts.public_ip_address.clone();
        let pub_ip_learned = pub_ip.is_none();
        local_peer.public_ip_address = pub_ip.clone();
        PeerNetwork {
            local_peer: local_peer,
            peer_version: peer_version,
            chain_view: chain_view, 

            peerdb: peerdb,

            peers: PeerMap::new(),
            sockets: HashMap::new(),
            events: HashMap::new(),
            connecting: HashMap::new(),
            bans: HashSet::new(),

            relay_handles: HashMap::new(),
            relayer_stats: RelayerStats::new(),

            handles: VecDeque::new(),
            network: None,
            p2p_network_handle: 0,
            http_network_handle: 0,

            burnchain: burnchain,
            connection_opts: connection_opts,

            work_state: PeerNetworkWorkState::GetPublicIP,

            walk: None,
            walk_deadline: 0,
            walk_attempts: 0,
            walk_retries: 0,
            walk_count: 0,
            walk_total_step_count: 0,
            walk_pingbacks: HashMap::new(),
            walk_result: NeighborWalkResult::new(),
            
            inv_state: None,
            block_downloader: None,

            do_prune: false,

            prune_deadline: 0,
            prune_outbound_counts : HashMap::new(),
            prune_inbound_counts : HashMap::new(),

            http: http,
            bind_nk: NeighborKey {
                network_id: 0,
                peer_version: 0,
                addrbytes: PeerAddress([0u8; 16]),
                port: 0
            },

            public_ip_address_unconfirmed: pub_ip.clone(),
            public_ip_learned: pub_ip_learned,
            public_ip_requested_at: 0,
            public_ip_learned_at: 0,
            public_ip_confirmed: false,
            public_ip_reply_handle: None,
            public_ip_self_event_id: 0,
            public_ip_ping_nonce: 0,
            public_ip_retries: 0
        }
    }

    /// start serving.
    pub fn bind(&mut self, my_addr: &SocketAddr, http_addr: &SocketAddr) -> Result<(), net_error> {
        let mut net = NetworkState::new(self.connection_opts.max_sockets)?;

        let p2p_handle = net.bind(my_addr)?;
        let http_handle = net.bind(http_addr)?;

        test_debug!("{:?}: bound on p2p {:?}, http {:?}", &self.local_peer, my_addr, http_addr);

        self.network = Some(net);
        self.p2p_network_handle = p2p_handle;
        self.http_network_handle = http_handle;

        self.http.set_server_handle(http_handle);

        self.bind_nk = NeighborKey {
            network_id: self.local_peer.network_id,
            peer_version: self.peer_version,
            addrbytes: PeerAddress::from_socketaddr(my_addr),
            port: my_addr.port()
        };

        Ok(())
    }

    /// Run a closure with the network state
    pub fn with_network_state<F, R>(peer_network: &mut PeerNetwork, closure: F) -> Result<R, net_error>
    where
        F: FnOnce(&mut PeerNetwork, &mut NetworkState) -> Result<R, net_error>
    {
        let mut net = peer_network.network.take();
        let res = match net {
            Some(ref mut network_state) => {
                closure(peer_network, network_state)
            },
            None => {
                return Err(net_error::NotConnected);
            }
        };
        peer_network.network = net;
        res
    }
    
    /// Create a network handle for another thread to use to communicate with remote peers
    pub fn new_handle(&mut self, bufsz: usize) -> NetworkHandle {
        let (server, client) = NetworkHandleServer::pair(bufsz);
        self.handles.push_back(server);
        client
    }

    /// Saturate a socket with a reply handle
    /// Return (number of bytes sent, whether or not there's more to send)
    fn do_saturate_p2p_socket(convo: &mut ConversationP2P, client_sock: &mut mio::net::TcpStream, handle: &mut ReplyHandleP2P) -> Result<(usize, bool), net_error> {
        let mut total_sent = 0;
        let mut flushed;
        
        loop {
            flushed = handle.try_flush()?;
            let send_res = convo.send(client_sock);
            match send_res {
                Err(e) => {
                    debug!("Failed to send data to socket {:?}: {:?}", client_sock, &e);
                    return Err(e);
                },
                Ok(sz) => {
                    total_sent += sz;
                    if sz == 0 {
                        break;
                    }
                }
            }
        }
        
        Ok((total_sent, flushed))
    }


    /// Saturate a socket with a reply handle.
    /// Return (number of bytes sent, whether or not there's more to send)
    pub fn saturate_p2p_socket(&mut self, event_id: usize, handle: &mut ReplyHandleP2P) -> Result<(usize, bool), net_error> {
        let convo_opt = self.peers.get_mut(&event_id);
        if convo_opt.is_none() {
            info!("No open socket for {}", event_id);
            return Err(net_error::PeerNotConnected);
        }
        
        let socket_opt = self.sockets.get_mut(&event_id);
        if socket_opt.is_none() {
            info!("No open socket for {}", event_id);
            return Err(net_error::PeerNotConnected);
        }
        
        let convo = convo_opt.unwrap();
        let client_sock = socket_opt.unwrap();

        PeerNetwork::do_saturate_p2p_socket(convo, client_sock, handle)
    }

    /// Send a message to a peer.
    /// Non-blocking -- caller has to call .try_flush() or .flush() on the resulting handle to make sure the data is
    /// actually sent.
    pub fn send_message(&mut self, neighbor_key: &NeighborKey, message: StacksMessage, ttl: u64) -> Result<ReplyHandleP2P, net_error> {
        let event_id_opt = self.events.get(&neighbor_key);
        if event_id_opt.is_none() {
            info!("Not connected to {:?}", &neighbor_key);
            return Err(net_error::NoSuchNeighbor);
        }

        let event_id = *(event_id_opt.unwrap());
        let convo_opt = self.peers.get_mut(&event_id);
        if convo_opt.is_none() {
            info!("No ongoing conversation with {:?}", &neighbor_key);
            return Err(net_error::PeerNotConnected);
        }

        let convo = convo_opt.unwrap();

        let mut rh = convo.send_signed_request(message, ttl)?;
        self.saturate_p2p_socket(event_id, &mut rh)?;
        
        // caller must send the remainder
        Ok(rh)
    }

    fn add_relay_handle(&mut self, event_id: usize, relay_handle: ReplyHandleP2P) -> () {
        if let Some(handle_list) = self.relay_handles.get_mut(&event_id) {
            handle_list.push_back(relay_handle);
        }
        else {
            let mut handle_list = VecDeque::new();
            handle_list.push_back(relay_handle);
            self.relay_handles.insert(event_id, handle_list);
        }
    }

    /// Relay a signed message to a peer.
    /// The peer network will take care of sending the data; no need to deal with a reply handle.
    /// Called from _within_ the p2p thread.
    pub fn relay_signed_message(&mut self, neighbor_key: &NeighborKey, message: StacksMessage) -> Result<(), net_error> {
        let event_id = {
            let event_id_opt = self.events.get(&neighbor_key);
            if event_id_opt.is_none() {
                info!("Not connected to {:?}", &neighbor_key);
                return Err(net_error::NoSuchNeighbor);
            }

            *(event_id_opt.unwrap())
        };

        let convo_opt = self.peers.get_mut(&event_id);
        if convo_opt.is_none() {
            info!("No ongoing conversation with {:?}", &neighbor_key);
            return Err(net_error::PeerNotConnected);
        }

        let convo = convo_opt.unwrap();
        let mut reply_handle = convo.relay_signed_message(message)?;

        let (num_sent, flushed) = self.saturate_p2p_socket(event_id, &mut reply_handle)?;
        if num_sent > 0 || !flushed {
            // keep trying to send
            self.add_relay_handle(event_id, reply_handle);
        }
        Ok(())
    }

    /// Broadcast a message to a list of neighbors
    pub fn broadcast_message(&mut self, mut neighbor_keys: Vec<NeighborKey>, relay_hints: Vec<RelayData>, message_payload: StacksMessageType) -> () {
        debug!("{:?}: Will broadcast '{}' to up to {} neighbors", &self.local_peer, message_payload.get_message_name(), neighbor_keys.len());
        for nk in neighbor_keys.drain(..) {
            if let Some(event_id) = self.events.get(&nk) {
                let event_id = *event_id;
                if let Some(convo) = self.peers.get_mut(&event_id) {
                    match convo.sign_and_forward(&self.local_peer, &self.chain_view, relay_hints.clone(), message_payload.clone()) {
                        Ok(rh) => {
                            debug!("{:?}: Broadcasted '{}' to {:?}", &self.local_peer, message_payload.get_message_name(), &nk);
                            self.add_relay_handle(event_id, rh);
                        },
                        Err(e) => {
                            warn!("{:?}: Failed to broadcast message to {:?}: {:?}", &self.local_peer, nk, &e);
                        }
                    }
                }
            }
        }
        debug!("{:?}: Done broadcasting '{}", &self.local_peer, message_payload.get_message_name());
    }

    /// Count how many outbound conversations are going on 
    pub fn count_outbound_conversations(peers: &PeerMap) -> u64 {
        let mut ret = 0;
        for (_, convo) in peers.iter() {
            if convo.stats.outbound {
                ret += 1;
            }
        }
        ret
    }

    /// Count how many connections to a given IP address we have 
    pub fn count_ip_connections(ipaddr: &SocketAddr, sockets: &HashMap<usize, mio_net::TcpStream>) -> u64 {
        let mut ret = 0;
        for (_, socket) in sockets.iter() {
            match socket.peer_addr() {
                Ok(addr) => {
                    if addr.ip() == ipaddr.ip() {
                        ret += 1;
                    }
                },
                Err(_) => {}
            };
        }
        ret
    }
    
    /// Connect to a peer.
    /// Idempotent -- will not re-connect if already connected.
    /// Fails if the peer is denied.
    pub fn connect_peer(&mut self, neighbor: &NeighborKey) -> Result<usize, net_error> {
        self.connect_peer_deny_checks(neighbor, true)
    }

    /// Connect to a peer, optionally checking our deny information.
    /// Idempotent -- will not re-connect if already connected.
    /// Fails if the peer is denied.
    fn connect_peer_deny_checks(&mut self, neighbor: &NeighborKey, check_denied: bool) -> Result<usize, net_error> {
        if check_denied {
            // don't talk to our bind address
            if self.is_bound(neighbor) {
                debug!("{:?}: do not connect to myself at {:?}", &self.local_peer, neighbor);
                return Err(net_error::Denied);
            }

            // don't talk if denied
            if PeerDB::is_peer_denied(&self.peerdb.conn(), neighbor.network_id, &neighbor.addrbytes, neighbor.port)? {
                debug!("{:?}: Neighbor {:?} is denied; will not connect", &self.local_peer, neighbor);
                return Err(net_error::Denied);
            }
        }

        // already connected?
        if let Some(event_id) = self.get_event_id(&neighbor) {
            test_debug!("{:?}: already connected to {:?} as event {}", &self.local_peer, &neighbor, event_id);
            return Ok(event_id);
        }

        let next_event_id = match self.network {
            None => {
                test_debug!("{:?}: network not connected", &self.local_peer);
                return Err(net_error::NotConnected);
            },
            Some(ref mut network) => {
                let sock = NetworkState::connect(&neighbor.addrbytes.to_socketaddr(neighbor.port))?;
                let hint_event_id = network.next_event_id()?;
                let registered_event_id = network.register(self.p2p_network_handle, hint_event_id, &sock)?;

                self.connecting.insert(registered_event_id, (sock, true, get_epoch_time_secs()));
                registered_event_id
            }
        };

        Ok(next_event_id)
    }

    /// Sample the available connections to broadcast on.
    /// Up to MAX_BROADCAST_OUTBOUND_PEERS outbound connections will be used.
    /// Up to MAX_BROADCAST_INBOUND_PEERS inbound connections will be used.
    /// The outbound will be sampled according to their AS distribution
    /// The inbound will be sampled according to how rarely they send duplicate messages
    fn sample_broadcast_peers<R: RelayPayload>(&self, relay_hints: &Vec<RelayData>, payload: &R) -> Result<Vec<NeighborKey>, net_error> {
        // coalesce
        let mut outbound_neighbors = vec![];
        let mut inbound_neighbors = vec![];

        for (_, convo) in self.peers.iter() {
            let nk = convo.to_neighbor_key();
            if convo.is_outbound() {
                outbound_neighbors.push(nk);
            }
            else {
                inbound_neighbors.push(nk);
            }
        }

        let mut outbound_dist = self.relayer_stats.get_outbound_relay_rankings(&self.peerdb, &outbound_neighbors)?;
        let mut inbound_dist = self.relayer_stats.get_inbound_relay_rankings(&inbound_neighbors, payload, RELAY_DUPLICATE_INFERENCE_WARMUP);

        // don't send a message to anyone who sent this message to us
        for (_, convo) in self.peers.iter() {
            if let Some(pubkey) = convo.ref_public_key() {
                let pubkey_hash = Hash160::from_data(&pubkey.to_bytes());
                for rhint in relay_hints {
                    if rhint.peer.public_key_hash == pubkey_hash {
                        // don't send to this peer
                        let nk = convo.to_neighbor_key();

                        test_debug!("{:?}: Do not forward {} to {:?}, since it already saw this message", &self.local_peer, payload.get_id(), &nk);
                        outbound_dist.remove(&nk);
                        inbound_dist.remove(&nk);
                    }
                }
            }
        }
        
        debug!("Inbound recipient distribution: {:?}", &inbound_dist);
        debug!("Outbound recipient distribution: {:?}", &outbound_dist);

        let mut outbound_sample = RelayerStats::sample_neighbors(outbound_dist, MAX_BROADCAST_OUTBOUND_RECEIVERS);
        let mut inbound_sample = RelayerStats::sample_neighbors(inbound_dist, MAX_BROADCAST_INBOUND_RECEIVERS);

        debug!("Inbound recipients: {:?}", &inbound_sample);
        debug!("Outbound recipients: {:?}", &outbound_sample);

        outbound_sample.append(&mut inbound_sample);
        Ok(outbound_sample)
    }

    /// Dispatch a single request from another thread.
    fn dispatch_request(&mut self, request: NetworkRequest) -> Result<(), net_error> {
        match request {
            NetworkRequest::Ban(neighbor_keys) => {
                for neighbor_key in neighbor_keys.iter() {
                    test_debug!("Request to ban {:?}", neighbor_key);
                    match self.events.get(neighbor_key) {
                        Some(event_id) => {
                            test_debug!("Will ban {:?} (event {})", neighbor_key, event_id);
                            self.bans.insert(*event_id);
                        },
                        None => {}
                    }
                }
                Ok(())
            },
            NetworkRequest::AdvertizeBlocks(blocks) => {
                if !(cfg!(test) && self.connection_opts.disable_block_advertisement) {
                    self.advertize_blocks(blocks)?;
                }
                Ok(())
            }
            NetworkRequest::AdvertizeMicroblocks(mblocks) => {
                if !(cfg!(test) && self.connection_opts.disable_block_advertisement) {
                    self.advertize_microblocks(mblocks)?;
                }
                Ok(())
            }
            NetworkRequest::Relay(neighbor_key, msg) => {
                self.relay_signed_message(&neighbor_key, msg)
                    .and_then(|_| Ok(()))
            },
            NetworkRequest::Broadcast(relay_hints, msg) => {
                // pick some neighbors. Note that only some messages can be broadcasted.
                let neighbor_keys = match msg {
                    StacksMessageType::Blocks(ref data) => {
                        // send to each neighbor that needs one
                        let mut all_neighbors = HashSet::new();
                        for (_, block) in data.blocks.iter() {
                            let mut neighbors = self.sample_broadcast_peers(&relay_hints, block)?;
                            for nk in neighbors.drain(..) {
                                all_neighbors.insert(nk);
                            }
                        }
                        Ok(all_neighbors.into_iter().collect())
                    }
                    StacksMessageType::Microblocks(ref data) => {
                        // send to each neighbor that needs at least one
                        let mut all_neighbors = HashSet::new();
                        for mblock in data.microblocks.iter() {
                            let mut neighbors = self.sample_broadcast_peers(&relay_hints, mblock)?;
                            for nk in neighbors.drain(..) {
                                all_neighbors.insert(nk);
                            }
                        }
                        Ok(all_neighbors.into_iter().collect())
                    },
                    StacksMessageType::Transaction(ref data) => self.sample_broadcast_peers(&relay_hints, data),
                    _ => {
                        // not suitable for broadcast
                        return Err(net_error::InvalidMessage);
                    }
                }?;
                self.broadcast_message(neighbor_keys, relay_hints, msg);
                Ok(())
            }
        }
    }

    /// Process any handle requests from other threads.
    /// Returns the number of requests dispatched.
    /// This method does not block.
    fn dispatch_requests(&mut self) {
        let mut to_remove = vec![];
        let mut messages = vec![];
        let mut responses = vec![];

        // receive all in-bound requests
        for i in 0..self.handles.len() {
            match self.handles.get(i) {
                Some(ref handle) => {
                    loop {
                        // drain all inbound requests
                        let inbound_request_res = handle.chan_in.try_recv();
                        match inbound_request_res {
                            Ok(inbound_request) => {
                                messages.push((i, inbound_request));
                            },
                            Err(TryRecvError::Empty) => {
                                // nothing to do
                                break;
                            },
                            Err(TryRecvError::Disconnected) => {
                                // dead; remove
                                to_remove.push(i);
                                break;
                            }
                        }
                    }
                },
                None => {}
            }
        }

        // dispatch all in-bound requests from waiting threads
        for (i, inbound_request) in messages {
            let inbound_str = format!("{:?}", &inbound_request);
            let dispatch_res = self.dispatch_request(inbound_request);
            responses.push((i, inbound_str, dispatch_res));
        }

        for (i, inbound_str, dispatch_res) in responses {
            if let Err(e) = dispatch_res {
                warn!("P2P client channel {}: request '{:?}' failed: '{:?}'", i, &inbound_str, &e);
            }
        }

        // clear out dead handles
        to_remove.reverse();
        for i in to_remove {
            self.handles.remove(i);
        }
    }

    /// Process ban requests.  Update the deny in the peer database.  Return the vec of event IDs to disconnect from.
    fn process_bans(&mut self) -> Result<Vec<usize>, net_error> {
        if cfg!(test) && self.connection_opts.disable_network_bans {
             return Ok(vec![]);
        }

        let mut tx = self.peerdb.tx_begin()?;
        let mut disconnect = vec![];
        for event_id in self.bans.drain() {
            let (neighbor_key, neighbor_info_opt) = match self.peers.get(&event_id) {
                Some(convo) => {
                    match Neighbor::from_conversation(&tx, convo)? {
                        Some(neighbor) => {
                            if neighbor.is_allowed() {
                                debug!("Misbehaving neighbor {:?} is allowed; will not punish", &neighbor.addr);
                                continue;
                            }
                            (convo.to_neighbor_key(), Some(neighbor))
                        }
                        None => {
                            test_debug!("No such neighbor in peer DB, but will ban nevertheless: {:?}", convo.to_neighbor_key());
                            (convo.to_neighbor_key(), None)
                        }
                    }
                },
                None => {
                    continue;
                }
            };

            disconnect.push(event_id);

            let now = get_epoch_time_secs();
            let penalty = 
                if let Some(neighbor_info) = neighbor_info_opt {
                    if neighbor_info.denied < 0 || (neighbor_info.denied as u64) < now + DENY_MIN_BAN_DURATION {
                        now + DENY_MIN_BAN_DURATION
                    }
                    else {
                        // already recently penalized; make ban length grow exponentially
                        if ((neighbor_info.denied as u64) - now) * 2 < DENY_BAN_DURATION {
                            now + ((neighbor_info.denied as u64) - now) * 2
                        }
                        else {
                            now + DENY_BAN_DURATION
                        }
                    }
                }
                else {
                    now + DENY_BAN_DURATION
                };

            debug!("Ban peer {:?} for {}s until {}", &neighbor_key, penalty - now, penalty);

            PeerDB::set_deny_peer(&mut tx, neighbor_key.network_id, &neighbor_key.addrbytes, neighbor_key.port, penalty)?;
        }

        tx.commit()?;
        Ok(disconnect)
    }

    /// Get the neighbor if we know of it and it's public key is unexpired.
    fn lookup_peer(&self, cur_block_height: u64, peer_addr: &SocketAddr) -> Result<Option<Neighbor>, net_error> {
        let conn = self.peerdb.conn();
        let addrbytes = PeerAddress::from_socketaddr(peer_addr);
        let neighbor_opt = PeerDB::get_peer(conn, self.local_peer.network_id, &addrbytes, peer_addr.port())
            .map_err(net_error::DBError)?;

        match neighbor_opt {
            None => Ok(None),
            Some(neighbor) => {
                if neighbor.expire_block < cur_block_height {
                    Ok(Some(neighbor))
                }
                else {
                    Ok(None)
                }
            }
        }
    }

    /// Get number of inbound connections we're servicing
    pub fn num_peers(&self) -> usize {
        self.sockets.len()
    }

    /// Is an event ID connecting?
    pub fn is_connecting(&self, event_id: usize) -> bool {
        self.connecting.contains_key(&event_id)
    }

    /// Is this neighbor key the same as the one that represents our p2p bind address?
    fn is_bound(&self, neighbor_key: &NeighborKey) -> bool {
        self.bind_nk.network_id == neighbor_key.network_id && self.bind_nk.addrbytes == neighbor_key.addrbytes && self.bind_nk.port == neighbor_key.port
    }

    /// Check to see if we can register the given socket
    /// * we can't have registered this neighbor already
    /// * if this is inbound, we can't add more than self.num_clients
    fn can_register_peer(&mut self, event_id: usize, neighbor_key: &NeighborKey, outbound: bool) -> Result<(), net_error> {
        if !(!self.public_ip_confirmed && self.public_ip_self_event_id == event_id) {
            // (this is _not_ us connecting to ourselves)
            // don't talk to our bind address 
            if self.is_bound(neighbor_key) {
                debug!("{:?}: do not register myself at {:?}", &self.local_peer, neighbor_key);
                return Err(net_error::Denied);
            }

            // denied?
            if PeerDB::is_peer_denied(&self.peerdb.conn(), neighbor_key.network_id, &neighbor_key.addrbytes, neighbor_key.port)? {
                info!("{:?}: Peer {:?} is denied; dropping", &self.local_peer, neighbor_key);
                return Err(net_error::Denied);
            }
        }
        else {
            debug!("{:?}: skip deny check for verifying my IP address (event {})", &self.local_peer, event_id);
        }
        
        // already connected?
        if let Some(event_id) = self.get_event_id(&neighbor_key) {
            test_debug!("{:?}: already connected to {:?}", &self.local_peer, &neighbor_key);
            return Err(net_error::AlreadyConnected(event_id));
        }

        // consider rate-limits on in-bound peers
        let num_outbound = PeerNetwork::count_outbound_conversations(&self.peers);
        if !outbound && (self.peers.len() as u64) - num_outbound >= self.connection_opts.num_clients {
            // too many inbounds 
            info!("{:?}: Too many inbound connections", &self.local_peer);
            return Err(net_error::TooManyPeers);
        }

        Ok(())
    }
    
    /// Low-level method to register a socket/event pair on the p2p network interface.
    /// Call only once the socket is registered with the underlying poller (so we can detect
    /// connection events).  If this method fails for some reason, it'll de-register the socket
    /// from the poller.
    /// outbound is true if we are the peer that started the connection (otherwise it's false)
    fn register_peer(&mut self, event_id: usize, socket: mio_net::TcpStream, outbound: bool) -> Result<(), net_error> {
        let client_addr = match socket.peer_addr() {
            Ok(addr) => addr,
            Err(e) => {
                warn!("Failed to get peer address of {:?}: {:?}", &socket, &e);
                self.deregister_socket(event_id, socket);
                return Err(net_error::SocketError);
            }
        };

        let neighbor_opt = match self.lookup_peer(self.chain_view.burn_block_height, &client_addr) {
            Ok(neighbor_opt) => neighbor_opt,
            Err(e) => {
                debug!("Failed to look up peer {}: {:?}", client_addr, &e);
                self.deregister_socket(event_id, socket);
                return Err(e);
            }
        };

        // NOTE: the neighbor_key will have the same network_id as the remote peer, and the same
        // major version number in the peer_version.  The chat logic won't accept any messages for
        // which this is not true.  Comparison and Hashing are defined for neighbor keys
        // appropriately, so it's okay for us to use self.peer_version and
        // self.local_peer.network_id here for the remote peer's neighbor key.
        let (pubkey_opt, neighbor_key) = match neighbor_opt {
            Some(neighbor) => (Some(neighbor.public_key.clone()), neighbor.addr),
            None => (None, NeighborKey::from_socketaddr(self.peer_version, self.local_peer.network_id, &client_addr))
        };

        match self.can_register_peer(event_id, &neighbor_key, outbound) {
            Ok(_) => {},
            Err(e) => {
                debug!("Could not register peer {:?}: {:?}", &neighbor_key, &e);
                self.deregister_socket(event_id, socket);
                return Err(e);
            }
        }

        let mut new_convo = ConversationP2P::new(self.local_peer.network_id, self.peer_version, &self.burnchain, &client_addr, &self.connection_opts, outbound, event_id);
        new_convo.set_public_key(pubkey_opt);
        
        debug!("{:?}: Registered {} as event {} ({:?},outbound={})", &self.local_peer, &client_addr, event_id, &neighbor_key, outbound);

        assert!(!self.sockets.contains_key(&event_id));
        assert!(!self.peers.contains_key(&event_id));

        self.sockets.insert(event_id, socket);
        self.peers.insert(event_id, new_convo);
        self.events.insert(neighbor_key, event_id);

        Ok(())
    }

    /// Are we connected to a remote host already?
    pub fn is_registered(&self, neighbor_key: &NeighborKey) -> bool {
        self.events.contains_key(&neighbor_key)
    }
    
    /// Get the event ID associated with a neighbor key 
    pub fn get_event_id(&self, neighbor_key: &NeighborKey) -> Option<usize> {
        let event_id_opt = match self.events.get(neighbor_key) {
             Some(eid) => Some(*eid),
             None => None
        };
        event_id_opt
    }

    /// Get a ref to a conversation given a neighbor key
    pub fn get_convo(&self, neighbor_key: &NeighborKey) -> Option<&ConversationP2P> {
        match self.events.get(neighbor_key) {
            Some(event_id) => self.peers.get(event_id),
            None => None
        }
    }

    /// Deregister a socket from our p2p network instance.
    fn deregister_socket(&mut self, event_id: usize, socket: mio_net::TcpStream) -> () {
        match self.network {
            Some(ref mut network) => {
                let _ = network.deregister(event_id, &socket);
            },
            None => {}
        }
    }

    /// Deregister a socket/event pair
    pub fn deregister_peer(&mut self, event_id: usize) -> () {
        test_debug!("{:?}: Disconnect event {}", &self.local_peer, event_id);
        if self.peers.contains_key(&event_id) {
            self.peers.remove(&event_id);
        }

        let mut to_remove : Vec<NeighborKey> = vec![];
        for (neighbor_key, ev_id) in self.events.iter() {
            if *ev_id == event_id {
                to_remove.push(neighbor_key.clone());
            }
        }
        for nk in to_remove {
            // remove events
            self.events.remove(&nk);
        }

        let mut to_remove : Vec<usize> = vec![];
        match self.network {
            None => {},
            Some(ref mut network) => {
                match self.sockets.get_mut(&event_id) {
                    None => {},
                    Some(ref sock) => {
                        let _ = network.deregister(event_id, sock);
                        to_remove.push(event_id);   // force it to close anyway
                    }
                }
            }
        }

        for event_id in to_remove {
            // remove socket
            self.sockets.remove(&event_id);
            self.connecting.remove(&event_id);
            self.relay_handles.remove(&event_id);
        }
    }

    /// Deregister by neighbor key 
    pub fn deregister_neighbor(&mut self, neighbor_key: &NeighborKey) -> () {
        debug!("Disconnect from {:?}", neighbor_key);
        let event_id = match self.events.get(&neighbor_key) {
            None => {
                return;
            }
            Some(eid) => *eid
        };
        self.deregister_peer(event_id);
    }

    /// Deregister and ban a neighbor
    pub fn deregister_and_ban_neighbor(&mut self, neighbor: &NeighborKey) -> () {
        debug!("Disconnect from and ban {:?}", neighbor);
        match self.events.get(neighbor) {
            Some(event_id) => {
                self.bans.insert(*event_id);
            }
            None => {}
        }
        
        // erase local state too
        match self.inv_state {
            Some(ref mut inv_state) => {
                inv_state.del_peer(neighbor);
            },
            None => {}
        }

        self.relayer_stats.process_neighbor_ban(neighbor);

        self.deregister_neighbor(neighbor);
    }

    /// Sign a p2p message to be sent to a particular peer we're having a conversation with.
    /// The peer must already be connected.
    pub fn sign_for_peer(&mut self, peer_key: &NeighborKey, message_payload: StacksMessageType) -> Result<StacksMessage, net_error> {
        match self.events.get(&peer_key) {
            None => {
                // not connected
                info!("Could not sign for peer {:?}: not connected", peer_key);
                Err(net_error::PeerNotConnected)
            },
            Some(event_id) => {
                match self.peers.get_mut(&event_id) {
                    None => {
                        Err(net_error::PeerNotConnected)
                    },
                    Some(ref mut convo) => {
                        convo.sign_message(&self.chain_view, &self.local_peer.private_key, message_payload)
                    }
                }
            }
        }
    }
    
    /// Process new inbound TCP connections we just accepted.
    /// Returns the event IDs of sockets we need to register
    fn process_new_sockets(&mut self, poll_state: &mut NetworkPollState) -> Result<Vec<usize>, net_error> {
        if self.network.is_none() {
            test_debug!("{:?}: network not connected", &self.local_peer);
            return Err(net_error::NotConnected);
        }

        let mut registered = vec![];

        for (hint_event_id, client_sock) in poll_state.new.drain() {
            let event_id = match self.network {
                Some(ref mut network) => {
                    // add to poller
                    let event_id = match network.register(self.p2p_network_handle, hint_event_id, &client_sock) {
                        Ok(event_id) => event_id,
                        Err(e) => {
                            warn!("Failed to register {:?}: {:?}", &client_sock, &e);
                            continue;
                        }
                    };
            
                    // event ID already used?
                    if self.peers.contains_key(&event_id) {
                        warn!("Already have an event {}: {:?}", event_id, self.peers.get(&event_id));
                        let _ = network.deregister(event_id, &client_sock);
                        continue;
                    }

                    event_id
                },
                None => {
                    test_debug!("{:?}: network not connected", &self.local_peer);
                    return Err(net_error::NotConnected);
                }
            };

            // start tracking it
            if let Err(_e) = self.register_peer(event_id, client_sock, false) {
                // NOTE: register_peer will deregister the socket for us
                continue;
            }
            registered.push(event_id);
        }
    
        Ok(registered)
    }

    /// Process network traffic on a p2p conversation.
    /// Returns list of unhandled messages, and whether or not the convo is still alive.
    fn process_p2p_conversation(local_peer: &LocalPeer, peerdb: &mut PeerDB, sortdb: &SortitionDB, chainstate: &mut StacksChainState, chain_view: &BurnchainView, 
                                event_id: usize, client_sock: &mut mio_net::TcpStream, convo: &mut ConversationP2P) -> Result<(Vec<StacksMessage>, bool), net_error> {
        // get incoming bytes and update the state of this conversation.
        let mut convo_dead = false;
        let recv_res = convo.recv(client_sock);
        match recv_res {
            Err(e) => {
                match e {
                    net_error::PermanentlyDrained => {
                        // socket got closed, but we might still have pending unsolicited messages
                        debug!("{:?}: Remote peer disconnected event {} (socket {:?})", local_peer, event_id, &client_sock);
                    },
                    _ => {
                        debug!("{:?}: Failed to receive data on event {} (socket {:?}): {:?}", local_peer, event_id, &client_sock, &e);
                    }
                }
                convo_dead = true;
            },
            Ok(_) => {}
        }
    
        // react to inbound messages -- do we need to send something out, or fulfill requests
        // to other threads?  Try to chat even if the recv() failed, since we'll want to at
        // least drain the conversation inbox.
        let chat_res = convo.chat(local_peer, peerdb, sortdb, chainstate, chain_view);
        let unhandled = match chat_res {
            Err(e) => {
                debug!("Failed to converse on event {} (socket {:?}): {:?}", event_id, &client_sock, &e);
                convo_dead = true;
                vec![]
            },
            Ok(unhandled_messages) => unhandled_messages
        };

        if !convo_dead {
            // (continue) sending out data in this conversation, if the conversation is still
            // ongoing
            let send_res = convo.send(client_sock);
            match send_res {
                Err(e) => {
                    debug!("Failed to send data to event {} (socket {:?}): {:?}", event_id, &client_sock, &e);
                    convo_dead = true;
                },
                Ok(_) => {}
            }
        }

        Ok((unhandled, !convo_dead))
    }

    /// Process any newly-connecting sockets
    fn process_connecting_sockets(&mut self, poll_state: &mut NetworkPollState) -> () {
        for event_id in poll_state.ready.iter() {
            if self.connecting.contains_key(event_id) {
                let (socket, outbound, _) = self.connecting.remove(event_id).unwrap();
                debug!("{:?}: Connected event {}: {:?} (outbound={})", &self.local_peer, event_id, &socket, outbound);

                let sock_str = format!("{:?}", &socket);
                if let Err(_e) = self.register_peer(*event_id, socket, outbound) {
                    debug!("{:?}: Failed to register connected event {} ({}): {:?}", &self.local_peer, event_id, sock_str, &_e);
                }
            }
        }
    }

    /// Process sockets that are ready, but specifically inbound or outbound only.
    /// Advance the state of all such conversations with remote peers.
    /// Return the list of events that correspond to failed conversations, as well as the set of
    /// unhandled messages grouped by event_id.
    fn process_ready_sockets(&mut self, sortdb: &SortitionDB, chainstate: &mut StacksChainState, poll_state: &mut NetworkPollState) -> (Vec<usize>, HashMap<usize, Vec<StacksMessage>>) {
        let mut to_remove = vec![];
        let mut unhandled : HashMap<usize, Vec<StacksMessage>> = HashMap::new();

        for event_id in &poll_state.ready {
            if !self.sockets.contains_key(&event_id) {
                test_debug!("Rogue socket event {}", event_id);
                to_remove.push(*event_id);
                continue;
            }

            let client_sock_opt = self.sockets.get_mut(&event_id);
            if client_sock_opt.is_none() {
                test_debug!("No such socket event {}", event_id);
                to_remove.push(*event_id);
                continue;
            }
            let client_sock = client_sock_opt.unwrap();

            match self.peers.get_mut(event_id) {
                Some(ref mut convo) => {
                    // activity on a p2p socket
                    debug!("{:?}: process p2p data from {:?}", &self.local_peer, convo);
                    let mut convo_unhandled = match PeerNetwork::process_p2p_conversation(&self.local_peer, &mut self.peerdb, sortdb, chainstate, &self.chain_view, *event_id, client_sock, convo) {
                        Ok((convo_unhandled, alive)) => {
                            if !alive {
                                to_remove.push(*event_id);
                            }
                            convo_unhandled
                        },
                        Err(_e) => {
                            to_remove.push(*event_id);
                            continue;
                        }
                    };

                    // forward along unhandled messages from this peer
                    if unhandled.contains_key(event_id) {
                        unhandled.get_mut(event_id).unwrap().append(&mut convo_unhandled);
                    }
                    else {
                        unhandled.insert(*event_id, convo_unhandled);
                    }
                },
                None => {
                    warn!("Rogue event {} for socket {:?}", event_id, &client_sock);
                    to_remove.push(*event_id);
                }
            }
        }

        (to_remove, unhandled)
    }

    /// Get stats for a neighbor 
    pub fn get_neighbor_stats(&self, nk: &NeighborKey) -> Option<NeighborStats> {
        match self.events.get(&nk) {
            None => {
                None
            }
            Some(eid) => {
                match self.peers.get(&eid) {
                    None => {
                        None
                    },
                    Some(ref convo) => {
                        Some(convo.stats.clone())
                    }
                }
            }
        }
    }

    /// Update peer connections as a result of a peer graph walk.
    /// -- Drop broken connections.
    /// -- Update our frontier.
    /// -- Prune our frontier if it gets too big.
    fn process_neighbor_walk(&mut self, walk_result: NeighborWalkResult) -> () {
        for broken in walk_result.broken_connections.iter() {
            self.deregister_and_ban_neighbor(broken);
        }

        for dead in walk_result.dead_connections.iter() {
            self.deregister_neighbor(dead);
        }

        for replaced in walk_result.replaced_neighbors.iter() {
            self.deregister_neighbor(replaced);
        }

        // store for later
        self.walk_result = walk_result;
    }

    /// Queue up pings to everyone we haven't spoken to in a while to let them know that we're still
    /// alive.
    pub fn queue_ping_heartbeats(&mut self) -> () {
        let now = get_epoch_time_secs();
        let mut relay_handles = HashMap::new();
        for (_, convo) in self.peers.iter_mut() {
            if convo.is_outbound() && convo.is_authenticated() && convo.stats.last_handshake_time > 0 && convo.stats.last_send_time + (convo.heartbeat as u64) + self.connection_opts.neighbor_request_timeout < now {
                // haven't talked to this neighbor in a while
                let payload = StacksMessageType::Ping(PingData::new());
                let ping_res = convo.sign_message(&self.chain_view, &self.local_peer.private_key, payload);

                match ping_res {
                    Ok(ping) => {
                        // NOTE: use "relay" here because we don't intend to wait for a reply
                        // (the conversational logic will update our measure of this node's uptime)
                        match convo.relay_signed_message(ping) {
                            Ok(handle) => {
                                relay_handles.insert(convo.conn_id, handle);
                            },
                            Err(_e) => {
                                debug!("Outbox to {:?} is full; cannot ping", &convo);
                            }
                        };
                    },
                    Err(e) => {
                        debug!("Unable to create ping message for {:?}: {:?}", &convo, &e);
                    }
                };
            }
        }
        for (event_id, handle) in relay_handles.drain() {
            self.add_relay_handle(event_id, handle);
        }
    }

    /// Remove unresponsive peers
    fn disconnect_unresponsive(&mut self) -> () {
        let now = get_epoch_time_secs();
        let mut to_remove = vec![];
        for (event_id, (socket, _, ts)) in self.connecting.iter() {
            if ts + self.connection_opts.connect_timeout < now {
                debug!("{:?}: Disconnect unresponsive connecting peer {:?}: timed out after {} ({} < {})s", &self.local_peer, socket, self.connection_opts.timeout, ts + self.connection_opts.timeout, now);
                to_remove.push(*event_id);
            }
        }
        
        for (event_id, convo) in self.peers.iter() {
            if convo.is_authenticated() {
                // have handshaked with this remote peer
                if convo.stats.last_contact_time + (convo.peer_heartbeat as u64) + self.connection_opts.neighbor_request_timeout < now {
                    // we haven't heard from this peer in too long a time 
                    debug!("{:?}: Disconnect unresponsive authenticated peer {:?}: {} + {} + {} < {}", &self.local_peer, &convo, convo.stats.last_contact_time, convo.peer_heartbeat, self.connection_opts.neighbor_request_timeout, now);
                    to_remove.push(*event_id);
                }
            }
            else {
                // have not handshaked with this remote peer
                if convo.instantiated + self.connection_opts.handshake_timeout < now {
                    debug!("{:?}: Disconnect unresponsive unauthenticated peer {:?}: {} + {} < {}", &self.local_peer, &convo, convo.instantiated, self.connection_opts.handshake_timeout, now);
                    to_remove.push(*event_id);
                }
            }
        }

        for event_id in to_remove.into_iter() {
            self.deregister_peer(event_id);
        }
    }

    /// Prune inbound and outbound connections if we can 
    fn prune_connections(&mut self) -> () {
        if cfg!(test) && self.connection_opts.disable_network_prune {
             return;
        }

        test_debug!("Prune connections");
        let mut safe : HashSet<usize> = HashSet::new();
        let now = get_epoch_time_secs();

        // don't prune allowed peers 
        for (nk, event_id) in self.events.iter() {
            let neighbor = match PeerDB::get_peer(self.peerdb.conn(), self.local_peer.network_id, &nk.addrbytes, nk.port) {
                Ok(neighbor_opt) => {
                    match neighbor_opt {
                        Some(n) => n,
                        None => {
                            continue;
                        }
                    }
                },
                Err(e) => {
                    debug!("Failed to query {:?}: {:?}", &nk, &e);
                    return;
                }
            };
            if neighbor.allowed < 0 || (neighbor.allowed as u64) > now {
                test_debug!("{:?}: event {} is allowed: {:?}", &self.local_peer, event_id, &nk);
                safe.insert(*event_id);
            }
        }

        // if we're in the middle of a peer walk, then don't prune any outbound connections it established
        // (yet)
        match self.walk {
            Some(ref walk) => {
                for event_id in walk.events.iter() {
                    safe.insert(*event_id);
                }
            },
            None => {}
        };

        self.prune_frontier(&safe);
    }

    /// Regenerate our session private key and re-handshake with everyone.
    fn rekey(&mut self, old_local_peer_opt: Option<&LocalPeer>) -> () {
        assert!(old_local_peer_opt.is_some());
        let _old_local_peer = old_local_peer_opt.unwrap();

        // begin re-key 
        let mut msgs = HashMap::new();
        for (event_id, convo) in self.peers.iter_mut() {
            let nk = convo.to_neighbor_key();
            let handshake_data = HandshakeData::from_local_peer(&self.local_peer);
            let handshake = StacksMessageType::Handshake(handshake_data);

            debug!("{:?}: send re-key Handshake ({:?} --> {:?}) to {:?}", &self.local_peer, 
                   &to_hex(&Secp256k1PublicKey::from_private(&_old_local_peer.private_key).to_bytes_compressed()),
                   &to_hex(&Secp256k1PublicKey::from_private(&self.local_peer.private_key).to_bytes_compressed()), &nk);

            if let Ok(msg) = convo.sign_message(&self.chain_view, &_old_local_peer.private_key, handshake) {
                msgs.insert(nk, (*event_id, msg));
            }
        }

        for (nk, (event_id, msg)) in msgs.drain() {
            match self.send_message(&nk, msg, self.connection_opts.neighbor_request_timeout) {
                Ok(handle) => {
                    self.add_relay_handle(event_id, handle);
                },
                Err(e) => {
                    info!("Failed to rekey to {:?}: {:?}", &nk, &e);
                }
            }
        }
    }

    /// Flush relayed message handles, but don't block.
    /// Drop broken handles.
    /// Return the list of broken conversation event IDs
    fn flush_relay_handles(&mut self) -> Vec<usize> {
        let mut broken = vec![];
        let mut drained = vec![];

        // flush each outgoing conversation 
        for (event_id, handle_list) in self.relay_handles.iter_mut() {
            if handle_list.len() == 0 {
                drained.push(*event_id);
                continue;
            }

            if let (Some(ref mut socket), Some(ref mut convo)) = (self.sockets.get_mut(event_id), self.peers.get_mut(event_id)) {
                while handle_list.len() > 0 {
                    let handle = handle_list.front_mut().unwrap();
                    
                    debug!("Flush relay handle to {:?} ({:?})", socket, convo);
                    let (num_sent, flushed) = match PeerNetwork::do_saturate_p2p_socket(convo, socket, handle) {
                        Ok(x) => x,
                        Err(e) => {
                            info!("Broken connection on event {}: {:?}", event_id, &e);
                            broken.push(*event_id);
                            break;
                        }
                    };

                    if flushed && num_sent == 0 {
                        // message fully sent
                        let handle = handle_list.pop_front().unwrap();
                        
                        // if we're expecting a reply, go consume it out of the underlying
                        // connection
                        if handle.expects_reply() {
                            if let Ok(msg) = handle.try_recv() {
                                debug!("Got back internal message {} seq {}", msg.get_message_name(), msg.request_id());
                            }
                        }
                        continue;
                    }
                    else if num_sent == 0 {
                        // saturated
                        break;
                    }
                }
            }
        }

        for empty in drained.drain(..) {
            self.relay_handles.remove(&empty);
        }

        broken
    }

    /// Update the state of our neighbor walk.
    /// Return true if we finish.
    fn do_network_neighbor_walk(&mut self) -> Result<bool, net_error> {
        if cfg!(test) && self.connection_opts.disable_neighbor_walk {
            test_debug!("neighbor walk is disabled");
            return Ok(true);
        }

        if self.do_prune {
            // wait until we do a prune before we try and find new neighbors
            return Ok(true);
        }

        // walk the peer graph and deal with new/dropped connections
        let (done, walk_result_opt) = self.walk_peer_graph();
        match walk_result_opt {
            None => {},
            Some(walk_result) => {
                // remember to prune later, if need be
                self.do_prune = walk_result.do_prune;
                self.process_neighbor_walk(walk_result);
            }
        }
        Ok(done)
    }

    /// Begin the process of learning this peer's public IP address.
    /// Return Ok(finished with this step)
    /// Return Err(..) on failure
    fn begin_learn_public_ip(&mut self) -> Result<bool, net_error> {
        if self.peers.len() == 0 {
            return Err(net_error::NoSuchNeighbor);
        }

        debug!("{:?}: begin obtaining public IP address", &self.local_peer);

        // pick a random outbound conversation
        let mut idx = thread_rng().gen::<usize>() % self.peers.len();
        for _ in 0..self.peers.len()+1 {
            let event_id = match self.peers.keys().skip(idx).next() {
                Some(eid) => *eid,
                None => {
                    idx = 0;
                    continue;
                }
            };
            idx = (idx + 1) % self.peers.len();

            if let Some(convo) = self.peers.get_mut(&event_id) {
                if !convo.is_authenticated() || !convo.is_outbound() {
                    continue;
                }

                debug!("Ask {:?} for my IP address", &convo);
               
                let nonce = thread_rng().gen::<u32>();
                let natpunch_request = convo.sign_message(&self.chain_view, &self.local_peer.private_key, StacksMessageType::NatPunchRequest(nonce))
                    .map_err(|e| {
                        info!("Failed to sign NAT punch request: {:?}", &e);
                        e
                    })?;

                let mut rh = convo.send_signed_request(natpunch_request, self.connection_opts.timeout)
                    .map_err(|e| {
                        info!("Failed to send NAT punch request: {:?}", &e);
                        e
                    })?;

                self.saturate_p2p_socket(event_id, &mut rh)
                    .map_err(|e| {
                        info!("Failed to saturate NAT punch socket on event {}", &event_id);
                        e
                    })?;

                self.public_ip_reply_handle = Some(rh);
                break;
            }
        }

        if self.public_ip_reply_handle.is_none() {
            // no one to talk to
            debug!("{:?}: Did not find any outbound neighbors to ask for a NAT punch reply", &self.local_peer);
        }
        return Ok(true);
    }


    /// Learn this peer's public IP address.
    /// If it was given to us directly, then we can just skip this step.
    /// Once learned, we'll confirm it by trying to self-connect.
    fn do_learn_public_ip(&mut self) -> Result<bool, net_error> {
        if self.public_ip_reply_handle.is_none() {
            if !self.begin_learn_public_ip()? {
                return Ok(false);
            }

            // began request
            self.public_ip_requested_at = get_epoch_time_secs();
            self.public_ip_retries += 1;
        }

        let rh_opt = self.public_ip_reply_handle.take();
        if let Some(mut rh) = rh_opt {

            debug!("{:?}: waiting for NatPunchReply on event {}", &self.local_peer, rh.get_event_id());

            if let Err(e) = self.saturate_p2p_socket(rh.get_event_id(), &mut rh) {
                info!("{:?}: Failed to query my public IP address: {:?}", &self.local_peer, &e);
                return Err(e);
            }

            match rh.try_send_recv() {
                Ok(message) => match message.payload {
                    StacksMessageType::NatPunchReply(data) => {
                        // peer offers us our public IP address.
                        // confirm it by self-connecting
                        debug!("{:?}: learned that my IP address is supposidly {:?}", &self.local_peer, &data.addrbytes);

                        // prepare for the next step -- confirming the public IP address
                        self.public_ip_confirmed = false;
                        self.public_ip_self_event_id = 0;
                        self.public_ip_address_unconfirmed = Some((data.addrbytes, self.bind_nk.port));
                        return Ok(true);
                    },
                    other_payload => {
                        debug!("{:?}: Got unexpected payload {:?}", &self.local_peer, &other_payload);

                        // restart
                        return Err(net_error::InvalidMessage);
                    }
                }
                Err(req_res) => match req_res {
                    Ok(same_req) => {
                        // try again
                        self.public_ip_reply_handle = Some(same_req);
                        return Ok(false);
                    }
                    Err(e) => {
                        // disconnected
                        debug!("{:?}: Failed to get a NatPunchReply reply: {:?}", &self.local_peer, &e);
                        return Err(e);
                    }
                }
            }
        }

        return Ok(true);
    }

    /// Begin the process of confirming our public IP address
    /// Return Ok(finished preparing to confirm the IP address)
    /// return Err(..) on failure
    fn begin_ping_public_ip(&mut self, public_ip: (PeerAddress, u16)) -> Result<bool, net_error> {
        // ping ourselves using our public IP 
        if self.public_ip_self_event_id == 0 {
            debug!("{:?}: Begin confirming public IP address", &self.local_peer);

            // connect to ourselves
            let public_nk = NeighborKey {
                network_id: self.local_peer.network_id,
                peer_version: self.peer_version, 
                addrbytes: public_ip.0.clone(),
                port: public_ip.1
            };

            let event_id = match self.connect_peer_deny_checks(&public_nk, false) {
                Ok(eid) => eid,
                Err(net_error::AlreadyConnected(eid)) => eid,   // weird if this happens, but you never know
                Err(e) => {
                    info!("Failed to connect to my IP address: {:?}", &e);
                    return Err(e);
                }
            };

            self.public_ip_self_event_id = event_id;

            // call again
            return Ok(false);
        }
        else if self.connecting.contains_key(&self.public_ip_self_event_id) {
            debug!("{:?}: still connecting to myself at {:?}", &self.local_peer, &public_ip);

            // call again
            return Ok(false);
        }
        else if let Some(ref mut convo) = self.peers.get_mut(&self.public_ip_self_event_id) {
            // connected!  Ping myself with another natpunch
            debug!("{:?}: Pinging myself at {:?}", &self.local_peer, &public_ip);

            let nonce = thread_rng().gen::<u32>();
            let ping_natpunch = StacksMessageType::NatPunchRequest(nonce);
            self.public_ip_ping_nonce = nonce;
            let ping_request = convo.sign_message(&self.chain_view, &self.local_peer.private_key, ping_natpunch)
                .map_err(|e| {
                    info!("Failed to sign ping to myself: {:?}", &e);
                    e
                })?;

            let mut rh = convo.send_signed_request(ping_request, self.connection_opts.timeout)
                .map_err(|e| {
                    info!("Failed to send ping to myself: {:?}", &e);
                    e
                })?;

            self.saturate_p2p_socket(self.public_ip_self_event_id, &mut rh)
                .map_err(|e| {
                    info!("Failed to saturate ping socket to myself");
                    e
                })?;

            self.public_ip_reply_handle = Some(rh);
            return Ok(true);
        }
        else {
            // could not connect (timed out or the like)
            info!("{:?}: Failed to connect to myself for IP confirmation", &self.local_peer);
            return Ok(true);
        }
    }
    
    /// Confirm our public IP address if we had to learn it -- try to connect to ourselves via a
    /// ping, and if we succeed, we know that the peer we learned it from was being honest (enough)
    /// Return Ok(done with this step?) on success
    /// Return Err(..) on failure
    fn do_ping_public_ip(&mut self) -> Result<bool, net_error> {
        assert!(self.public_ip_address_unconfirmed.is_some());
        let public_ip = self.public_ip_address_unconfirmed.clone().unwrap();

        if self.public_ip_reply_handle.is_none() {
            if !self.begin_ping_public_ip(public_ip.clone())? {
                return Ok(false);
            }
        }

        let rh_opt = self.public_ip_reply_handle.take();
        if let Some(mut rh) = rh_opt {
            debug!("{:?}: waiting for Pong from myself to confirm my IP address", &self.local_peer);

            if let Err(e) = self.saturate_p2p_socket(rh.get_event_id(), &mut rh) {
                info!("{:?}: Failed to ping myself to confirm my IP address", &self.local_peer);
                return Err(e);
            }

            match rh.try_send_recv() {
                Ok(message) => {
                    // disconnect from myself
                    self.deregister_peer(self.public_ip_self_event_id);
                    self.public_ip_self_event_id = 0;

                    match message.payload {
                        StacksMessageType::NatPunchReply(data) => {
                            if data.nonce == self.public_ip_ping_nonce {
                                // confirmed!
                                info!("{:?}: confirmed my public IP to be {:?}", &self.local_peer, &public_ip);
                                self.public_ip_confirmed = true;
                                self.public_ip_learned_at = get_epoch_time_secs();
                                self.public_ip_retries = 0;

                                // if our IP address changed, then disconnect witih everyone
                                let old_ip = self.local_peer.public_ip_address.clone();
                                self.local_peer.public_ip_address = self.public_ip_address_unconfirmed.clone();

                                if old_ip != self.local_peer.public_ip_address {
                                    let mut all_event_ids = vec![];
                                    for (eid, _) in self.peers.iter() {
                                        all_event_ids.push(*eid);
                                    }
                                    
                                    info!("IP address changed from {:?} to {:?}; closing all connections and re-establishing them", &old_ip, &self.local_peer.public_ip_address);
                                    for eid in all_event_ids.into_iter() {
                                        self.deregister_peer(eid);
                                    }
                                }
                                return Ok(true);
                            }
                            else {
                                // weird response
                                info!("{:?}: invalid Pong response to myself: {} != {}", &self.local_peer, data.nonce, self.public_ip_ping_nonce);
                                return Err(net_error::InvalidMessage);
                            }
                        },
                        other_payload => {
                            info!("{:?}: unexpected response to my public IP confirmation ping: {:?}", &self.local_peer, &other_payload);
                            return Err(net_error::InvalidMessage);
                        }
                    }
                },
                Err(req_res) => match req_res {
                    Ok(same_req) => {
                        // try again
                        self.public_ip_reply_handle = Some(same_req);
                        return Ok(false);
                    }
                    Err(e) => {
                        // disconnected
                        debug!("{:?}: Failed to get a ping reply: {:?}", &self.local_peer, &e);
                        return Err(e);
                    }
                }
            }
            }
        
        return Ok(true);
    }

    /// Do we need to (re)fetch our public IP?
    fn need_public_ip(&mut self) -> bool {
        if !self.public_ip_learned {
            // IP was given, not learned.  nothing to do
            test_debug!("{:?}: IP address was given to us", &self.local_peer);
            return false;
        }
        if self.local_peer.public_ip_address.is_some() && self.public_ip_learned_at + self.connection_opts.public_ip_timeout >= get_epoch_time_secs() {
            // still fresh
            test_debug!("{:?}: learned IP address is still fresh", &self.local_peer);
            return false;
        }
        let throttle_timeout = 
            if self.local_peer.public_ip_address.is_none() {
                self.connection_opts.public_ip_request_timeout
            }
            else {
                self.connection_opts.public_ip_timeout
            };

        if self.public_ip_retries > self.connection_opts.public_ip_max_retries {
            if self.public_ip_requested_at + throttle_timeout >= get_epoch_time_secs() {
                // throttle
                debug!("{:?}: throttle public IP request (max retires {} exceeded) until {}", &self.local_peer, self.public_ip_retries, self.public_ip_requested_at + throttle_timeout);
                return false;
            }
            else {
                // try again
                self.public_ip_retries = 0;
            }
        }

        return true;
    }

    /// Reset all state for querying our public IP address
    fn public_ip_reset(&mut self) {
        debug!("{:?}: reset public IP query state", &self.local_peer);

        if self.public_ip_self_event_id > 0 {
            self.deregister_peer(self.public_ip_self_event_id);
            self.public_ip_self_event_id = 0;
        }

        self.public_ip_self_event_id = 0;
        self.public_ip_reply_handle = None;
        self.public_ip_confirmed = false;
        self.public_ip_address_unconfirmed = None;

        if self.public_ip_learned {
            // will go relearn it if it wasn't given
            self.local_peer.public_ip_address = None;
        }
    }

    /// Learn our publicly-routable IP address
    fn do_get_public_ip(&mut self) -> Result<bool, net_error> {
        if !self.need_public_ip() {
            return Ok(true);
        }
        if self.local_peer.public_ip_address.is_some() && self.public_ip_requested_at + self.connection_opts.public_ip_request_timeout >= get_epoch_time_secs() {
            // throttle
            debug!("{:?}: throttle public IP request query until {}", &self.local_peer, self.public_ip_requested_at + self.connection_opts.public_ip_request_timeout);
            return Ok(true);
        }

        match self.do_learn_public_ip() {
            Ok(b) => {
                if !b {
                    test_debug!("{:?}: try do_learn_public_ip again", &self.local_peer);
                    return Ok(false);
                }
            },
            Err(e) => {
                test_debug!("{:?}: failed to learn public IP: {:?}", &self.local_peer, &e);
                self.public_ip_reset();
                
                match e {
                    net_error::NoSuchNeighbor => {
                        // haven't connected to anyone yet
                        return Ok(true);
                    },
                    _ => {
                        return Err(e);
                    }
                };
            }
        }
        Ok(true)
    }

    /// Confirm our publicly-routable IP address.
    /// Return true once we're done.
    fn do_confirm_public_ip(&mut self) -> Result<bool, net_error> {
        if !self.need_public_ip() {
            return Ok(true);
        }
        if self.public_ip_confirmed {
            // IP already confirmed
            test_debug!("{:?}: learned IP address is confirmed", &self.local_peer);
            return Ok(true);
        }
        if self.public_ip_address_unconfirmed.is_none() {
            // can't do this yet, so skip
            test_debug!("{:?}: unconfirmed IP address is not known yet", &self.local_peer);
            return Ok(true);
        }
        
        // finished request successfully
        self.public_ip_requested_at = get_epoch_time_secs();

        match self.do_ping_public_ip() {
            Ok(b) => {
                if !b {
                    test_debug!("{:?}: try do_confirm_public_ip again", &self.local_peer);
                    return Ok(false);
                }
            },
            Err(e) => {
                test_debug!("{:?}: failed to confirm public IP: {:?}", &self.local_peer, &e);
                self.public_ip_reset();
                
                match e {
                    net_error::NoSuchNeighbor => {
                        // haven't connected to anyone yet
                        return Ok(true);
                    },
                    _ => {
                        return Err(e);
                    }
                };
            }
        }

        // learned and confirmed! clean up
        if self.public_ip_self_event_id > 0 {
            self.deregister_peer(self.public_ip_self_event_id);
        }

        self.public_ip_self_event_id = 0;
        self.public_ip_reply_handle = None;
        Ok(true)
    }

    /// Update the state of our neighbors' block inventories.
    /// Return true if we finish
    fn do_network_inv_sync(&mut self, sortdb: &SortitionDB) -> Result<bool, net_error> {
        if cfg!(test) && self.connection_opts.disable_inv_sync {
            if self.inv_state.is_none() {
                self.init_inv_sync(sortdb);
            }
            
            test_debug!("{:?}: inv sync is disabled", &self.local_peer);
            return Ok(true);
        }

        // synchronize peer block inventories 
        let (done, dead_neighbors, broken_neighbors) = self.sync_peer_block_invs(sortdb)?;
        
        // disconnect and ban broken peers
        for broken in broken_neighbors.into_iter() {
            self.deregister_and_ban_neighbor(&broken);
        }

        // disconnect from dead connections
        for dead in dead_neighbors.into_iter() {
            self.deregister_neighbor(&dead);
        }

        Ok(done)
    }

    /// Download blocks, and add them to our network result.
    fn do_network_block_download(&mut self, sortdb: &SortitionDB, chainstate: &mut StacksChainState, dns_client: &mut DNSClient, network_result: &mut NetworkResult) -> Result<bool, net_error> {
        if cfg!(test) && self.connection_opts.disable_block_download {
            if self.block_downloader.is_none() {
                self.init_block_downloader();
            }
            
            test_debug!("{:?}: block download is disabled", &self.local_peer);
            return Ok(true);
        }

        let (done, mut blocks, mut microblocks, mut broken_http_peers, mut broken_p2p_peers) = self.download_blocks(sortdb, chainstate, dns_client)?;

        network_result.blocks.append(&mut blocks);
        network_result.confirmed_microblocks.append(&mut microblocks);

        if cfg!(test) {
            let mut block_set = HashSet::new();
            let mut microblock_set = HashSet::new();

            for (_, block) in network_result.blocks.iter() {
                if block_set.contains(&block.block_hash()) {
                    test_debug!("Duplicate block {}", block.block_hash());
                }
                block_set.insert(block.block_hash());
            }

            for (_, mblocks) in network_result.confirmed_microblocks.iter() {
                for mblock in mblocks.iter() {
                    if microblock_set.contains(&mblock.block_hash()) {
                        test_debug!("Duplicate microblock {}", mblock.block_hash());
                    }
                    microblock_set.insert(mblock.block_hash());
                }
            }
        }

        let _ = PeerNetwork::with_network_state(self, |ref mut network, ref mut network_state| {
            for dead_event in broken_http_peers.drain(..) {
                debug!("{:?}: De-register broken HTTP connection {}", &network.local_peer, dead_event);
                network.http.deregister_http(network_state, dead_event);
            }
            Ok(())
        });

        for broken_neighbor in broken_p2p_peers.drain(..) {
            debug!("{:?}: De-register broken neighbor {:?}", &self.local_peer, &broken_neighbor);
            self.deregister_and_ban_neighbor(&broken_neighbor);
        }

        Ok(done)
    }

    /// Do the actual work in the state machine.
    /// Return true if we need to prune connections.
    fn do_network_work(&mut self, 
                       sortdb: &SortitionDB, 
                       chainstate: &mut StacksChainState, 
                       mut dns_client_opt: Option<&mut DNSClient>,
                       download_backpressure: bool,
                       network_result: &mut NetworkResult) -> Result<bool, net_error> {

        // do some Actual Work(tm)
        let mut do_prune = false;
        let mut did_cycle = false;

        while !did_cycle {
            debug!("{:?}: network work state is {:?}", &self.local_peer, &self.work_state);
            let cur_state = self.work_state;
            match self.work_state {
                PeerNetworkWorkState::GetPublicIP => {
                    if cfg!(test) && self.connection_opts.disable_natpunch {
                        self.work_state = PeerNetworkWorkState::BlockInvSync;
                    }
                    else {
                        // (re)determine our public IP address
                        match self.do_get_public_ip() {
                            Ok(b) => {
                                if b {
                                    self.work_state = PeerNetworkWorkState::ConfirmPublicIP;
                                }
                            }
                            Err(e) => {
                                info!("Failed to query public IP ({:?}; skipping", &e);
                                self.work_state = PeerNetworkWorkState::BlockInvSync;
                            }
                        }
                    }
                },
                PeerNetworkWorkState::ConfirmPublicIP => {
                    // confirm the public IP address we previously got
                    if cfg!(test) && self.connection_opts.disable_natpunch {
                        self.work_state = PeerNetworkWorkState::BlockInvSync;
                    }
                    else {
                        match self.do_confirm_public_ip() {
                            Ok(b) => {
                                if b {
                                    self.work_state = PeerNetworkWorkState::BlockInvSync;
                                }
                            },
                            Err(e) => {
                                info!("Failed to confirm public IP ({:?}); skipping", &e);
                                self.work_state = PeerNetworkWorkState::BlockInvSync;
                            }
                        }
                    }
                }
                PeerNetworkWorkState::BlockInvSync => {
                    // synchronize peer block inventories 
                    if self.do_network_inv_sync(sortdb)? {
                        if !download_backpressure {
                            // proceed to get blocks, if we're not backpressured
                            self.work_state = PeerNetworkWorkState::BlockDownload;
                        }
                        else {
                            // skip downloads for now
                            self.work_state = PeerNetworkWorkState::Prune;
                        }

                        // pass along hints
                        if let Some(ref inv_sync) = self.inv_state {
                            if inv_sync.learned_data {
                                // tell the downloader to wake up
                                if let Some(ref mut downloader) = self.block_downloader {
                                    downloader.hint_download_rescan();
                                }
                            }
                        }
                    }
                },
                PeerNetworkWorkState::BlockDownload => {
                    // go fetch blocks
                    match dns_client_opt {
                        Some(ref mut dns_client) => {
                            if self.do_network_block_download(sortdb, chainstate, *dns_client, network_result)? {
                                // advance work state
                                self.work_state = PeerNetworkWorkState::Prune;
                            }
                        },
                        None => {
                            // skip this step -- no DNS client available
                            test_debug!("{:?}: no DNS client provided; skipping block download", &self.local_peer);
                            self.work_state = PeerNetworkWorkState::Prune;
                        }
                    }
                },
                PeerNetworkWorkState::Prune => {
                    // did one pass
                    did_cycle = true;

                    // clear out neighbor connections after we finish sending
                    if self.do_prune {
                        do_prune = true;

                        // re-enable neighbor walks
                        self.do_prune = false;
                    }

                    // restart
                    self.work_state = PeerNetworkWorkState::GetPublicIP;
                }
            }

            if self.work_state == cur_state {
                // only break early if we can't make progress
                break;
            }
        }

        Ok(do_prune)
    }

    /// Given an event ID, find the other event ID corresponding
    /// to the same remote peer.  There will be at most two such events 
    /// -- one registered as the inbound connection, and one registered as the
    /// outbound connection.
    fn find_reciprocal_event(&self, event_id: usize) -> Option<usize> {
        let pubkey = match self.peers.get(&event_id) {
            Some(convo) => match convo.get_public_key() {
                Some(pubk) => pubk,
                None => {
                    return None;
                }
            },
            None => {
                return None;
            }
        };

        for (ev_id, convo) in self.peers.iter() {
            if *ev_id == event_id {
                continue;
            }
            if let Some(pubk) = convo.ref_public_key() {
                if *pubk == pubkey {
                    return Some(*ev_id);
                }
            }
        }
        None
    }

    /// Given an event ID, find the NeighborKey that corresponds to the outbound connection we have
    /// to the peer the event ID references.  This checks both the conversation referenced by the
    /// event ID, as well as the reciprocal conversation of the event ID.
    pub fn find_outbound_neighbor(&self, event_id: usize) -> Option<NeighborKey> {
        let (is_authenticated, is_outbound, neighbor_key) = match self.peers.get(&event_id) {
            Some(convo) => (convo.is_authenticated(), convo.is_outbound(), convo.to_neighbor_key()),
            None => {
                test_debug!("No such neighbor event={}", event_id);
                return None;
            }
        };

        let outbound_neighbor_key = 
            if !is_outbound {
                let reciprocal_event_id = match self.find_reciprocal_event(event_id) {
                    Some(re) => re,
                    None => {
                        test_debug!("{:?}: no reciprocal conversation for {:?}", &self.local_peer, &neighbor_key);
                        return None;
                    }
                };

                let (reciprocal_is_authenticated, reciprocal_is_outbound, reciprocal_neighbor_key) = match self.peers.get(&reciprocal_event_id) {
                    Some(convo) => (convo.is_authenticated(), convo.is_outbound(), convo.to_neighbor_key()),
                    None => {
                        test_debug!("{:?}: No reciprocal conversation for {} (event={})", &self.local_peer, &neighbor_key, event_id);
                        return None;
                    }
                };

                if !is_authenticated && !reciprocal_is_authenticated {
                    test_debug!("{:?}: {:?} and {:?} are not authenticated", &self.local_peer, &neighbor_key, &reciprocal_neighbor_key);
                    return None;
                }

                if !is_outbound && !reciprocal_is_outbound {
                    test_debug!("{:?}: {:?} and {:?} are not outbound", &self.local_peer, &neighbor_key, &reciprocal_neighbor_key);
                    return None;
                }

                reciprocal_neighbor_key
            }
            else {
                neighbor_key
            };

        Some(outbound_neighbor_key)
    }

    /// Update a peer's inventory state to indicate that the given block is available.
    /// If updated, return the sortition height of the bit in the inv that was set.
    fn handle_unsolicited_inv_update(&mut self, sortdb: &SortitionDB, event_id: usize, outbound_neighbor_key: &NeighborKey, consensus_hash: &ConsensusHash, burn_header_hash: &BurnchainHeaderHash, microblocks: bool) -> Option<u64> {
        let block_sortition_height = match self.inv_state {
            Some(ref mut inv) => {
                let res = 
                    if microblocks {
                        inv.set_microblocks_available(outbound_neighbor_key, sortdb, consensus_hash, burn_header_hash)
                    }
                    else {
                        inv.set_block_available(outbound_neighbor_key, sortdb, consensus_hash, burn_header_hash)
                    };

                match res {
                    Ok(Some(block_height)) => block_height,
                    Ok(None) => {
                        debug!("Peer {:?} already known to have {} for {}", &outbound_neighbor_key, if microblocks { "streamed microblocks" } else { "blocks" }, burn_header_hash);
                        return None;
                    },
                    Err(net_error::InvalidMessage) => {
                        // punish this peer
                        info!("Peer {:?} sent an invalid update for {}", &outbound_neighbor_key, if microblocks { "streamed microblocks" } else { "blocks" });
                        self.bans.insert(event_id);

                        if let Some(outbound_event_id) = self.events.get(&outbound_neighbor_key) {
                            self.bans.insert(*outbound_event_id);
                        }
                        return None;
                    },
                    Err(e) => {
                        warn!("Failed to update inv state for {:?}: {:?}", &outbound_neighbor_key, &e);
                        return None;
                    }
                }
            },
            None => {
                return None;
            }
        };
        Some(block_sortition_height)
    }

    /// Handle unsolicited BlocksAvailable.
    /// Update our inv for this peer.
    /// Mask errors.
    fn handle_unsolicited_BlocksAvailable(&mut self, sortdb: &SortitionDB, event_id: usize, new_blocks: &BlocksAvailableData) -> () {
        let outbound_neighbor_key = match self.find_outbound_neighbor(event_id) {
            Some(onk) => onk,
            None => {
                return;
            }
        };

        debug!("{:?}: Process BlocksAvailable from {:?} with {} entries", &self.local_peer, outbound_neighbor_key, new_blocks.available.len());

        for (consensus_hash, burn_header_hash) in new_blocks.available.iter() {
            let block_sortition_height = match self.handle_unsolicited_inv_update(sortdb, event_id, &outbound_neighbor_key, consensus_hash, burn_header_hash, false) {
                Some(bsh) => bsh,
                None => {
                    continue;
                }
            };

            // have the downloader request this block if it's new
            match self.block_downloader {
                Some(ref mut downloader) => {
                    downloader.hint_block_sortition_height_available(block_sortition_height);
                },
                None => {}
            }
        }
    }
    
    /// Handle unsolicited MicroblocksAvailable.
    /// Update our inv for this peer.
    /// Mask errors.
    fn handle_unsolicited_MicroblocksAvailable(&mut self, sortdb: &SortitionDB, event_id: usize, new_mblocks: &BlocksAvailableData) -> () {
        let outbound_neighbor_key = match self.find_outbound_neighbor(event_id) {
            Some(onk) => onk,
            None => {
                return;
            }
        };

        debug!("{:?}: Process MicroblocksAvailable from {:?} with {} entries", &self.local_peer, outbound_neighbor_key, new_mblocks.available.len());

        for (consensus_hash, burn_header_hash) in new_mblocks.available.iter() {
            let mblock_sortition_height = match self.handle_unsolicited_inv_update(sortdb, event_id, &outbound_neighbor_key, consensus_hash, burn_header_hash, true) {
                Some(bsh) => bsh,
                None => {
                    continue;
                }
            };

            // have the downloader request this block if it's new
            match self.block_downloader {
                Some(ref mut downloader) => {
                    downloader.hint_microblock_sortition_height_available(mblock_sortition_height);
                },
                None => {}
            }
        }
    }
    
    /// Handle unsolicited BlocksData.
    /// Don't (yet) validate the data, but do update our inv for the peer that sent it.
    /// Mask errors.
    fn handle_unsolicited_BlocksData(&mut self, sortdb: &SortitionDB, event_id: usize, new_blocks: &BlocksData) -> () {
        let outbound_neighbor_key = match self.find_outbound_neighbor(event_id) {
            Some(onk) => onk,
            None => {
                return;
            }
        };

        debug!("{:?}: Process BlocksData from {:?} with {} entries", &self.local_peer, outbound_neighbor_key, new_blocks.blocks.len());

        for (burn_header_hash, block) in new_blocks.blocks.iter() {
            let sortid = SortitionId::stubbed(burn_header_hash);
            let sn = match SortitionDB::get_block_snapshot(&sortdb.conn, &sortid) {
                Ok(Some(sn)) => sn,
                Ok(None) => {
                    // ignore
                    continue;
                },
                Err(e) => {
                    warn!("Failed to query block snapshot for {}: {:?}", burn_header_hash, &e);
                    continue;
                }
            };

            if sn.winning_stacks_block_hash != block.block_hash() {
                info!("Ignoring block {} -- winning block was {} (sortition: {})", block.block_hash(), sn.winning_stacks_block_hash, sn.sortition);
                continue;
            }

            self.handle_unsolicited_inv_update(sortdb, event_id, &outbound_neighbor_key, &sn.consensus_hash, burn_header_hash, false);
        }
    }
    
    /// Handle unsolicited messages propagated up to us from our ongoing ConversationP2Ps.
    /// Return messages that we couldn't handle here, but key them by neighbor, not event.
    /// Drop invalid messages.
    fn handle_unsolicited_messages(&mut self, sortdb: &SortitionDB, mut unsolicited: HashMap<usize, Vec<StacksMessage>>) -> Result<HashMap<NeighborKey, Vec<StacksMessage>>, net_error> {
        let mut unhandled : HashMap<NeighborKey, Vec<StacksMessage>> = HashMap::new();
        for (event_id, messages) in unsolicited.drain() {
            let neighbor_key = match self.peers.get(&event_id) {
                Some(convo) => convo.to_neighbor_key(),
                None => {
                    test_debug!("No such neighbor event={}, dropping message", event_id);
                    continue;
                }
            };
            for message in messages {
                match message.payload {
                    // Update our inv state for this peer, but only do so if we have an
                    // outbound connection to it and it's authenticated (we don't synchronize inv
                    // state with inbound peers).  Since we will have received this message
                    // from an _inbound_ conversation, we need to find the reciprocal _outbound_
                    // conversation and use _that_ conversation's neighbor key to identify
                    // which inventory we need to update. 
                    StacksMessageType::BlocksAvailable(ref new_blocks) => {
                        self.handle_unsolicited_BlocksAvailable(sortdb, event_id, new_blocks);
                    },
                    StacksMessageType::MicroblocksAvailable(ref new_mblocks) => {
                        self.handle_unsolicited_MicroblocksAvailable(sortdb, event_id, new_mblocks);
                    },
                    StacksMessageType::Blocks(ref new_blocks) => {
                        // update inv state for this peer
                        self.handle_unsolicited_BlocksData(sortdb, event_id, new_blocks);
                        
                        // forward to relayer for processing
                        if let Some(msgs) = unhandled.get_mut(&neighbor_key) {
                            msgs.push(message);
                        }
                        else {
                            unhandled.insert(neighbor_key.clone(), vec![message]);
                        }
                    },
                    _ => {
                        if let Some(msgs) = unhandled.get_mut(&neighbor_key) {
                            msgs.push(message);
                        }
                        else {
                            unhandled.insert(neighbor_key.clone(), vec![message]);
                        }
                    }
                }
            }
        }
        Ok(unhandled)
    }
    
    /// Find unauthenticated inbound conversations
    fn find_unauthenticated_inbound_convos(&self) -> Vec<usize> {
        let mut ret = vec![];
        for (event_id, convo) in self.peers.iter() {
            if !convo.is_outbound() && !convo.is_authenticated() {
                ret.push(*event_id);
            }
        }
        ret
    }

    /// Find inbound conversations that have authenticated, given a list of event ids to search
    /// for.  Add them to our network pingbacks
    fn schedule_network_pingbacks(&mut self, event_ids: Vec<usize>) -> Result<(), net_error> {
        if cfg!(test) && self.connection_opts.disable_pingbacks {
            test_debug!("{:?}: pingbacks are disabled for testing", &self.local_peer);
            return Ok(())
        }

        // clear timed-out pingbacks
        let mut to_remove = vec![];
        for (naddr, pingback) in self.walk_pingbacks.iter() {
            if pingback.ts + self.connection_opts.pingback_timeout < get_epoch_time_secs() {
                to_remove.push((*naddr).clone());
            }
        }

        for naddr in to_remove.into_iter() {
            self.walk_pingbacks.remove(&naddr);
        }

        let my_pubkey_hash = Hash160::from_data(&Secp256k1PublicKey::from_private(&self.local_peer.private_key).to_bytes()[..]);

        // add new pingbacks
        for event_id in event_ids.into_iter() {
            if let Some(ref convo) = self.peers.get(&event_id) {
                if !convo.is_outbound() && convo.is_authenticated() {
                    let nk = convo.to_handshake_neighbor_key();
                    let addr = convo.to_handshake_neighbor_address();
                    let pubkey = convo.get_public_key().expect("BUG: convo is authenticated but we have no public key for it");

                    if addr.public_key_hash == my_pubkey_hash {
                        // don't talk to ourselves
                        continue;
                    }

                    let neighbor_opt = PeerDB::get_peer(self.peerdb.conn(), self.local_peer.network_id, &addr.addrbytes, addr.port)
                        .map_err(net_error::DBError)?;
                    
                    if neighbor_opt.is_some() {
                        debug!("{:?}: will not ping back {:?}: already known to us", &self.local_peer, &nk);
                        continue;
                    }

                    debug!("{:?}: will ping back {:?} ({:?}) to see if it's routable from us", &self.local_peer, &nk, convo);
                    self.walk_pingbacks.insert(addr, NeighborPingback { 
                        peer_version: nk.peer_version,
                        network_id: nk.network_id,
                        ts: get_epoch_time_secs(),
                        pubkey: pubkey
                    });

                    if self.walk_pingbacks.len() > MAX_NEIGHBORS_DATA_LEN as usize {
                        // drop one at random 
                        let idx = thread_rng().gen::<usize>() % self.walk_pingbacks.len();
                        let drop_addr = match self.walk_pingbacks.keys().skip(idx).next() {
                            Some(ref addr) => (*addr).clone(),
                            None => {
                                continue;
                            }
                        };

                        debug!("{:?}: drop pingback {:?}", &self.local_peer, drop_addr);
                        self.walk_pingbacks.remove(&drop_addr);
                    }
                }
            }
        }

        test_debug!("{:?}: have {} pingbacks scheduled", &self.local_peer, self.walk_pingbacks.len());
        Ok(())
    }

    /// Are we in the process of downloading blocks?
    pub fn has_more_downloads(&self) -> bool {
        if let Some(ref dl) = self.block_downloader {
            !dl.is_download_idle() || dl.is_initial_download()
        }
        else {
            false
        }
    }

    /// Get the local peer from the peer DB, but also preserve the public IP address
    pub fn load_local_peer(&self) -> Result<LocalPeer, net_error> {
        let mut lp = PeerDB::get_local_peer(&self.peerdb.conn())?;
        lp.public_ip_address = self.local_peer.public_ip_address.clone();
        Ok(lp)
    }
   
    /// Update p2p networking state.
    /// -- accept new connections
    /// -- send data on ready sockets
    /// -- receive data on ready sockets
    /// -- clear out timed-out requests
    fn dispatch_network(&mut self,
                        network_result: &mut NetworkResult,
                        sortdb: &SortitionDB, 
                        chainstate: &mut StacksChainState, 
                        dns_client_opt: Option<&mut DNSClient>,
                        download_backpressure: bool,
                        mut poll_state: NetworkPollState) -> Result<(), net_error> {

        if self.network.is_none() {
            test_debug!("{:?}: network not connected", &self.local_peer);
            return Err(net_error::NotConnected);
        }

        // update burnchain snapshot if we need to (careful -- it's expensive)
        let sn = SortitionDB::get_canonical_burn_chain_tip_stubbed(&sortdb.conn)?;
        if sn.block_height > self.chain_view.burn_block_height {
            debug!("{:?}: load chain view for burn block {}", &self.local_peer, sn.block_height);
            let new_chain_view = {
                let ic = sortdb.index_conn();
                ic.get_burnchain_view(&self.burnchain, &sn)?
            };
            
            // wake up the inv-sync and downloader -- we have potentially more sortitions
            self.hint_sync_invs();
            self.hint_download_rescan();
            self.chain_view = new_chain_view;
        }
       
        // update local-peer state
        self.local_peer = self.load_local_peer()?;

        // set up new inbound conversations
        self.process_new_sockets(&mut poll_state)?;
    
        // set up sockets that have finished connecting
        self.process_connecting_sockets(&mut poll_state);

        // find out who is inbound and unathenticed
        let unauthenticated_inbounds = self.find_unauthenticated_inbound_convos();

        // run existing conversations, clear out broken ones, and get back messages forwarded to us
        let (error_events, unsolicited_messages) = self.process_ready_sockets(sortdb, chainstate, &mut poll_state);
        for error_event in error_events {
            debug!("{:?}: Failed connection on event {}", &self.local_peer, error_event);
            self.deregister_peer(error_event);
        }
        let unhandled_messages = self.handle_unsolicited_messages(sortdb, unsolicited_messages)?;
        network_result.consume_unsolicited(unhandled_messages);

        // schedule now-authenticated inbound convos for pingback
        self.schedule_network_pingbacks(unauthenticated_inbounds)?;

        // do some Actual Work(tm)
        // do this _after_ processing new sockets, so the act of opening a socket doesn't trample
        // an already-used network ID.
        let do_prune = self.do_network_work(sortdb, chainstate, dns_client_opt, download_backpressure, network_result)?;
        if do_prune {
            // prune back our connections if it's been a while
            // (only do this if we're done with all other tasks).
            // Also, process banned peers.
            let mut dead_events = self.process_bans()?;
            for dead in dead_events.drain(..) {
                debug!("{:?}: Banned connection on event {}", &self.local_peer, dead);
                self.deregister_peer(dead);
            }
            self.prune_connections();
        }
        
        // In parallel, do a neighbor walk
        self.do_network_neighbor_walk()?;
        
        // remove timed-out requests from other threads 
        for (_, convo) in self.peers.iter_mut() {
            convo.clear_timeouts();
        }
        
        // clear out peers that we haven't heard from in our heartbeat interval
        self.disconnect_unresponsive();
        
        // queue up pings to neighbors we haven't spoken to in a while
        self.queue_ping_heartbeats();
        
        // move conversations along
        let error_events = self.flush_relay_handles();
        for error_event in error_events {
            debug!("{:?}: Failed connection on event {}", &self.local_peer, error_event);
            self.deregister_peer(error_event);
        }

        // is our key about to expire?  do we need to re-key?
        // NOTE: must come last since it invalidates local_peer
        if self.local_peer.private_key_expire < self.chain_view.burn_block_height + 1 {
            self.peerdb.rekey(self.local_peer.private_key_expire + self.connection_opts.private_key_lifetime)?;
            let new_local_peer = self.load_local_peer()?;
            let old_local_peer = self.local_peer.clone();
            self.local_peer = new_local_peer;
            self.rekey(Some(&old_local_peer));
        }

        // update our relay statistics, so we know who to forward messages to
        self.update_relayer_stats(&network_result);

        // finally, handle network I/O requests from other threads, and get back reply handles to them.
        // do this after processing new sockets, so we don't accidentally re-use an event ID.
        self.dispatch_requests();
    
        Ok(())
    }

    /// Top-level main-loop circuit to take.
    /// -- polls the peer network and http network server sockets to get new sockets and detect ready sockets
    /// -- carries out network conversations
    /// -- receives and dispatches requests from other threads
    /// -- runs the p2p and http peer main loop
    /// Returns the table of unhandled network messages to be acted upon, keyed by the neighbors
    /// that sent them (i.e. keyed by their event IDs)
    pub fn run(&mut self, sortdb: &SortitionDB, chainstate: &mut StacksChainState, mempool: &mut MemPoolDB,
               dns_client_opt: Option<&mut DNSClient>, download_backpressure: bool,
               poll_timeout: u64, handler_args: &RPCHandlerArgs) -> Result<NetworkResult, net_error> {
        
        debug!(">>>>>>>>>>>>>>>>>>>>>>> Begin Network Dispatch (poll for {}) >>>>>>>>>>>>>>>>>>>>>>>>>>>>", poll_timeout);
        let mut poll_states = match self.network {
            None => {
                debug!("{:?}: network not connected", &self.local_peer);
                Err(net_error::NotConnected)
            },
            Some(ref mut network) => {
                let poll_result = network.poll(poll_timeout);
                poll_result
            }
        }?;

        let p2p_poll_state = poll_states.remove(&self.p2p_network_handle).expect("BUG: no poll state for p2p network handle");
        let http_poll_state = poll_states.remove(&self.http_network_handle).expect("BUG: no poll state for http network handle");
  
        let mut result = NetworkResult::new();

        PeerNetwork::with_network_state(self, |ref mut network, ref mut network_state| {
            let http_stacks_msgs = network.http.run(
                network_state, network.chain_view.clone(), &network.peers, sortdb,
                &network.peerdb, chainstate, mempool, http_poll_state, handler_args)?;
            result.consume_http_uploads(http_stacks_msgs);
            Ok(())
        })?;
        
        self.dispatch_network(&mut result, sortdb, chainstate, dns_client_opt, download_backpressure, p2p_poll_state)?;

        debug!("<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<< End Network Dispatch <<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<");
        Ok(result)
    }
}

#[cfg(test)]
mod test {

    use super::*;
    use net::*;
    use net::db::*;
    use net::codec::*;
    use std::thread;
    use std::time;
    use util::log;
    use util::sleep_ms;
    use burnchains::*;
    use burnchains::burnchain::*;

    use rand::RngCore;
    use rand;

    fn make_random_peer_address() -> PeerAddress {
        let mut rng = rand::thread_rng();
        let mut bytes = [0u8; 16];
        rng.fill_bytes(&mut bytes);
        PeerAddress(bytes)
    }

    fn make_test_neighbor(port: u16) -> Neighbor {
        let neighbor = Neighbor {
            addr: NeighborKey {
                peer_version: 0x12345678,
                network_id: 0x9abcdef0,
                addrbytes: PeerAddress([0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0xff,0xff,0x7f,0x00,0x00,0x01]),
                port: port,
            },
            public_key: Secp256k1PublicKey::from_hex("02fa66b66f8971a8cd4d20ffded09674e030f0f33883f337f34b95ad4935bac0e3").unwrap(),
            expire_block: 23456,
            last_contact_time: 1552509642,
            allowed: -1,
            denied: -1,
            asn: 34567,
            org: 45678,
            in_degree: 1,
            out_degree: 1
        };
        neighbor
    }

    fn make_test_p2p_network(initial_neighbors: &Vec<Neighbor>) -> PeerNetwork {
        let mut conn_opts = ConnectionOptions::default();
        conn_opts.inbox_maxlen = 5;
        conn_opts.outbox_maxlen = 5;

        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();

        let burnchain = Burnchain {
            peer_version: 0x012345678,
            network_id: 0x9abcdef0,
            chain_name: "bitcoin".to_string(),
            network_name: "testnet".to_string(),
            working_dir: "/nope".to_string(),
            consensus_hash_lifetime: 24,
            stable_confirmations: 7,
            first_block_height: 50,
            first_block_hash: first_burn_hash.clone(),
        };

        let mut burnchain_view = BurnchainView {
            burn_block_height: 12345,
            burn_consensus_hash: ConsensusHash::from_hex("1111111111111111111111111111111111111111").unwrap(),
            burn_stable_block_height: 12339,
            burn_stable_consensus_hash: ConsensusHash::from_hex("2222222222222222222222222222222222222222").unwrap(),
            last_consensus_hashes: HashMap::new()
        };
        burnchain_view.make_test_data();

        let db = PeerDB::connect_memory(0x9abcdef0, 0, 23456, "http://test-p2p.com".into(), &vec![], initial_neighbors).unwrap();
        let local_peer = PeerDB::get_local_peer(db.conn()).unwrap();
        let p2p = PeerNetwork::new(db, local_peer, 0x12345678, burnchain, burnchain_view, conn_opts);
        p2p
    }

    // tests relay_signed_message()
    #[test]
    #[ignore]
    fn test_dispatch_requests_connect_and_message_relay() {
        let neighbor = make_test_neighbor(2100);

        let mut p2p = make_test_p2p_network(&vec![]);

        let ping = StacksMessage::new(p2p.peer_version, p2p.local_peer.network_id,
                                      p2p.chain_view.burn_block_height,
                                      &p2p.chain_view.burn_consensus_hash,
                                      p2p.chain_view.burn_stable_block_height,
                                      &p2p.chain_view.burn_stable_consensus_hash,
                                      StacksMessageType::Ping(PingData::new()));

        let mut h = p2p.new_handle(1);

        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:2100").unwrap();

        // start fake neighbor endpoint, which will accept once and wait 5 seconds
        let endpoint_thread = thread::spawn(move || {
            let (sock, addr) = listener.accept().unwrap();
            test_debug!("Accepted {:?}", &addr);
            thread::sleep(time::Duration::from_millis(5000));
        });
        
        p2p.bind(&"127.0.0.1:2000".parse().unwrap(), &"127.0.0.1:2001".parse().unwrap()).unwrap();
        p2p.connect_peer(&neighbor.addr).unwrap();

        // start dispatcher
        let p2p_thread = thread::spawn(move || {
            for i in 0..5 {
                test_debug!("dispatch batch {}", i);

                p2p.dispatch_requests();
                let mut poll_states = match p2p.network {
                    None => {
                        panic!("network not connected");
                    },
                    Some(ref mut network) => {
                        network.poll(100).unwrap()
                    }
                };

                let mut p2p_poll_state = poll_states.remove(&p2p.p2p_network_handle).unwrap();

                p2p.process_new_sockets(&mut p2p_poll_state).unwrap();
                p2p.process_connecting_sockets(&mut p2p_poll_state);

                thread::sleep(time::Duration::from_millis(1000));
            }
        });

        // will eventually accept
        let mut sent = false;
        for i in 0..10 {
            match h.relay_signed_message(neighbor.addr.clone(), ping.clone()) {
                Ok(_) => {
                    sent = true;
                    break;
                },
                Err(net_error::NoSuchNeighbor) | Err(net_error::FullHandle) => {
                    test_debug!("Failed to relay; try again in {} ms", (i + 1) * 1000);
                    sleep_ms((i + 1) * 1000);
                },
                Err(e) => {
                    eprintln!("{:?}", &e);
                    assert!(false);
                }
            }
        }

        if !sent {
            error!("Failed to relay to neighbor");
            assert!(false);
        }

        p2p_thread.join().unwrap();
        test_debug!("dispatcher thread joined");

        endpoint_thread.join().unwrap();
        test_debug!("fake endpoint thread joined");
    }
    
    #[test]
    #[ignore]
    fn test_dispatch_requests_connect_and_ban() {
        let neighbor = make_test_neighbor(2200);

        let mut p2p = make_test_p2p_network(&vec![]);

        let ping = StacksMessage::new(p2p.peer_version, p2p.local_peer.network_id,
                                      p2p.chain_view.burn_block_height,
                                      &p2p.chain_view.burn_consensus_hash,
                                      p2p.chain_view.burn_stable_block_height,
                                      &p2p.chain_view.burn_stable_consensus_hash,
                                      StacksMessageType::Ping(PingData::new()));

        let mut h = p2p.new_handle(1);

        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:2200").unwrap();

        // start fake neighbor endpoint, which will accept once and wait 5 seconds
        let endpoint_thread = thread::spawn(move || {
            let (sock, addr) = listener.accept().unwrap();
            test_debug!("Accepted {:?}", &addr);
            thread::sleep(time::Duration::from_millis(5000));
        });
        
        p2p.bind(&"127.0.0.1:2010".parse().unwrap(), &"127.0.0.1:2011".parse().unwrap()).unwrap();
        p2p.connect_peer(&neighbor.addr).unwrap();

        let (sx, rx) = sync_channel(1);

        // start dispatcher, and relay back the list of peers we banned
        let p2p_thread = thread::spawn(move || {
            let mut banned_peers = vec![];
            for i in 0..5 {
                test_debug!("dispatch batch {}", i);

                p2p.dispatch_requests();
                let mut poll_state = match p2p.network {
                    None => {
                        panic!("network not connected");
                    },
                    Some(ref mut network) => {
                        network.poll(100).unwrap()
                    }
                };

                let mut p2p_poll_state = poll_state.remove(&p2p.p2p_network_handle).unwrap();

                p2p.process_new_sockets(&mut p2p_poll_state).unwrap();
                p2p.process_connecting_sockets(&mut p2p_poll_state);

                let mut banned = p2p.process_bans().unwrap();
                if banned.len() > 0 {
                    test_debug!("Banned {} peer(s)", banned.len());
                }

                banned_peers.append(&mut banned);

                thread::sleep(time::Duration::from_millis(5000));
            }

            let _ = sx.send(banned_peers);
        });

        // will eventually accept and ban
        for i in 0..5 {
            match h.ban_peers(vec![neighbor.addr.clone()]) {
                Ok(_) => {
                    continue;
                },
                Err(net_error::FullHandle) => {
                    test_debug!("Failed to relay; try again in {} ms", 1000 * (i + 1));
                    sleep_ms(1000 * (i + 1));
                },
                Err(e) => {
                    eprintln!("{:?}", &e);
                    assert!(false);
                }
            }
        }
        
        let banned = rx.recv().unwrap();
        assert!(banned.len() >= 1);

        p2p_thread.join().unwrap();
        test_debug!("dispatcher thread joined");

        endpoint_thread.join().unwrap();
        test_debug!("fake endpoint thread joined");
    }
}
