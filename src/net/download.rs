/*
 copyright: (c) 2013-2020 by Blockstack PBC, a public benefit corporation.

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

use std::sync::mpsc::sync_channel;
use std::sync::mpsc::SyncSender;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::TrySendError;
use std::sync::mpsc::TryRecvError;
use std::sync::mpsc::RecvError;
use std::sync::mpsc::RecvTimeoutError;

use std::hash::{Hash, Hasher};
use net::PeerAddress;
use net::Neighbor;
use net::NeighborKey;
use net::Error as net_error;
use net::db::PeerDB;
use net::asn::ASEntry4;
use net::inv::InvState;

use net::*;
use net::codec::*;
use net::dns::*;
use net::rpc::*;

use net::StacksMessage;
use net::StacksP2P;
use net::GetBlocksInv;
use net::connection::ReplyHandleHttp;
use net::connection::ConnectionOptions;

use net::neighbors::MAX_NEIGHBOR_BLOCK_DELAY;

use net::server::HttpPeer;

use net::db::*;

use net::p2p::PeerNetwork;

use util::db::Error as db_error;
use util::db::DBConn;
use util::secp256k1::Secp256k1PublicKey;
use util::secp256k1::Secp256k1PrivateKey;

use chainstate::burn::BlockHeaderHash;
use chainstate::burn::db::sortdb::{
    SortitionDB, SortitionDBConn, SortitionId, PoxId, BlockHeaderCache
};
use chainstate::burn::BlockSnapshot;

use chainstate::stacks::StacksBlockId;
use chainstate::stacks::Error as chainstate_error;
use chainstate::stacks::db::StacksChainState;
use chainstate::stacks::StacksBlockHeader;

use burnchains::Burnchain;
use burnchains::BurnchainView;

use std::net::SocketAddr;
use std::net::IpAddr;

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

use std::io::Read;
use std::io::Write;

use std::convert::TryFrom;

use util::log;
use util::get_epoch_time_secs;
use util::get_epoch_time_ms;
use util::hash::to_hex;

use rand::RngCore;
use rand::thread_rng;
use rand::seq::SliceRandom;

use core::EMPTY_MICROBLOCK_PARENT_HASH;
use core::FIRST_STACKS_BLOCK_HASH;
use core::FIRST_BURNCHAIN_CONSENSUS_HASH;

#[cfg(not(test))] pub const BLOCK_DOWNLOAD_INTERVAL : u64 = 180;
#[cfg(test)] pub const BLOCK_DOWNLOAD_INTERVAL : u64 = 30;

/// This module is responsible for downloading blocks and microblocks from other peers, using block
/// inventory state (see src/net/inv.rs)

#[derive(Debug, PartialEq, Clone, Hash, Eq)]
pub struct BlockRequestKey {
    pub neighbor: NeighborKey,
    pub data_url: UrlString,
    pub consensus_hash: ConsensusHash,
    pub anchor_block_hash: BlockHeaderHash,
    pub index_block_hash: StacksBlockId,
    pub child_block_header: Option<StacksBlockHeader>,      // only used if asking for a microblock; used to confirm the stream's continuity
    pub sortition_height: u64,
}


impl BlockRequestKey {
    pub fn new(neighbor: NeighborKey, data_url: UrlString, consensus_hash: ConsensusHash, anchor_block_hash: BlockHeaderHash, index_block_hash: StacksBlockId, child_block_header: Option<StacksBlockHeader>, sortition_height: u64) -> BlockRequestKey {
        BlockRequestKey {
            neighbor: neighbor,
            data_url: data_url,
            consensus_hash: consensus_hash,
            anchor_block_hash: anchor_block_hash,
            index_block_hash: index_block_hash,
            child_block_header: child_block_header,
            sortition_height: sortition_height
        }
    }
}

#[derive(Debug, Clone, PartialEq, Copy)]
pub enum BlockDownloaderState {
    DNSLookupBegin,
    DNSLookupFinish,
    GetBlocksBegin,
    GetBlocksFinish,
    GetMicroblocksBegin,
    GetMicroblocksFinish,
    Done
}

pub struct BlockDownloader {
    state: BlockDownloaderState,
    pox_id: PoxId,

    /// Sortition height at which to attempt to fetch blocks
    block_sortition_height: u64,
    microblock_sortition_height: u64,
    next_block_sortition_height: u64,
    next_microblock_sortition_height: u64,

    /// How many blocks downloaded since we re-scanned the chain?
    num_blocks_downloaded: u64,
    num_microblocks_downloaded: u64,

    /// How many times have we tried to download blocks, only to find nothing?
    empty_block_download_passes: u64,
    empty_microblock_download_passes: u64,

    /// When was the last time we did a full scan of the inv state?  when was the last time the inv
    /// state was updated?
    pub finished_scan_at: u64,
    last_inv_update_at: u64,

    /// Maximum number of concurrent requests
    max_inflight_requests: u64,

    /// Block requests to try, grouped by block, keyed by sortition height
    blocks_to_try: HashMap<u64, VecDeque<BlockRequestKey>>,
    
    /// Microblock requests to try, grouped by block, keyed by sortition height
    microblocks_to_try: HashMap<u64, VecDeque<BlockRequestKey>>,

    /// In-flight requests for DNS names
    parsed_urls: HashMap<UrlString, DNSRequest>,
    dns_lookups: HashMap<UrlString, Option<Vec<SocketAddr>>>,
    dns_timeout: u128,

    /// In-flight requests for blocks and confirmed microblocks
    /// The key for each of these is the sortition height and _index_ block hash.
    getblock_requests: HashMap<BlockRequestKey, usize>,
    getmicroblocks_requests: HashMap<BlockRequestKey, usize>,
    blocks: HashMap<BlockRequestKey, StacksBlock>,
    microblocks: HashMap<BlockRequestKey, Vec<StacksMicroblock>>,

    /// statistics on peers' data-plane endpoints
    dead_peers: Vec<usize>,
    broken_peers: Vec<usize>,
    broken_neighbors: Vec<NeighborKey>,     // disconnect peers who report invalid block inventories too

    /// how often to download
    download_interval: u64,

    /// set of blocks and microblocks we have successfully downloaded (even if they haven't been
    /// stored yet)
    blocks_downloaded: HashSet<StacksBlockId>,
    microblocks_downloaded: HashSet<StacksBlockId>
}

impl BlockDownloader {
    pub fn new(dns_timeout: u128, download_interval: u64, max_inflight_requests: u64) -> BlockDownloader {
        BlockDownloader {
            state: BlockDownloaderState::DNSLookupBegin,
            pox_id: PoxId::initial(),

            block_sortition_height: 0,
            microblock_sortition_height: 0,
            next_block_sortition_height: 0,
            next_microblock_sortition_height: 0,

            num_blocks_downloaded: 0,
            num_microblocks_downloaded: 0,
            empty_block_download_passes: 0,
            empty_microblock_download_passes: 0,
            finished_scan_at: 0,
            last_inv_update_at: 0,

            max_inflight_requests: max_inflight_requests,
            blocks_to_try: HashMap::new(),
            microblocks_to_try: HashMap::new(),

            parsed_urls: HashMap::new(),
            dns_lookups: HashMap::new(),
            dns_timeout: dns_timeout,

            getblock_requests: HashMap::new(),
            getmicroblocks_requests: HashMap::new(),
            blocks: HashMap::new(),
            microblocks: HashMap::new(),

            dead_peers: vec![],
            broken_peers: vec![],
            broken_neighbors: vec![],

            download_interval: download_interval,

            blocks_downloaded: HashSet::new(),
            microblocks_downloaded: HashSet::new(),
        }
    }

    pub fn reset(&mut self) -> () {
        self.state = BlockDownloaderState::DNSLookupBegin;

        self.dns_lookups.clear();
        self.parsed_urls.clear();

        self.getblock_requests.clear();
        self.getmicroblocks_requests.clear();
        self.blocks_to_try.clear();
        self.microblocks_to_try.clear();
        self.blocks.clear();
        self.microblocks.clear();

        self.dead_peers.clear();
        self.broken_peers.clear();
        self.broken_neighbors.clear();

        // perserve sortition height
        // preserve download accounting
    }

    pub fn restart_scan(&mut self) -> () {
        // prepare to restart a full-chain scan for block downloads
        self.block_sortition_height = 0;
        self.microblock_sortition_height = 0;
        self.next_block_sortition_height = 0;
        self.next_microblock_sortition_height = 0;
        self.empty_block_download_passes = 0;
        self.empty_microblock_download_passes = 0;
    }

    pub fn dns_lookups_begin(&mut self, pox_id: &PoxId, dns_client: &mut DNSClient, mut urls: Vec<UrlString>) -> Result<(), net_error> {
        assert_eq!(self.state, BlockDownloaderState::DNSLookupBegin);

        // optimistic concurrency control: remember the current PoX Id
        self.pox_id = pox_id.clone();
        self.dns_lookups.clear();
        for url_str in urls.drain(..) {
            if url_str.len() == 0 {
                continue;
            }
            let url = url_str.parse_to_block_url()?;        // NOTE: should always succeed, since a UrlString shouldn't decode unless it's a valid URL or the empty string
            let port = match url.port_or_known_default() {
                Some(p) => p,
                None => {
                    warn!("Unsupported URL {:?}: unknown port", &url);
                    continue;
                }
            };
            match url.host() {
                Some(url::Host::Domain(domain)) => {
                    dns_client.queue_lookup(domain.clone(), port, get_epoch_time_ms() + self.dns_timeout)?;
                    self.dns_lookups.insert(url_str.clone(), None);
                    self.parsed_urls.insert(url_str, DNSRequest::new(domain.to_string(), port, 0));
                },
                Some(url::Host::Ipv4(addr)) => {
                    self.dns_lookups.insert(url_str, Some(vec![SocketAddr::new(IpAddr::V4(addr), port)]));
                }
                Some(url::Host::Ipv6(addr)) => {
                    self.dns_lookups.insert(url_str, Some(vec![SocketAddr::new(IpAddr::V6(addr), port)]));
                },
                None => {
                    warn!("Unsupported URL {:?}", &url_str);
                }
            }
        }

        self.state = BlockDownloaderState::DNSLookupFinish;
        Ok(())
    }

    pub fn dns_lookups_try_finish(&mut self, dns_client: &mut DNSClient) -> Result<bool, net_error> {
        dns_client.try_recv()?;

        let mut inflight = 0;
        for (url_str, request) in self.parsed_urls.iter() {
            match dns_client.poll_lookup(&request.host, request.port) {
                Ok(Some(query_result)) => {
                    if let Some(dns_result) = self.dns_lookups.get_mut(url_str) {
                        // solicited
                        match query_result.result {
                            Ok(addrs) => {
                                *dns_result = Some(addrs);
                            },
                            Err(msg) => {
                                warn!("DNS failed to look up {:?}: {}", &url_str, msg);
                            }
                        }
                    }
                },
                Ok(None) => {
                    inflight += 1;
                }
                Err(e) => {
                    warn!("DNS lookup failed on {:?}: {:?}", url_str, &e);
                }
            }
        }

        if inflight == 0 {
            // done with DNS
            dns_client.clear_all_requests();
            self.state = BlockDownloaderState::GetBlocksBegin;
        }

        Ok(inflight == 0)
    }

    pub fn getblocks_begin(&mut self, requests: HashMap<BlockRequestKey, usize>) -> () {
        assert_eq!(self.state, BlockDownloaderState::GetBlocksBegin);

        // don't touch blocks-to-try -- that's managed by the peer network directly.
        self.getblock_requests = requests;
        self.state = BlockDownloaderState::GetBlocksFinish;
    }

    /// Finish fetching blocks.  Return true once all reply handles have been fulfilled (either
    /// with data, or with an error).
    /// Store blocks as we get them.
    pub fn getblocks_try_finish(&mut self, http: &mut HttpPeer) -> Result<bool, net_error> {
        assert_eq!(self.state, BlockDownloaderState::GetBlocksFinish);

        // requests that are still pending
        let mut pending_block_requests = HashMap::new();

        for (block_key, event_id) in self.getblock_requests.drain() {
            match http.get_conversation(event_id) {
                None => {
                    if http.is_connecting(event_id) {
                        debug!("Event {} ({:?}, {:?} for block {} is not connected yet", event_id, &block_key.neighbor, &block_key.data_url, &block_key.index_block_hash);
                        pending_block_requests.insert(block_key, event_id);
                    }
                    else {
                        debug!("Event {} ({:?}, {:?} for block {} failed to connect", event_id, &block_key.neighbor, &block_key.data_url, &block_key.index_block_hash);
                        self.dead_peers.push(event_id);
                    }
                }
                Some(ref mut convo) => match convo.try_get_response() {
                    None => {
                        // still waiting
                        debug!("Event {} ({:?}, {:?} for block {}) is still waiting for a response", event_id, &block_key.neighbor, &block_key.data_url, &block_key.index_block_hash);
                        pending_block_requests.insert(block_key, event_id);
                    },
                    Some(http_response) => match http_response {
                        HttpResponseType::Block(_md, block) => {
                            if StacksBlockHeader::make_index_block_hash(&block_key.consensus_hash, &block.block_hash()) != block_key.index_block_hash {
                                test_debug!("Invalid block from {:?} ({:?}): did not ask for block {}/{}", &block_key.neighbor, &block_key.data_url, block_key.consensus_hash, block.block_hash());
                                self.broken_peers.push(event_id);
                                self.broken_neighbors.push(block_key.neighbor.clone());
                            }
                            else {
                                // got the block
                                test_debug!("Got block {}: {}/{}", &block_key.sortition_height, &block_key.consensus_hash, block.block_hash());
                                self.blocks.insert(block_key, block);
                            }
                        },
                        // TODO: redirect?
                        HttpResponseType::NotFound(_, _) => {
                            // remote peer didn't have the block 
                            test_debug!("Remote neighbor {:?} ({:?}) does not actually have block {} indexed at {} ({})", &block_key.neighbor, &block_key.data_url, block_key.sortition_height, &block_key.index_block_hash, &block_key.consensus_hash);
                            
                            // the fact that we asked this peer means that it's block inv indicated
                            // it was present, so the absence is the mark of a broken peer
                            self.broken_peers.push(event_id);
                            self.broken_neighbors.push(block_key.neighbor.clone());
                        }
                        _ => {
                            // wrong message response
                            test_debug!("Got bad HTTP response from {:?}: {:?}", &block_key.data_url, &http_response);
                            self.broken_peers.push(event_id);
                            self.broken_neighbors.push(block_key.neighbor.clone());
                        }
                    }
                }
            }
        }

        // are we done?
        if pending_block_requests.len() == 0 {
            self.state = BlockDownloaderState::GetMicroblocksBegin;
            return Ok(true);
        }

        // still have more to go 
        for (block_key, event_id) in pending_block_requests.drain() {
            self.getblock_requests.insert(block_key, event_id);
        }
        return Ok(false);
    }
   
    /// Start fetching microblocks
    pub fn getmicroblocks_begin(&mut self, requests: HashMap<BlockRequestKey, usize>) -> () {
        assert_eq!(self.state, BlockDownloaderState::GetMicroblocksBegin);

        self.getmicroblocks_requests = requests;
        self.state = BlockDownloaderState::GetMicroblocksFinish;
    }

    pub fn getmicroblocks_try_finish(&mut self, http: &mut HttpPeer) -> Result<bool, net_error> {
        assert_eq!(self.state, BlockDownloaderState::GetMicroblocksFinish);

        // requests that are still pending
        let mut pending_microblock_requests = HashMap::new();

        for (block_key, event_id) in self.getmicroblocks_requests.drain() {
            let rh_block_key = block_key.clone();
            match http.get_conversation(event_id) {
                None => {
                    if http.is_connecting(event_id) {
                        debug!("Event {} ({:?}, {:?} for microblocks built by ({}) is not connected yet", &block_key.neighbor, &block_key.data_url, &block_key.index_block_hash, event_id);
                        pending_microblock_requests.insert(block_key, event_id);
                    }
                    else {
                        debug!("Event {} ({:?}, {:?} for microblocks built by ({}) failed to connect", &block_key.neighbor, &block_key.data_url, &block_key.index_block_hash, event_id);
                        self.dead_peers.push(event_id);
                    }
                }
                Some(ref mut convo) => match convo.try_get_response() {
                    None => {
                        // still waiting
                        debug!("Event {} ({:?}, {:?} for microblocks built by {:?}) is still waiting for a response", &block_key.neighbor, &block_key.data_url, &block_key.index_block_hash, event_id);
                        pending_microblock_requests.insert(rh_block_key, event_id);
                    },
                    Some(http_response) => match http_response {
                        HttpResponseType::Microblocks(_md, microblocks) => {
                            if microblocks.len() == 0 {
                                // we wouldn't have asked for a 0-length stream
                                test_debug!("Got unexpected zero-length microblock stream from {:?} ({:?})", &block_key.neighbor, &block_key.data_url);
                                self.broken_peers.push(event_id);
                                self.broken_neighbors.push(block_key.neighbor.clone());
                            }
                            else {
                                // have microblocks (but we don't know yet if they're well-formed)
                                test_debug!("Got (tentative) microblocks {}: {}/{}-{}", block_key.sortition_height, &block_key.consensus_hash, &block_key.index_block_hash, microblocks[0].block_hash());
                                self.microblocks.insert(block_key, microblocks);
                            }
                        },
                        // TODO: redirect?
                        HttpResponseType::NotFound(_, _) => {
                            // remote peer didn't have the microblock, even though their blockinv said
                            // they did.
                            test_debug!("Remote neighbor {:?} ({:?}) does not have microblock stream indexed at {}", &block_key.neighbor, &block_key.data_url, &block_key.index_block_hash);
                            
                            // the fact that we asked this peer means that it's block inv indicated
                            // it was present, so the absence is the mark of a broken peer
                            self.broken_peers.push(event_id);
                            self.broken_neighbors.push(block_key.neighbor.clone());
                        }
                        _ => {
                            // wrong message response
                            test_debug!("Got bad HTTP response from {:?}", &block_key.data_url);
                            self.broken_peers.push(event_id);
                            self.broken_neighbors.push(block_key.neighbor.clone());
                        }
                    }
                }
            }
        }

        // are we done?
        if pending_microblock_requests.len() == 0 {
            self.state = BlockDownloaderState::Done;
            return Ok(true);
        }

        // still have more to go 
        for (block_key, event_id) in pending_microblock_requests.drain() {
            self.getmicroblocks_requests.insert(block_key, event_id);
        }
        return Ok(false);
    }
    
    /// Get the availability of each block in the given sortition range, using the inv state.
    /// Return the local block headers, paired with the list of peers that can serve them.
    /// Possibly less than the given range request.
    pub fn get_block_availability(inv_state: &InvState, 
                                  sortdb: &SortitionDB, 
                                  header_cache: &mut BlockHeaderCache, 
                                  sortition_height_start: u64, 
                                  mut sortition_height_end: u64) -> Result<Vec<(ConsensusHash, Option<BlockHeaderHash>, Vec<NeighborKey>)>, net_error> {

        let first_block_height = sortdb.first_block_height;

        // what blocks do we have in this range?
        let local_blocks = {
            let ic = sortdb.index_conn();
            let tip = SortitionDB::get_canonical_burn_chain_tip(&ic)?;

            if tip.block_height < first_block_height + sortition_height_start {
                test_debug!("Tip height {} < {}", tip.block_height, first_block_height + sortition_height_start);
                return Ok(vec![]);
            }

            if tip.block_height < first_block_height + sortition_height_end {
                test_debug!("Truncate end sortition {} down to {}", sortition_height_end, tip.block_height - first_block_height);
                sortition_height_end = tip.block_height - first_block_height;
            }

            if sortition_height_end <= sortition_height_start {
                test_debug!("sortition end {} <= sortition start {}", sortition_height_end, sortition_height_start);
                return Ok(vec![]);
            }

            debug!("Begin headers load");
            let last_ancestor = SortitionDB::get_ancestor_snapshot(&ic, first_block_height + sortition_height_end, &tip.sortition_id)?
                .ok_or_else(|| net_error::DBError(db_error::NotFoundError))?;
            
            debug!("Load {} headers off of {} ({})", sortition_height_end - sortition_height_start, last_ancestor.block_height, &last_ancestor.consensus_hash);
            let local_blocks = ic.get_stacks_header_hashes(
                sortition_height_end - sortition_height_start,
                &last_ancestor.consensus_hash,
                header_cache)?;

            for (_i, (_consensus_hash, _block_hash_opt)) in local_blocks.iter().enumerate() {
                test_debug!("  Loaded {} ({}): {:?}/{:?}", (_i as u64) + sortition_height_start, (_i as u64) + sortition_height_start + first_block_height, _consensus_hash, _block_hash_opt);
            }
            debug!("End headers load");

            // update cache 
            SortitionDB::merge_block_header_cache(header_cache, &local_blocks);

            local_blocks
        };

        let mut ret = vec![];
        for (i, (consensus_hash, block_hash_opt)) in local_blocks.into_iter().enumerate() {
            let sortition_height = sortition_height_start + (i as u64) + 1;
            match block_hash_opt {
                Some(block_hash) => {
                    // a sortition happened at this height
                    let mut neighbors = vec![];
                    for (nk, stats) in inv_state.block_stats.iter() {
                        test_debug!("stats for {:?}: {:?}; testing block {}", &nk, &stats, sortition_height + first_block_height); 
                        if stats.inv.has_ith_block(sortition_height + first_block_height) {
                            neighbors.push(nk.clone());
                        }
                    }
                    test_debug!("at sortition height {} (block {}): {:?}/{:?} blocks available from {:?}", sortition_height, sortition_height + first_block_height, &consensus_hash, &block_hash, &neighbors);
                    ret.push((consensus_hash, Some(block_hash), neighbors));
                },
                None => {
                    // no sortition 
                    test_debug!("at sortition height {} (block {}): {:?}/(no sortition)", sortition_height, sortition_height + first_block_height, &consensus_hash);
                    ret.push((consensus_hash, None, vec![]));
                }
            }
        }

        Ok(ret)
    }

    /// Find out which neighbors can serve a confirmed microblock stream, given the
    /// burn/block-header-hashes of the sortition that _produced_ them.
    fn get_microblock_stream_availability(inv_state: &InvState, sortdb: &SortitionDB, consensus_hash: &ConsensusHash, block_hash: &BlockHeaderHash) -> Result<Vec<NeighborKey>, net_error> {
        let sn = SortitionDB::get_block_snapshot_consensus(sortdb.conn(), consensus_hash)?
            .ok_or_else(|| net_error::DBError(db_error::NotFoundError))?;

        let block_height = sn.block_height;

        if sn.winning_stacks_block_hash != *block_hash {
            test_debug!("Snapshot of {} (height {}) does not have winning block hash {}", consensus_hash, block_height, block_hash);
            return Err(net_error::DBError(db_error::NotFoundError));
        }

        let mut neighbors = vec![];
        for (nk, stats) in inv_state.block_stats.iter() {
            test_debug!("stats for {:?}: {:?}; testing block {}", &nk, &stats, block_height);
            if stats.inv.has_ith_microblock_stream(block_height) {
                neighbors.push(nk.clone());
            }
        }
        test_debug!("at sortition height {} (block {}): {:?}/{:?} microblocks available from {:?}", block_height - sortdb.first_block_height + 1, block_height, consensus_hash, block_hash, &neighbors);
        Ok(neighbors)
    }

    /// Clear out broken peers that told us they had blocks, but didn't serve them.
    fn clear_broken_peers(&mut self) -> (Vec<usize>, Vec<NeighborKey>) {
        // remove dead/broken peers
        let mut disconnect = vec![];
        let mut disconnect_neighbors = vec![];

        disconnect.append(&mut self.broken_peers);
        disconnect.append(&mut self.dead_peers);
        disconnect_neighbors.append(&mut self.broken_neighbors);

        (disconnect, disconnect_neighbors)
    }
    
    /// Set a hint that a block is now available from a remote peer, if we're idling or we're ahead
    /// of the given height.
    pub fn hint_block_sortition_height_available(&mut self, block_sortition_height: u64) -> () {
        if self.empty_block_download_passes > 0 || block_sortition_height < self.block_sortition_height {
            // idling on new blocks to fetch
            self.empty_block_download_passes = 0;
            self.block_sortition_height = block_sortition_height;
            self.next_block_sortition_height = block_sortition_height;

            debug!("Awaken downloader to start scanning at block sortiton height {}", block_sortition_height);
        }
    }
    
    /// Set a hint that a confirmed microblock stream is now available from a remote peer, if we're idling or we're ahead
    /// of the given height.
    pub fn hint_microblock_sortition_height_available(&mut self, mblock_sortition_height: u64) -> () {
        if self.empty_microblock_download_passes > 0 || mblock_sortition_height < self.microblock_sortition_height {
            // idling on new blocks to fetch
            self.empty_microblock_download_passes = 0;
            self.microblock_sortition_height = mblock_sortition_height;
            self.next_microblock_sortition_height = mblock_sortition_height;

            debug!("Awaken downloader to start scanning at microblock sortiton height {}", mblock_sortition_height);
        }
    }

    /// Set a hint that we should re-scan for blocks
    pub fn hint_download_rescan(&mut self) -> () {
        if self.empty_block_download_passes > 0 {
            self.empty_block_download_passes = 0;
            self.next_block_sortition_height = 0;
        }

        if self.empty_microblock_download_passes > 0 {
            self.empty_microblock_download_passes = 0;
            self.next_microblock_sortition_height = 0;
        }

        debug!("Awaken downloader to restart scanning");
    }

    // are we doing the initial block download?
    pub fn is_initial_download(&self) -> bool {
        self.finished_scan_at == 0 
    }

    // is the downloader idle? i.e. did we already do a scan?
    pub fn is_download_idle(&self) -> bool {
        self.empty_block_download_passes > 0 && self.empty_microblock_download_passes > 0
    }
}

impl PeerNetwork {
    pub fn with_downloader_state<F, R>(&mut self, handler: F) -> Result<R, net_error> 
    where
        F: FnOnce(&mut PeerNetwork, &mut BlockDownloader) -> Result<R, net_error>
    {
        let mut downloader = self.block_downloader.take();
        let res = match downloader {
            None => {
                debug!("{:?}: downloader not connected", &self.local_peer);
                Err(net_error::NotConnected)
            },
            Some(ref mut dl) => handler(self, dl)
        };
        self.block_downloader = downloader;
        res
    }

    /// Pass a hint to the downloader to re-scan
    pub fn hint_download_rescan(&mut self) -> () {
        match self.block_downloader {
            Some(ref mut dl) => dl.hint_download_rescan(),
            None => {}
        }
    }

    /// Get the data URL for a neighbor
    fn get_data_url(&self, neighbor_key: &NeighborKey) -> Option<UrlString> {
        match self.events.get(neighbor_key) {
            Some(ref event_id) => match self.peers.get(event_id) {
                Some(ref convo) => {
                    if convo.data_url.len() > 0 {
                        Some(convo.data_url.clone())
                    }
                    else {
                        None
                    }
                },
                None => None,
            },
            None => None
        }
    }

    /// Create block request keys for a range of blocks that are available but that we don't have in a given range of
    /// sortitions.  The same keys can be used to fetch confirmed microblock streams.
    fn make_requests(&mut self, sortdb: &SortitionDB, chainstate: &StacksChainState, downloader: &BlockDownloader, start_sortition_height: u64, microblocks: bool) -> Result<HashMap<u64, VecDeque<BlockRequestKey>>, net_error> {
        let scan_batch_size = self.burnchain.pox_constants.reward_cycle_length as u64;
        let mut blocks_to_try : HashMap<u64, VecDeque<BlockRequestKey>> = HashMap::new();

        debug!("{:?}: find {} availability over sortitions ({}-{})...", &self.local_peer, if microblocks { "microblocks" } else { "anchored blocks" }, start_sortition_height, start_sortition_height + scan_batch_size);

        let mut availability = PeerNetwork::with_inv_state(self, |ref mut network, ref mut inv_state| {
            BlockDownloader::get_block_availability(inv_state, sortdb, &mut network.header_cache, start_sortition_height, start_sortition_height + scan_batch_size)
        })?;

        debug!("{:?}: {} availability calculated over {} sortitions ({}-{})", &self.local_peer, if microblocks { "microblocks" } else { "anchored blocks" }, availability.len(), start_sortition_height, start_sortition_height + scan_batch_size);

        for (i, (consensus_hash, block_hash_opt, mut neighbors)) in availability.drain(..).enumerate() {
            if (i as u64) >= scan_batch_size {
                // we may have loaded scan_batch_size + 1 so we can find the child block for
                // microblocks, but we don't have to request this block's data either way.
                break;
            }

            let block_hash = match block_hash_opt {
                Some(h) => h,
                None => {
                    continue;
                }
            };
            
            let index_block_hash = StacksBlockHeader::make_index_block_hash(&consensus_hash, &block_hash);
            let mut child_block_header = None;

            let (target_consensus_hash, target_block_hash) = 
                if !microblocks {
                    // asking for a block
                    if StacksChainState::has_block_indexed(&chainstate.blocks_path, &index_block_hash)? {
                        // we already have this block
                        test_debug!("{:?}: Already have anchored block {}/{}", &self.local_peer, &consensus_hash, &block_hash);
                        continue;
                    }
                     
                    test_debug!("{:?}: Do not have anchored block {}/{} ({})", &self.local_peer, &consensus_hash, &block_hash, &index_block_hash);

                    (consensus_hash, block_hash)
                }
                else {
                    // asking for microblocks
                    let block_header = match StacksChainState::load_block_header(&chainstate.blocks_path, &consensus_hash, &block_hash) {
                        Ok(Some(header)) => header,
                        Ok(None) => {
                            // we don't have this anchored block confirmed yet, so we can't ask for
                            // microblocks.
                            test_debug!("{:?}: Do not have anchored block {}/{} yet, so cannot ask for the microblocks it confirmed", &self.local_peer, &consensus_hash, &block_hash);
                            continue;
                        },
                        Err(chainstate_error::DBError(db_error::NotFoundError)) => {
                            // we can't fetch this microblock stream because we don't yet know
                            // about this block
                            test_debug!("{:?}: Do not have anchored block {}/{} yet, so cannot ask for the microblocks it confirmed", &self.local_peer, &consensus_hash, &block_hash);
                            continue;
                        },
                        Err(e) => {
                            return Err(e.into());
                        }
                    };

                    if block_header.parent_microblock == EMPTY_MICROBLOCK_PARENT_HASH && block_header.parent_microblock_sequence == 0 {
                        // this block doesn't confirm a microblock stream
                        test_debug!("Block {}/{} does not confirm a microblock stream", &consensus_hash, &block_hash);
                        continue;
                    }

                    // does this anchor block _confirm_ a microblock stream that we don't know about?
                    let parent_header_opt = {
                        let ic = sortdb.index_conn();
                        match StacksChainState::load_parent_block_header(&ic, &chainstate.blocks_path, &consensus_hash, &block_hash) {
                            Ok(header_opt) => header_opt,
                            Err(chainstate_error::DBError(db_error::NotFoundError)) => {
                                // we don't know about this parent block yet
                                debug!("{:?}: Do not have parent of anchored block {}/{} yet, so cannot ask for the microblocks it produced", &self.local_peer, &consensus_hash, &block_hash);
                                continue;
                            },
                            Err(e) => {
                                return Err(e.into());
                            }
                        }
                    };

                    if let Some((parent_header, parent_consensus_hash)) = parent_header_opt {
                        if StacksChainState::get_microblock_stream_head_hash(&chainstate.blocks_db, &parent_consensus_hash, &parent_header.block_hash())?.is_some() {
                            // we already have the first block in the stream that descends from the parent, which indicates that we have already fetched this stream (but possibly out-of-order).
                            // Verify this by checking that we also have the tail that connects to this anchored block.
                            if StacksChainState::load_staging_microblock(&chainstate.blocks_db, &parent_consensus_hash, &parent_header.block_hash(), &block_header.parent_microblock)?.is_some() {
                                test_debug!("{:?}: Already have microblock stream confirmed by {}/{} (built by {}/{})", &self.local_peer, &consensus_hash, &block_hash, &parent_consensus_hash, &parent_header.block_hash());
                                continue;
                            }
                        }

                        // ask for the microblocks _confirmed_ by this block (by asking for the
                        // microblocks built off of this block's _parent_)
                        let mut microblock_stream_neighbors = match self.inv_state {
                            Some(ref inv_state) => BlockDownloader::get_microblock_stream_availability(inv_state, sortdb, &parent_consensus_hash, &parent_header.block_hash())?,
                            None => vec![]
                        };

                        // use these neighbors instead
                        neighbors.clear();
                        neighbors.append(&mut microblock_stream_neighbors);

                        test_debug!("{:?}: Get microblocks produced by {}/{}, confirmed by {}/{}", &self.local_peer, &parent_consensus_hash, &parent_header.block_hash(), &consensus_hash, &block_hash);

                        child_block_header = Some(block_header);
                        (parent_consensus_hash, parent_header.block_hash())
                    }
                    else {
                        // we don't have the block that produced this stream 
                        test_debug!("{:?}: Do not have parent anchored block of {}/{}", &self.local_peer, &consensus_hash, &block_hash);
                        continue;
                    }
                };

            let target_index_block_hash = StacksBlockHeader::make_index_block_hash(&target_consensus_hash, &target_block_hash);
            if !microblocks && downloader.blocks_downloaded.contains(&target_index_block_hash) {
                // already downloaded this
                continue;
            }
            if microblocks && downloader.microblocks_downloaded.contains(&target_index_block_hash) {
                // already downloaded this stream
                continue;
            }

            // don't request the same data from the same data url, in case multiple peers report the
            // same data url (e.g. two peers sharing a Gaia hub).
            let block_urls : HashSet<UrlString> = HashSet::new();
            (&mut neighbors[..]).shuffle(&mut thread_rng());

            let mut requests = VecDeque::new();
            for nk in neighbors.drain(..) {
                let data_url = match self.get_data_url(&nk) {
                    Some(url) => url,
                    None => {
                        continue;
                    }
                };
                if data_url.len() == 0 {
                    // peer doesn't yet know its public IP address, and isn't given a data URL
                    // directly
                    continue;
                }
                if block_urls.contains(&data_url) {
                    continue;
                }

                test_debug!("{:?}: Make request for {} at sortition height {} to {:?}: {:?}/{:?}", 
                             &self.local_peer, if microblocks { "microblock stream" } else { "anchored block" }, (i as u64) + start_sortition_height, &nk, &target_consensus_hash, &target_block_hash);

                let request = BlockRequestKey::new(nk, data_url, target_consensus_hash.clone(), target_block_hash.clone(), target_index_block_hash.clone(), child_block_header.clone(), (i as u64) + start_sortition_height);
                requests.push_back(request);
            }

            blocks_to_try.insert((i as u64) + start_sortition_height, requests);
        }

        Ok(blocks_to_try)
    }

    /// Make requests for missing anchored blocks
    fn make_block_requests(&mut self, sortdb: &SortitionDB, chainstate: &mut StacksChainState, downloader: &BlockDownloader, start_sortition_height: u64) -> Result<HashMap<u64, VecDeque<BlockRequestKey>>, net_error> {
        self.make_requests(sortdb, chainstate, downloader, start_sortition_height, false)
    }

    /// Make requests for missing confirmed microblocks 
    fn make_confirmed_microblock_requests(&mut self, sortdb: &SortitionDB, chainstate: &mut StacksChainState, downloader: &BlockDownloader, start_sortition_height: u64) -> Result<HashMap<u64, VecDeque<BlockRequestKey>>, net_error> {
        self.make_requests(sortdb, chainstate, downloader, start_sortition_height, true)
    }

    /// Prioritize block requests -- ask for the rarest blocks first
    fn prioritize_requests(requests: &HashMap<u64, VecDeque<BlockRequestKey>>) -> Vec<u64> {
        let mut ordered = vec![];
        for (block_height, requests) in requests.iter() {
            ordered.push((*block_height, requests.len()));
        }
        ordered.sort_by(|(_, ref l1), (_, ref l2)| l1.cmp(l2));
        ordered.iter().map(|(ref h, _)| *h).collect()
    }

    /// Go start resolving block URLs to their IP addresses
    pub fn block_dns_lookups_begin(&mut self, sortdb: &SortitionDB, chainstate: &mut StacksChainState, dns_client: &mut DNSClient) -> Result<(), net_error> {
        test_debug!("{:?}: block_dns_lookups_begin", &self.local_peer);
        let (need_blocks, block_sortition_height, microblock_sortition_height) = match self.block_downloader {
            Some(ref mut downloader) => (downloader.blocks_to_try.len() == 0, downloader.block_sortition_height, downloader.microblock_sortition_height),
            None => {
                test_debug!("{:?}: downloader not connected", &self.local_peer);
                return Err(net_error::NotConnected);
            }
        };

        if need_blocks {
            PeerNetwork::with_downloader_state(self, |ref mut network, ref mut downloader| {
                test_debug!("{:?}: needs blocks", &network.local_peer);

                let mut next_block_sortition_height = block_sortition_height;
                let mut next_microblock_sortition_height = microblock_sortition_height;

                debug!("{:?}: Look for blocks at sortition {}, microblocks at sortition {}", &network.local_peer, next_block_sortition_height, next_microblock_sortition_height);

                // fetch as many blocks and microblocks as we can -- either
                // downloader.max_inflight_requests, or however many blocks remain between the
                // downloader's sortition height and the chain tip's sortition height (whichever is
                // smaller).
                while next_block_sortition_height <= network.chain_view.burn_block_height - sortdb.first_block_height || next_microblock_sortition_height <= network.chain_view.burn_block_height - sortdb.first_block_height {

                    debug!("{:?}: Make block requests from sortition height {}", &network.local_peer, next_block_sortition_height);
                    let mut next_blocks_to_try = network.make_block_requests(sortdb, chainstate, downloader, next_block_sortition_height)?;
                    
                    debug!("{:?}: Make microblock requests from sortition height {}", &network.local_peer, next_microblock_sortition_height);
                    let mut next_microblocks_to_try = network.make_confirmed_microblock_requests(sortdb, chainstate, downloader, next_microblock_sortition_height)?;

                    let mut height = next_block_sortition_height;
                    let mut mblock_height = next_microblock_sortition_height;

                    let mut max_height = 0;
                    let mut max_mblock_height = 0;

                    for h in next_blocks_to_try.keys() {
                        if *h > max_height {
                            max_height = *h;
                        }
                    }

                    for h in next_microblocks_to_try.keys() {
                        if *h > max_mblock_height {
                            max_mblock_height = *h;
                        }
                    }

                    test_debug!("{:?}: at {},{}: {} blocks to get, {} microblock streams to get (up to {},{})", 
                                &network.local_peer, next_block_sortition_height, next_microblock_sortition_height, next_blocks_to_try.len(), next_microblocks_to_try.len(), max_height, max_mblock_height);

                    test_debug!("{:?}: Begin block requests", &network.local_peer);
                    for (_key, _requests) in next_blocks_to_try.iter() {
                        test_debug!("   {:?}: {:?}", _key, _requests);
                    }
                    test_debug!("{:?}: End block requests", &network.local_peer);

                    test_debug!("{:?}: Begin microblock requests", &network.local_peer);
                    for (_key, _requests) in next_microblocks_to_try.iter() {
                        test_debug!("   {:?}: {:?}", _key, _requests);
                    }
                    test_debug!("{:?}: End microblock requests", &network.local_peer);

                    // queue up block requests in order by sortition height
                    while height <= max_height && (downloader.blocks_to_try.len() as u64) < downloader.max_inflight_requests {
                        if !next_blocks_to_try.contains_key(&height) {
                            height += 1;
                            continue;
                        }

                        if downloader.blocks_to_try.contains_key(&height) {
                            debug!("Block download already in-flight for {}", height);
                            height += 1;
                            continue;
                        }
                        
                        let requests = next_blocks_to_try.remove(&height).expect("BUG: hashmap both contains and does not contain sortition height");
                        if requests.len() == 0 {
                            height += 1;
                            continue;
                        }

                        assert_eq!(height, requests.front().as_ref().unwrap().sortition_height);

                        test_debug!("{:?}: request anchored block for sortition {}: {}/{} ({})", 
                                    &network.local_peer, height, &requests.front().as_ref().unwrap().consensus_hash, &requests.front().as_ref().unwrap().anchor_block_hash, &requests.front().as_ref().unwrap().index_block_hash);

                        downloader.blocks_to_try.insert(height, requests);

                        height += 1;
                    }
                    
                    // queue up microblock requests in order by sortition height.
                    // Note that we use a different sortition height scan point for microblocks,
                    // since we can only get microblocks once we have both the block that produced
                    // them as well as the block that confirms them.
                    while mblock_height <= max_mblock_height && (downloader.microblocks_to_try.len() as u64) < downloader.max_inflight_requests {
                        if !next_microblocks_to_try.contains_key(&mblock_height) {
                            mblock_height += 1;
                            continue;
                        }

                        if downloader.microblocks_to_try.contains_key(&mblock_height) {
                            mblock_height += 1;
                            debug!("Microblocks download already in-flight for {}", height);
                            continue;
                        }
                        
                        let requests = next_microblocks_to_try.remove(&mblock_height).expect("BUG: hashmap both contains and does not contain sortition height");
                        if requests.len() == 0 {
                            mblock_height += 1;
                            continue;
                        }
                        
                        assert_eq!(mblock_height, requests.front().as_ref().unwrap().sortition_height);

                        test_debug!("{:?}: request microblock stream produced by sortition {}: {}/{} ({})", 
                                    &network.local_peer, mblock_height, &requests.front().as_ref().unwrap().consensus_hash, &requests.front().as_ref().unwrap().anchor_block_hash, &requests.front().as_ref().unwrap().index_block_hash);

                        downloader.microblocks_to_try.insert(mblock_height, requests);

                        mblock_height += 1;
                    }

                    debug!("{:?}: block download scan now at ({},{}) (was ({},{}))", &network.local_peer, height, mblock_height, block_sortition_height, microblock_sortition_height);
                    
                    if max_height <= next_block_sortition_height && max_mblock_height <= next_microblock_sortition_height {
                        test_debug!("{:?}: no more requests to make", &network.local_peer);
                        break;
                    }

                    // restart next scan at this height
                    next_block_sortition_height = height;
                    next_microblock_sortition_height = mblock_height;

                    // at capacity?
                    if (downloader.blocks_to_try.len() as u64) >= downloader.max_inflight_requests || (downloader.microblocks_to_try.len() as u64) >= downloader.max_inflight_requests {
                        test_debug!("{:?}: queued up {} requests (blocks so far: {}, microblocks so far: {})", &network.local_peer, downloader.blocks_to_try.len(), downloader.blocks_to_try.len(), downloader.microblocks_to_try.len());
                        break;
                    }
                }

                if downloader.blocks_to_try.len() == 0 && downloader.microblocks_to_try.len() == 0 {
                    // nothing in this range, so advance sortition range to try for next time 
                    next_block_sortition_height = next_block_sortition_height + (network.burnchain.pox_constants.reward_cycle_length as u64);
                    next_microblock_sortition_height = next_microblock_sortition_height + (network.burnchain.pox_constants.reward_cycle_length as u64);

                    test_debug!("{:?}: Pessimistically increase block and microblock sortition heights to ({},{})", &network.local_peer, next_block_sortition_height, next_microblock_sortition_height);
                }

                downloader.next_block_sortition_height = next_block_sortition_height;
                downloader.next_microblock_sortition_height = next_microblock_sortition_height;

                debug!("{:?}: Will try for {} blocks and {} microblocks (next sortition heights are {},{}, chain tip is {})", 
                        &network.local_peer, downloader.blocks_to_try.len(), downloader.microblocks_to_try.len(), next_block_sortition_height, next_microblock_sortition_height, network.chain_view.burn_block_height - sortdb.first_block_height);
                Ok(())
            })?;
        }
        else {
            test_debug!("{:?}: does NOT need blocks", &self.local_peer);
        }

        PeerNetwork::with_downloader_state(self, |ref mut network, ref mut downloader| {
            let mut urlset = HashSet::new();
            for (_, requests) in downloader.blocks_to_try.iter() {
                for request in requests.iter() {
                    urlset.insert(request.data_url.clone());
                }
            }
            
            for (_, requests) in downloader.microblocks_to_try.iter() {
                for request in requests.iter() {
                    urlset.insert(request.data_url.clone());
                }
            }

            let mut urls = vec![];
            for url in urlset.drain() {
                urls.push(url);
            }
            
            downloader.dns_lookups_begin(&network.pox_id, dns_client, urls)
        })
    }

    /// Finish resolving URLs to their IP addresses 
    pub fn block_dns_lookups_try_finish(&mut self, dns_client: &mut DNSClient) -> Result<bool, net_error> {
        test_debug!("{:?}: block_dns_lookups_try_finish", &self.local_peer);
        PeerNetwork::with_downloader_state(self, |ref mut _network, ref mut downloader| {
            downloader.dns_lookups_try_finish(dns_client)
        })
    }

    fn connect_or_send_http_request(&mut self, data_url: UrlString, addr: SocketAddr, request: HttpRequestType, chainstate: &mut StacksChainState) -> Result<usize, net_error> {
        PeerNetwork::with_network_state(self, |ref mut network, ref mut network_state| {
            match network.http.connect_http(network_state, data_url.clone(), addr.clone(), Some(request.clone())) {
                Ok(event_id) => Ok(event_id),
                Err(net_error::AlreadyConnected(event_id, _)) => {
                    match network.http.get_conversation_and_socket(event_id) {
                        (Some(ref mut convo), Some(ref mut socket)) => {
                            convo.send_request(request)?;
                            HttpPeer::saturate_http_socket(socket, convo, chainstate)?;
                            Ok(event_id)
                        },
                        (_, _) => {
                            debug!("HTTP failed to connect to {:?}, {:?}", &data_url, &addr);
                            Err(net_error::PeerNotConnected)
                        }
                    }
                },
                Err(e) => {
                    return Err(e);
                }
            }
        })
    }

    /// Start a request, given the list of request keys to consider.  Use the given request_factory to
    /// create the HTTP request.  Pops requests off the front of request_keys, and returns once it successfully
    /// sends out a request via the HTTP peer.  Returns the event ID in the http peer that's
    /// handling the request.
    fn begin_request<F>(network: &mut PeerNetwork, dns_lookups: &HashMap<UrlString, Option<Vec<SocketAddr>>>, request_name: &str, request_keys: &mut VecDeque<BlockRequestKey>, chainstate: &mut StacksChainState, request_factory: F) -> Option<(BlockRequestKey, usize)> 
    where
        F: Fn(PeerHost, StacksBlockId) -> HttpRequestType
    {
        loop {
            match request_keys.pop_front() {
                Some(key) => {
                    if let Some(Some(ref sockaddrs)) = dns_lookups.get(&key.data_url) {
                        assert!(sockaddrs.len() > 0);

                        let peerhost = match PeerHost::try_from_url(&key.data_url) {
                            Some(ph) => ph,
                            None => {
                                warn!("Unparseable URL {:?}", &key.data_url);
                                continue;
                            }
                        };

                        for addr in sockaddrs.iter() {
                            let request = request_factory(peerhost.clone(), key.index_block_hash.clone());
                            match network.connect_or_send_http_request(key.data_url.clone(), addr.clone(), request, chainstate) {
                                Ok(handle) => {
                                    debug!("{:?}: Begin HTTP request for {} {} to {:?} ({:?})", &network.local_peer, request_name, &key.index_block_hash, &key.neighbor, &key.data_url);
                                    return Some((key, handle));
                                }
                                Err(e) => {
                                    debug!("{:?}: Failed to connect or send HTTP request for {} to {:?} ({:?}, {:?}): {:?}", &network.local_peer, request_name, &key.neighbor, &key.data_url, addr, &e);
                                }
                            }
                        }

                        debug!("{:?}: Failed request for {} {:?} from {:?}", &network.local_peer, request_name, &key.index_block_hash, sockaddrs);
                    }
                    else {
                        debug!("{:?}: Will not request {} {:?}: failed to look up DNS name in {:?}", &network.local_peer, request_name, &key.index_block_hash, &key.data_url);
                    }
                },
                None => {
                    debug!("{:?}: No more requests keys", &network.local_peer);
                    break;
                }
            }
        }
        None
    }


    /// Start fetching blocks
    pub fn block_getblocks_begin(&mut self, chainstate: &mut StacksChainState) -> Result<(), net_error> {
        test_debug!("{:?}: block_getblocks_begin", &self.local_peer);
        PeerNetwork::with_downloader_state(self, |ref mut network, ref mut downloader| {
            let mut priority = PeerNetwork::prioritize_requests(&downloader.blocks_to_try);
            let mut requests = HashMap::new();
            for sortition_height in priority.drain(..) {
                match downloader.blocks_to_try.get_mut(&sortition_height) {
                    Some(ref mut keys) => {
                        match PeerNetwork::begin_request(network, &downloader.dns_lookups, "anchored block", keys, chainstate, |peerhost, index_block_hash| HttpRequestType::GetBlock(HttpRequestMetadata::from_host(peerhost), index_block_hash)) {
                            Some((key, handle)) => {
                                requests.insert(key.clone(), handle);
                            },
                            None => {}
                        }
                    },
                    None => {
                        debug!("{:?}: No block at sortition height {}", &network.local_peer, sortition_height);
                    }
                }
            }

            downloader.getblocks_begin(requests);
            Ok(())
        })
    }

    /// Try to see if all blocks are finished downloading
    pub fn block_getblocks_try_finish(&mut self) -> Result<bool, net_error> {
        test_debug!("{:?}: block_getblocks_try_finish", &self.local_peer);
        PeerNetwork::with_downloader_state(self, |ref mut network, ref mut downloader| {
            downloader.getblocks_try_finish(&mut network.http)
        })
    }

    /// Proceed to get microblocks 
    pub fn block_getmicroblocks_begin(&mut self, chainstate: &mut StacksChainState) -> Result<(), net_error> {
        test_debug!("{:?}: block_getmicroblocks_begin", &self.local_peer);
        PeerNetwork::with_downloader_state(self, |ref mut network, ref mut downloader| {
            let mut priority = PeerNetwork::prioritize_requests(&downloader.microblocks_to_try);
            let mut requests = HashMap::new();
            for sortition_height in priority.drain(..) {
                match downloader.microblocks_to_try.get_mut(&sortition_height) {
                    Some(ref mut keys) => {
                        match PeerNetwork::begin_request(network, &downloader.dns_lookups, "microblock stream", keys, chainstate, |peerhost, index_block_hash| HttpRequestType::GetMicroblocksConfirmed(HttpRequestMetadata::from_host(peerhost), index_block_hash)) {
                            Some((key, handle)) => {
                                requests.insert(key.clone(), handle);
                            },
                            None => {}
                        }
                    },
                    None => {
                        debug!("{:?}: No microblocks at sortition height {}", &network.local_peer, sortition_height);
                    }
                }
            }

            downloader.getmicroblocks_begin(requests);
            Ok(())
        })
    }
    
    /// Try to see if all microblocks are finished downloading
    pub fn block_getmicroblocks_try_finish(&mut self) -> Result<bool, net_error> {
        test_debug!("{:?}: block_getmicroblocks_try_finish", &self.local_peer);
        PeerNetwork::with_downloader_state(self, |ref mut network, ref mut downloader| {
            downloader.getmicroblocks_try_finish(&mut network.http)
        })
    }

    /// Process newly-fetched blocks and microblocks.
    /// Returns true if we've completed all requests.
    /// Returns (done?, blocks-we-got, microblocks-we-got) on success
    fn finish_downloads(&mut self, sortdb: &SortitionDB, chainstate: &mut StacksChainState) -> Result<(bool, Option<PoxId>, Vec<(ConsensusHash, StacksBlock)>, Vec<(ConsensusHash, Vec<StacksMicroblock>)>), net_error> {
        let mut blocks = vec![];
        let mut microblocks = vec![];
        let mut done = false;
        let mut old_pox_id = None;

        PeerNetwork::with_downloader_state(self, |ref mut network, ref mut downloader| {
            // extract blocks and microblocks downloaded
            for (request_key, block) in downloader.blocks.drain() {
                debug!("Downloaded block {}/{} ({}) at sortition height {}", &request_key.consensus_hash, &request_key.anchor_block_hash, &request_key.index_block_hash, request_key.sortition_height);
                blocks.push((request_key.consensus_hash.clone(), block));
                downloader.num_blocks_downloaded += 1;

                // don't try this again
                downloader.blocks_to_try.remove(&request_key.sortition_height);
                downloader.blocks_downloaded.insert(request_key.index_block_hash.clone());
            }
            for (request_key, microblock_stream) in downloader.microblocks.drain() {
                let block_header = StacksChainState::load_block_header(&chainstate.blocks_path, &request_key.consensus_hash, &request_key.anchor_block_hash)? 
                    .expect(&format!("BUG: missing Stacks block header for {}/{}", &request_key.consensus_hash, &request_key.anchor_block_hash));

                assert!(request_key.child_block_header.is_some(), "BUG: requested a microblock but didn't set the child block header");
                let child_block_header = request_key.child_block_header.unwrap();

                if StacksChainState::validate_parent_microblock_stream(&block_header, &child_block_header, &microblock_stream, true).is_some() {
                    // stream is valid!
                    debug!("Downloaded valid microblock stream {}/{} at sortition height {}", &request_key.consensus_hash, &request_key.anchor_block_hash, request_key.sortition_height);
                    microblocks.push((request_key.consensus_hash.clone(), microblock_stream));
                    downloader.num_microblocks_downloaded += 1;
                }
                else {
                    // stream is not well-formed
                    debug!("Microblock stream {:?}: {}/{} is invalid", request_key.sortition_height, &request_key.consensus_hash, &request_key.anchor_block_hash);
                }

                // don't try again
                downloader.microblocks_to_try.remove(&request_key.sortition_height);
                downloader.microblocks_downloaded.insert(request_key.index_block_hash.clone());
            }

            // clear empties
            let mut blocks_empty = vec![];
            let mut microblocks_empty = vec![];
            for (height, requests) in downloader.blocks_to_try.iter() {
                if requests.len() == 0 {
                    blocks_empty.push(*height);
                }
            }
            for (height, requests) in downloader.microblocks_to_try.iter() {
                if requests.len() == 0 {
                    microblocks_empty.push(*height);
                }
            }

            for height in blocks_empty.drain(..) {
                downloader.blocks_to_try.remove(&height);
            }

            for height in microblocks_empty.drain(..) {
                downloader.microblocks_to_try.remove(&height);
            }

            debug!("Blocks to try: {}; Microblocks to try: {}", downloader.blocks_to_try.len(), downloader.microblocks_to_try.len());
            if downloader.blocks_to_try.len() == 0 && downloader.microblocks_to_try.len() == 0 {
                // advance downloader state
                done = true;

                debug!("{:?}: Advance downloader to start at sortition heights {},{}", &network.local_peer, downloader.next_block_sortition_height, downloader.next_microblock_sortition_height);
                downloader.block_sortition_height = downloader.next_block_sortition_height;
                downloader.microblock_sortition_height = downloader.next_microblock_sortition_height;

                if downloader.block_sortition_height + sortdb.first_block_height >= network.chain_view.burn_block_height {
                    debug!("{:?}: Downloader for blocks has reached the chain tip", &network.local_peer);
                    downloader.block_sortition_height = 0;
                    downloader.next_block_sortition_height = 0;

                    if downloader.num_blocks_downloaded == 0 {
                        downloader.empty_block_download_passes += 1;
                    }
                    else {
                        downloader.empty_block_download_passes = 0;
                    }

                    downloader.num_blocks_downloaded = 0;
                }
                if downloader.microblock_sortition_height + sortdb.first_block_height >= network.chain_view.burn_block_height {
                    debug!("{:?}: Downloader for microblocks has reached the chain tip", &network.local_peer);
                    downloader.microblock_sortition_height = 0;
                    downloader.next_microblock_sortition_height = 0;
                    
                    if downloader.num_microblocks_downloaded == 0 {
                        downloader.empty_microblock_download_passes += 1;
                    }
                    else {
                        downloader.empty_microblock_download_passes = 0;
                    }
                    
                    downloader.num_microblocks_downloaded = 0;
                }

                if downloader.empty_block_download_passes > 0 && downloader.empty_microblock_download_passes > 0 {
                    // we scanned the entire chain and didn't download anything.
                    // Either we have everything already, or none of our peers have anything we don't have, or we can't reach any of our peers.
                    // Regardless, we can throttle back now.
                    debug!("Did a full pass over the burn chain sortitions and found no new data");
                    downloader.finished_scan_at = get_epoch_time_secs();
                }

                // propagate PoX ID as it was when we started
                old_pox_id = Some(downloader.pox_id.clone());
            }
            else {
                // still have different URLs to try for failed blocks.
                done = false;
                debug!("Re-trying blocks:");
                for (height, requests) in downloader.blocks_to_try.iter() {
                    assert!(requests.len() > 0, format!("Empty block requests at height {}", height));
                    debug!("   Height {}: anchored block {} available from {} peers", height, requests.front().unwrap().index_block_hash, requests.len());
                }
                for (height, requests) in downloader.microblocks_to_try.iter() {
                    assert!(requests.len() > 0, format!("Empty microblock requests at height {}", height));
                    debug!("   Height {}: microblocks {} available from {} peers", height, requests.front().unwrap().index_block_hash, requests.len());
                }

                downloader.state = BlockDownloaderState::GetBlocksBegin;
            }

            Ok((done, old_pox_id, blocks, microblocks))
        })
    }

    /// Initialize the downloader 
    pub fn init_block_downloader(&mut self) -> () {
        self.block_downloader = Some(BlockDownloader::new(self.connection_opts.dns_timeout, self.connection_opts.download_interval, self.connection_opts.max_inflight_blocks));
    }

    /// Process block downloader lifetime.  Returns the new blocks and microblocks if we get
    /// anything.
    /// Returns true/false if we're done, as well as any blocks and microblocks we got, as well as
    /// broken http and p2p neighbors we encountered (so the main loop can disconnect them)
    pub fn download_blocks(&mut self, sortdb: &SortitionDB, chainstate: &mut StacksChainState, dns_client: &mut DNSClient) -> Result<(bool, Option<PoxId>, Vec<(ConsensusHash, StacksBlock)>, Vec<(ConsensusHash, Vec<StacksMicroblock>)>, Vec<usize>, Vec<NeighborKey>), net_error> {
        if self.inv_state.is_none() {
            test_debug!("{:?}: Inv state not initialized yet", &self.local_peer);
            return Err(net_error::NotConnected);
        }

        if self.block_downloader.is_none() {
            self.init_block_downloader();
        }

        let last_inv_update_at = self.inv_state.as_ref().unwrap().last_change_at;

        match self.block_downloader {
            Some(ref mut downloader) => {
                if downloader.empty_block_download_passes > 0 && downloader.empty_microblock_download_passes > 0 {
                    if downloader.last_inv_update_at == last_inv_update_at && downloader.finished_scan_at + downloader.download_interval >= get_epoch_time_secs() {
                        // throttle ourselves
                        debug!("{:?}: Throttle block downloads until {}", &self.local_peer, downloader.finished_scan_at + downloader.download_interval);
                        return Ok((true, None, vec![], vec![], vec![], vec![]));
                    }
                    else {
                        // start a rescan -- we've waited long enough
                        debug!("{:?}: Noticed an inventory change; re-starting a download scan", &self.local_peer);
                        downloader.restart_scan();
                
                        downloader.last_inv_update_at = last_inv_update_at;
                    }
                }
                else {
                    downloader.last_inv_update_at = last_inv_update_at;
                }
            },
            None => {
                unreachable!();
            }
        }

        let mut done = false;

        let mut blocks = vec![];
        let mut microblocks = vec![];
        let mut old_pox_id = None;

        let mut done_cycle = false;
        while !done_cycle {
            let dlstate = self.block_downloader.as_ref().unwrap().state;

            debug!("{:?}: Download state is {:?}", &self.local_peer, &dlstate);
            match dlstate {
                BlockDownloaderState::DNSLookupBegin => {
                    self.block_dns_lookups_begin(sortdb, chainstate, dns_client)?;
                },
                BlockDownloaderState::DNSLookupFinish => {
                    self.block_dns_lookups_try_finish(dns_client)?;
                },
                BlockDownloaderState::GetBlocksBegin => {
                    self.block_getblocks_begin(chainstate)?;
                },
                BlockDownloaderState::GetBlocksFinish => {
                    self.block_getblocks_try_finish()?;
                },
                BlockDownloaderState::GetMicroblocksBegin => {
                    self.block_getmicroblocks_begin(chainstate)?;
                },
                BlockDownloaderState::GetMicroblocksFinish => {
                    self.block_getmicroblocks_try_finish()?;
                },
                BlockDownloaderState::Done => {
                    // did a pass.
                    // do we have more requests?
                    let (blocks_done, downloader_pox_id, mut successful_blocks, mut successful_microblocks) = self.finish_downloads(sortdb, chainstate)?;

                    old_pox_id = downloader_pox_id;
                    blocks.append(&mut successful_blocks);
                    microblocks.append(&mut successful_microblocks);
                    done = blocks_done;

                    done_cycle = true;
                }
            }
        
            let new_dlstate = self.block_downloader.as_ref().unwrap().state;
            if new_dlstate == dlstate {
                done_cycle = true;
            }
        }

        // remove dead/broken peers
        let (broken_http_peers, broken_p2p_peers) = match self.block_downloader {
            Some(ref mut downloader) => downloader.clear_broken_peers(),
            None => (vec![], vec![])
        };

        if done {
            // reset state if we're done
            match self.block_downloader {
                Some(ref mut downloader) => downloader.reset(),
                None => {}
            }
        }

        Ok((done, old_pox_id, blocks, microblocks, broken_http_peers, broken_p2p_peers))
    }
}

#[cfg(test)]
pub mod test {
    use super::*;
    use net::*;
    use net::codec::*;
    use net::inv::*;
    use net::test::*;
    use net::relay::*;
    use chainstate::stacks::*;
    use std::collections::HashMap;
    use chainstate::burn::db::sortdb::*;
    use util::test::*;

    fn get_peer_availability(peer: &mut TestPeer, start_height: u64, end_height: u64) -> Vec<(ConsensusHash, Option<BlockHeaderHash>, Vec<NeighborKey>)> {
        let inv_state = peer.network.inv_state.take().unwrap();
        let availability = peer.with_network_state(|ref mut sortdb, ref mut _chainstate, ref mut network, ref mut _relayer, ref mut _mempool| {
            BlockDownloader::get_block_availability(&inv_state, sortdb, &mut network.header_cache, start_height, end_height)
        }).unwrap();
        peer.network.inv_state = Some(inv_state);
        availability
    }
    
    #[test]
    fn test_get_block_availability() {
        with_timeout(600, || {
            let mut peer_1_config = TestPeerConfig::new("test_get_block_availability", 3210, 3211);
            let mut peer_2_config = TestPeerConfig::new("test_get_block_availability", 3212, 3213);

            // don't bother downloading blocks
            peer_1_config.connection_opts.disable_block_download = true;
            peer_2_config.connection_opts.disable_block_download = true;
            
            peer_1_config.add_neighbor(&peer_2_config.to_neighbor());
            peer_2_config.add_neighbor(&peer_1_config.to_neighbor());

            let reward_cycle_length = peer_1_config.burnchain.pox_constants.reward_cycle_length as u64;

            let mut peer_1 = TestPeer::new(peer_1_config);
            let mut peer_2 = TestPeer::new(peer_2_config);

            let num_blocks = 10;
            let first_stacks_block_height = {
                let sn = SortitionDB::get_canonical_burn_chain_tip(&peer_1.sortdb.as_ref().unwrap().conn()).unwrap();
                sn.block_height
            };

            let mut block_data = vec![];

            for i in 0..num_blocks {
                let (mut burn_ops, stacks_block, microblocks) = peer_2.make_default_tenure();

                let (_, burn_header_hash, consensus_hash) = peer_2.next_burnchain_block(burn_ops.clone());
                peer_2.process_stacks_epoch_at_tip(&stacks_block, &microblocks);

                TestPeer::set_ops_burn_header_hash(&mut burn_ops, &burn_header_hash);

                peer_1.next_burnchain_block_raw(burn_ops);

                let sn = SortitionDB::get_canonical_burn_chain_tip(&peer_2.sortdb.as_ref().unwrap().conn()).unwrap();
                block_data.push((sn.consensus_hash.clone(), stacks_block, microblocks));

                /*
                let (burn_ops, stacks_block, microblocks) = peer_2.make_default_tenure();
                peer_1.next_burnchain_block(burn_ops.clone());
                peer_2.next_burnchain_block(burn_ops.clone());
                peer_2.process_stacks_epoch_at_tip(&stacks_block, &microblocks);

                let sn = SortitionDB::get_canonical_burn_chain_tip(&peer_2.sortdb.as_ref().unwrap().conn()).unwrap();
                block_data.push((sn.consensus_hash.clone(), stacks_block, microblocks));
                */
            }

            let num_burn_blocks = {
                let sn = SortitionDB::get_canonical_burn_chain_tip(peer_1.sortdb.as_ref().unwrap().conn()).unwrap();
                sn.block_height - peer_1.config.burnchain.first_block_height
            };
            
            let mut round = 0;
            let mut inv_1_count = 0;
            let mut inv_2_count = 0;
            let mut all_blocks_available = false;

            // can only learn about 1 reward cycle's blocks at a time in PoX
            while inv_1_count < reward_cycle_length && inv_2_count < reward_cycle_length && !all_blocks_available {
                let result_1 = peer_1.step();
                let result_2 = peer_2.step();

                inv_1_count = match peer_1.network.inv_state {
                    Some(ref inv) => {
                        let mut count = inv.get_inv_sortitions(&peer_2.to_neighbor().addr);

                        // continue until peer 1 knows that peer 2 has blocks
                        let peer_1_availability = get_peer_availability(&mut peer_1, first_stacks_block_height, first_stacks_block_height + reward_cycle_length);

                        let mut all_availability = true;
                        for (_, _, neighbors) in peer_1_availability.iter() {
                            if neighbors.len() != 1 {
                                // not done yet
                                count = 0;
                                all_availability = false;
                                break;
                            }
                            assert_eq!(neighbors[0], peer_2.config.to_neighbor().addr);
                        }

                        all_blocks_available = all_availability;

                        count
                    },
                    None => 0
                };

                inv_2_count = match peer_2.network.inv_state {
                    Some(ref inv) => inv.get_inv_sortitions(&peer_1.to_neighbor().addr),
                    None => 0
                };

                // nothing should break
                match peer_1.network.inv_state {
                    Some(ref inv) => {
                        assert_eq!(inv.get_broken_peers().len(), 0);
                        assert_eq!(inv.get_diverged_peers().len(), 0);

                    },
                    None => {}
                }

                match peer_2.network.inv_state {
                    Some(ref inv) => {
                        assert_eq!(inv.get_broken_peers().len(), 0);
                        assert_eq!(inv.get_diverged_peers().len(), 0);
                    },
                    None => {}
                }


                round += 1;
            }

            info!("Completed walk round {} step(s)", round);
           
            let availability = get_peer_availability(&mut peer_1, first_stacks_block_height, first_stacks_block_height + reward_cycle_length);

            eprintln!("availability.len() == {}", availability.len());
            eprintln!("block_data.len() == {}", block_data.len());
            
            assert_eq!(availability.len() as u64, reward_cycle_length);
            assert_eq!(block_data.len() as u64, num_blocks);

            for ((sn_consensus_hash, stacks_block, microblocks), (consensus_hash, stacks_block_hash_opt, neighbors)) in block_data.iter().zip(availability.iter()) {
                assert_eq!(*consensus_hash, *sn_consensus_hash);
                assert!(stacks_block_hash_opt.is_some());
                assert_eq!(*stacks_block_hash_opt, Some(stacks_block.block_hash()));
            }
        })
    }
   
    fn get_blocks_inventory(peer: &mut TestPeer, start_height: u64, end_height: u64) -> BlocksInvData {
        let block_hashes = {
            let num_headers = end_height - start_height;
            let ic = peer.sortdb.as_mut().unwrap().index_conn();
            let tip = SortitionDB::get_canonical_burn_chain_tip(&ic).unwrap();
            let ancestor = SortitionDB::get_ancestor_snapshot(&ic, end_height, &tip.sortition_id).unwrap().unwrap();
            ic.get_stacks_header_hashes(num_headers, &ancestor.consensus_hash, &mut BlockHeaderCache::new()).unwrap()
        };

        let inv = peer.chainstate().get_blocks_inventory(&block_hashes).unwrap();
        inv
    }
    
    pub fn run_get_blocks_and_microblocks<T, F, P, C, D>(test_name: &str, port_base: u16, num_peers: usize, make_topology: T, block_generator: F, mut peer_func: P, mut check_breakage: C, mut done_func: D) -> Vec<TestPeer>
    where 
        T: FnOnce(&mut Vec<TestPeerConfig>) -> (),
        F: FnOnce(usize, &mut Vec<TestPeer>) -> Vec<(ConsensusHash, Option<StacksBlock>, Option<Vec<StacksMicroblock>>)>,
        P: FnMut(&mut Vec<TestPeer>) -> (),
        C: FnMut(&mut TestPeer) -> bool,
        D: FnMut(&mut Vec<TestPeer>) -> bool
    {
        assert!(num_peers > 0);
        let first_sortition_height = 0;

        let mut peer_configs = vec![];
        for i in 0..num_peers {
            let mut peer_config = TestPeerConfig::new(test_name, port_base + ((2*i) as u16), port_base + ((2*i+1) as u16));
            peer_config.burnchain.first_block_height = first_sortition_height;

            peer_configs.push(peer_config);
        }

        make_topology(&mut peer_configs);

        let mut peers = vec![];
        for conf in peer_configs.drain(..) {
            let peer = TestPeer::new(conf);
            peers.push(peer);
        }

        let mut num_blocks = 10;
        let first_stacks_block_height = {
            let sn = SortitionDB::get_canonical_burn_chain_tip(&peers[0].sortdb.as_ref().unwrap().conn()).unwrap();
            sn.block_height
        };

        let block_data = block_generator(num_blocks, &mut peers);
        num_blocks = block_data.len();

        let num_burn_blocks = {
            let sn = SortitionDB::get_canonical_burn_chain_tip(peers[0].sortdb.as_ref().unwrap().conn()).unwrap();
            sn.block_height
        };

        let mut dns_clients = vec![];
        let mut dns_threads = vec![];

        for _ in 0..peers.len() {
            let (dns_client, dns_thread_handle) = dns_thread_start(100);
            dns_clients.push(dns_client);
            dns_threads.push(dns_thread_handle);
        }
        
        let mut round = 0;
        let mut peer_invs = vec![BlocksInvData::empty(); num_peers];

        let mut done = false;

        loop {
            peer_func(&mut peers);

            for i in 0..peers.len() {
                let peer = &mut peers[i];
                
                test_debug!("======= peer {} step begin =========", i);
                let mut result = peer.step_dns(&mut dns_clients[i]).unwrap();

                let lp = peer.network.local_peer.clone();
                peer.with_db_state(|sortdb, chainstate, relayer, mempool| {
                    relayer.process_network_result(&lp, &mut result, sortdb, chainstate, mempool, None)
                }).unwrap();

                test_debug!("Peer {} processes {} blocks and {} microblock streams", i, result.blocks.len(), result.confirmed_microblocks.len());

                peer.with_peer_state(|peer, sortdb, chainstate, mempool| {
                    for i in 0..(result.blocks.len() + result.confirmed_microblocks.len() + 1) {
                        peer.coord.handle_new_stacks_block().unwrap();

                        let pox_id = {
                            let ic = sortdb.index_conn();
                            let tip_sort_id = SortitionDB::get_canonical_sortition_tip(sortdb.conn()).unwrap();
                            let sortdb_reader = SortitionHandleConn::open_reader(&ic, &tip_sort_id).unwrap();
                            sortdb_reader.get_pox_id().unwrap()
                        };

                        test_debug!("\n\n{:?}: after stacks block, new tip PoX ID is {:?}\n\n", &peer.to_neighbor().addr, &pox_id);
                    }
                    Ok(())
                }).unwrap();

                assert!(check_breakage(peer));

                peer_invs[i] = get_blocks_inventory(peer, 0, num_burn_blocks);
                test_debug!("Peer {} block inventory: {:?}", i, &peer_invs[i]);

                if let Some(ref inv) = peer.network.inv_state {
                    test_debug!("Peer {} inventory stats: {:?}", i, &inv.block_stats);
                }

                let (mut inbound, mut outbound) = peer.network.dump_peer_table();

                inbound.sort();
                outbound.sort();

                test_debug!("Peer {} outbound ({}): {}", i, outbound.len(), outbound.join(", "));
                test_debug!("Peer {} inbound ({}):  {}", i, inbound.len(), inbound.join(", "));
                test_debug!("======= peer {} step end   =========", i);
            }

            if !done {
                done = true;
                for i in 0..num_peers {
                    for b in 0..num_blocks {
                        if !peer_invs[i].has_ith_block(((b as u64) + first_stacks_block_height - first_sortition_height) as u16) {
                            if block_data[b].1.is_some() {
                                test_debug!("Peer {} is missing block {}", i, (b as u64) + first_stacks_block_height - first_sortition_height);
                                done = false;
                            }
                        }
                    }
                    for b in 0..(num_blocks - 1) {
                        if !peer_invs[i].has_ith_microblock_stream(((b as u64) + first_stacks_block_height - first_sortition_height) as u16) {
                            if block_data[b].2.is_some() {
                                test_debug!("Peer {} is missing microblock stream {}", i, (b as u64) + first_stacks_block_height - first_sortition_height);
                                done = false;
                            }
                        }
                    }
                }
            }

            if done {
                // all blocks obtained, now do custom check
                if done_func(&mut peers) {
                    break;
                }
            }
            
            round += 1;
        }

        info!("Completed walk round {} step(s)", round);
     
        let mut peer_invs = vec![];
        for peer in peers.iter_mut() {
            let peer_inv = get_blocks_inventory(peer, 0, num_burn_blocks);
            peer_invs.push(peer_inv);

            let availability = get_peer_availability(peer, first_stacks_block_height - first_sortition_height, first_stacks_block_height - first_sortition_height + (num_blocks as u64));
            
            assert_eq!(availability.len(), num_blocks);
            assert_eq!(block_data.len(), num_blocks);

            for ((sn_consensus_hash, stacks_block_opt, microblocks_opt), (consensus_hash, stacks_block_hash_opt, neighbors)) in block_data.iter().zip(availability.iter()) {
                assert_eq!(*consensus_hash, *sn_consensus_hash);

                if stacks_block_hash_opt.is_some() {
                    assert!(stacks_block_opt.is_some());
                    assert_eq!(*stacks_block_hash_opt, Some(stacks_block_opt.as_ref().unwrap().block_hash()));
                }
                else {
                    assert!(stacks_block_opt.is_none());
                }
            }
        }

        drop(dns_clients);
        for handle in dns_threads.drain(..) {
            handle.join().unwrap();
        }

        peers
    }

    #[test]
    #[ignore]
    pub fn test_get_blocks_and_microblocks_2_peers_download() {
        with_timeout(600, || {
            run_get_blocks_and_microblocks("test_get_blocks_and_microblocks_2_peers_download", 3200, 2,
                                           |ref mut peer_configs| {
                                               // build initial network topology
                                               assert_eq!(peer_configs.len(), 2);

                                               peer_configs[0].connection_opts.disable_block_advertisement = true;
                                               peer_configs[1].connection_opts.disable_block_advertisement = true;

                                               let peer_0 = peer_configs[0].to_neighbor();
                                               let peer_1 = peer_configs[1].to_neighbor();
                                               peer_configs[0].add_neighbor(&peer_1);
                                               peer_configs[1].add_neighbor(&peer_0);
                                           },
                                           |num_blocks, ref mut peers| {
                                               // build up block data to replicate
                                               let mut block_data = vec![];
                                               for _ in 0..num_blocks {
                                                   let (mut burn_ops, stacks_block, microblocks) = peers[1].make_default_tenure();

                                                   let (_, burn_header_hash, consensus_hash) = peers[1].next_burnchain_block(burn_ops.clone());
                                                   peers[1].process_stacks_epoch_at_tip(&stacks_block, &microblocks);

                                                   TestPeer::set_ops_burn_header_hash(&mut burn_ops, &burn_header_hash);

                                                   peers[0].next_burnchain_block_raw(burn_ops);

                                                   let sn = SortitionDB::get_canonical_burn_chain_tip(&peers[1].sortdb.as_ref().unwrap().conn()).unwrap();
                                                   block_data.push((sn.consensus_hash.clone(), Some(stacks_block), Some(microblocks)));
                                               }
                                               block_data
                                           },
                                           |_| {},
                                           |peer| {
                                               // check peer health
                                               // nothing should break 
                                               match peer.network.block_downloader {
                                                   Some(ref dl) => {
                                                       assert_eq!(dl.broken_peers.len(), 0);
                                                       assert_eq!(dl.dead_peers.len(), 0);
                                                   },
                                                   None => {}
                                               }

                                               // no block advertisements (should be disabled)
                                               let _ = peer.for_each_convo_p2p(|event_id, convo| {
                                                   let cnt = *(convo.stats.msg_rx_counts.get(&StacksMessageID::BlocksAvailable).unwrap_or(&0));
                                                   assert_eq!(cnt, 0, "neighbor event={} got {} BlocksAvailable messages", event_id, cnt);
                                                   Ok(())
                                               });

                                               true
                                           },
                                           |_| true);
        })
    }
   
    // TODO: hint on PoX inv change to advance downloader?
    #[test]
    #[ignore]
    pub fn test_get_blocks_and_microblocks_5_peers_star() {
        with_timeout(600, || {
            run_get_blocks_and_microblocks("test_get_blocks_and_microblocks_5_peers_star", 3210, 5,
                                           |ref mut peer_configs| {
                                               // build initial network topology -- a star with
                                               // peers[0] at the center, with all the blocks
                                               assert_eq!(peer_configs.len(), 5);
                                               let mut neighbors = vec![];

                                               for p in peer_configs.iter_mut() {
                                                   p.connection_opts.disable_block_advertisement = true;
                                                   p.connection_opts.max_clients_per_host = 30;
                                               }
                                               
                                               let peer_0 = peer_configs[0].to_neighbor();
                                               for i in 1..peer_configs.len() {
                                                   neighbors.push(peer_configs[i].to_neighbor());
                                                   peer_configs[i].add_neighbor(&peer_0);
                                               }

                                               for n in neighbors.drain(..) {
                                                   peer_configs[0].add_neighbor(&n);
                                               }
                                           },
                                           |num_blocks, ref mut peers| {
                                               // build up block data to replicate
                                               let mut block_data = vec![];
                                               for _ in 0..num_blocks {
                                                   let (mut burn_ops, stacks_block, microblocks) = peers[0].make_default_tenure();

                                                   let (_, burn_header_hash, consensus_hash) = peers[0].next_burnchain_block(burn_ops.clone());
                                                   peers[0].process_stacks_epoch_at_tip(&stacks_block, &microblocks);

                                                   TestPeer::set_ops_burn_header_hash(&mut burn_ops, &burn_header_hash);

                                                   for i in 1..peers.len() {
                                                        peers[i].next_burnchain_block_raw(burn_ops.clone());
                                                   }

                                                   let sn = SortitionDB::get_canonical_burn_chain_tip(&peers[0].sortdb.as_ref().unwrap().conn()).unwrap();
                                                   block_data.push((sn.consensus_hash.clone(), Some(stacks_block), Some(microblocks)));
                                               }
                                               block_data
                                           },
                                           |_| {},
                                           |peer| {
                                               // check peer health
                                               // nothing should break 
                                               match peer.network.block_downloader {
                                                   Some(ref dl) => {
                                                       assert_eq!(dl.broken_peers.len(), 0);
                                                       assert_eq!(dl.dead_peers.len(), 0);
                                                   },
                                                   None => {}
                                               }
                                               true
                                           },
                                           |_| true);
        })
    }

    #[test]
    #[ignore]
    pub fn test_get_blocks_and_microblocks_5_peers_line() {
        with_timeout(600, || {
            run_get_blocks_and_microblocks("test_get_blocks_and_microblocks_5_peers_line", 3220, 5,
                                           |ref mut peer_configs| {
                                               // build initial network topology -- a line with
                                               // peers[0] at the left, with all the blocks
                                               assert_eq!(peer_configs.len(), 5);
                                               let mut neighbors = vec![];
                                               
                                               for p in peer_configs.iter_mut() {
                                                   p.connection_opts.disable_block_advertisement = true;
                                                   p.connection_opts.max_clients_per_host = 30;
                                               }

                                               for i in 0..peer_configs.len() {
                                                   neighbors.push(peer_configs[i].to_neighbor());
                                               }

                                               for i in 0..peer_configs.len()-1 {
                                                   peer_configs[i].add_neighbor(&neighbors[i+1]);
                                                   peer_configs[i+1].add_neighbor(&neighbors[i]);
                                               }
                                           },
                                           |num_blocks, ref mut peers| {
                                               // build up block data to replicate
                                               let mut block_data = vec![];
                                               for _ in 0..num_blocks {
                                                   let (mut burn_ops, stacks_block, microblocks) = peers[0].make_default_tenure();

                                                   let (_, burn_header_hash, consensus_hash) = peers[0].next_burnchain_block(burn_ops.clone());
                                                   peers[0].process_stacks_epoch_at_tip(&stacks_block, &microblocks);

                                                   TestPeer::set_ops_burn_header_hash(&mut burn_ops, &burn_header_hash);

                                                   for i in 1..peers.len() {
                                                        peers[i].next_burnchain_block_raw(burn_ops.clone());
                                                   }

                                                   let sn = SortitionDB::get_canonical_burn_chain_tip(&peers[0].sortdb.as_ref().unwrap().conn()).unwrap();
                                                   block_data.push((sn.consensus_hash.clone(), Some(stacks_block), Some(microblocks)));
                                               }
                                               block_data
                                           },
                                           |_| {},
                                           |peer| {
                                               // check peer health
                                               // nothing should break 
                                               match peer.network.block_downloader {
                                                   Some(ref dl) => {
                                                       assert_eq!(dl.broken_peers.len(), 0);
                                                       assert_eq!(dl.dead_peers.len(), 0);
                                                   },
                                                   None => {}
                                               }
                                               true
                                           },
                                           |_| true);
        })
    }
    
    #[test]
    #[ignore]
    pub fn test_get_blocks_and_microblocks_overwhelmed_connections() {
        with_timeout(600, || {
            run_get_blocks_and_microblocks("test_get_blocks_and_microblocks_overwhelmed_connections", 3230, 5,
                                           |ref mut peer_configs| {
                                               // build initial network topology -- a star with
                                               // peers[0] at the center, with all the blocks
                                               assert_eq!(peer_configs.len(), 5);
                                               let mut neighbors = vec![];
                                               
                                               for p in peer_configs.iter_mut() {
                                                   p.connection_opts.disable_block_advertisement = true;
                                               }

                                               let peer_0 = peer_configs[0].to_neighbor();

                                               for i in 1..peer_configs.len() {
                                                   neighbors.push(peer_configs[i].to_neighbor());
                                                   peer_configs[i].add_neighbor(&peer_0);

                                                   // severely restrict the number of allowed
                                                   // connections in each peer
                                                   peer_configs[i].connection_opts.max_clients_per_host = 1;
                                                   peer_configs[i].connection_opts.num_clients = 1;
                                                   peer_configs[i].connection_opts.idle_timeout = 1;
                                               }

                                               for n in neighbors.drain(..) {
                                                   peer_configs[0].add_neighbor(&n);
                                               }
                                           },
                                           |num_blocks, ref mut peers| {
                                               // build up block data to replicate
                                               let mut block_data = vec![];
                                               for _ in 0..num_blocks {
                                                   let (mut burn_ops, stacks_block, microblocks) = peers[0].make_default_tenure();

                                                   let (_, burn_header_hash, consensus_hash) = peers[0].next_burnchain_block(burn_ops.clone());
                                                   peers[0].process_stacks_epoch_at_tip(&stacks_block, &microblocks);

                                                   TestPeer::set_ops_burn_header_hash(&mut burn_ops, &burn_header_hash);

                                                   for i in 1..peers.len() {
                                                        peers[i].next_burnchain_block_raw(burn_ops.clone());
                                                   }

                                                   let sn = SortitionDB::get_canonical_burn_chain_tip(&peers[0].sortdb.as_ref().unwrap().conn()).unwrap();
                                                   block_data.push((sn.consensus_hash.clone(), Some(stacks_block), Some(microblocks)));
                                               }
                                               block_data
                                           },
                                           |_| {},
                                           |peer| {
                                               // check peer health
                                               // nothing should break 
                                               match peer.network.block_downloader {
                                                   Some(ref dl) => {
                                                       assert_eq!(dl.broken_peers.len(), 0);
                                                       assert_eq!(dl.dead_peers.len(), 0);
                                                   },
                                                   None => {}
                                               }
                                               true
                                           },
                                           |_| true);
        })
    }
    
    #[test]
    #[ignore]
    pub fn test_get_blocks_and_microblocks_overwhelmed_sockets() {
        // this one can go for a while
        with_timeout(1200, || {
            run_get_blocks_and_microblocks("test_get_blocks_and_microblocks_overwhelmed_sockets", 3240, 5,
                                           |ref mut peer_configs| {
                                               // build initial network topology -- a star with
                                               // peers[0] at the center, with all the blocks
                                               assert_eq!(peer_configs.len(), 5);
                                               let mut neighbors = vec![];
                                               
                                               for p in peer_configs.iter_mut() {
                                                   p.connection_opts.disable_block_advertisement = true;
                                               }

                                               let peer_0 = peer_configs[0].to_neighbor();

                                               for i in 1..peer_configs.len() {
                                                   neighbors.push(peer_configs[i].to_neighbor());
                                                   peer_configs[i].add_neighbor(&peer_0);

                                                   // severely restrict the number of events
                                                   peer_configs[i].connection_opts.max_sockets = 10;
                                               }

                                               for n in neighbors.drain(..) {
                                                   peer_configs[0].add_neighbor(&n);
                                               }
                                           },
                                           |num_blocks, ref mut peers| {
                                               // build up block data to replicate
                                               let mut block_data = vec![];
                                               for _ in 0..num_blocks {
                                                   let (mut burn_ops, stacks_block, microblocks) = peers[0].make_default_tenure();

                                                   let (_, burn_header_hash, consensus_hash) = peers[0].next_burnchain_block(burn_ops.clone());
                                                   peers[0].process_stacks_epoch_at_tip(&stacks_block, &microblocks);

                                                   TestPeer::set_ops_burn_header_hash(&mut burn_ops, &burn_header_hash);

                                                   for i in 1..peers.len() {
                                                        peers[i].next_burnchain_block_raw(burn_ops.clone());
                                                   }

                                                   let sn = SortitionDB::get_canonical_burn_chain_tip(&peers[0].sortdb.as_ref().unwrap().conn()).unwrap();
                                                   block_data.push((sn.consensus_hash.clone(), Some(stacks_block), Some(microblocks)));
                                               }
                                               block_data
                                           },
                                           |_| {},
                                           |peer| {
                                               // check peer health
                                               // nothing should break 
                                               match peer.network.block_downloader {
                                                   Some(ref dl) => {
                                                       assert_eq!(dl.broken_peers.len(), 0);
                                                       assert_eq!(dl.dead_peers.len(), 0);
                                                   },
                                                   None => {}
                                               }
                                               true
                                           },
                                           |_| true);
        })
    }
}
