use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use std::thread::sleep;

use async_h1::{client};
use async_std::net::{TcpStream};
use http_types::{Method, Request, Url};

use serde_json::json;

use stacks::burnchains::Txid;
use stacks::chainstate::stacks::events::{StacksTransactionEvent, STXEventType, FTEventType, NFTEventType};
use stacks::chainstate::stacks::StacksTransaction;
use stacks::net::StacksMessageCodec;
use stacks::vm::types::{Value, QualifiedContractIdentifier, AssetIdentifier};
use stacks::vm::analysis::{contract_interface_builder::build_contract_interface};
use stacks::util::hash::{bytes_to_hex};
use stacks::chainstate::stacks::StacksBlockId;

use super::config::{EventObserverConfig, EventKeyType};
use super::node::{ChainTip};

#[derive(Debug, Clone)]
struct EventObserver {
    endpoint: String,
}

const STATUS_RESP_TRUE: &str = "success";
const STATUS_RESP_NOT_COMMITTED: &str = "abort_by_response";
const STATUS_RESP_POST_CONDITION: &str  = "abort_by_post_condition";

pub const PATH_MEMPOOL_TX_SUBMIT: &str = "new_mempool_tx";
pub const PATH_BLOCK_PROCESSED: &str = "new_block";

impl EventObserver {

    fn send_payload(&self, payload: &serde_json::Value, path: &str) {

        let body = match serde_json::to_vec(&payload) {
            Ok(body) => body,
            Err(err) => {
                error!("Event dispatcher: serialization failed  - {:?}", err);
                return
            }
        };

        let url = {
            let joined_components = match path.starts_with("/") {
                true => format!("{}{}", &self.endpoint, path),
                false => format!("{}/{}", &self.endpoint, path)
            };
            let url = format!("http://{}", joined_components);
            Url::parse(&url).expect(&format!("Event dispatcher: unable to parse {} as a URL", url))
        };

        let backoff = Duration::from_millis((1.0 * 1_000.0) as u64);

        loop {
            let body = body.clone();
            let mut req = Request::new(Method::Post, url.clone());
            req.append_header("Content-Type", "application/json").expect("Unable to set header");
            req.set_body(body);

            let response = async_std::task::block_on(async {
                let stream = match TcpStream::connect(self.endpoint.clone()).await {
                    Ok(stream) => stream,
                    Err(err) => {
                        println!("Event dispatcher: connection failed  - {:?}", err);
                        return None;
                    }
                };    
    
                match client::connect(stream, req).await {
                    Ok(response) => Some(response),
                    Err(err) => {
                        println!("Event dispatcher: rpc invokation failed  - {:?}", err);
                        return None;
                    }
                }
            });

            if let Some(response) = response {
                if response.status().is_success() {
                    break;
                } else {
                    error!("Event dispatcher: POST {} failed with error {:?}", self.endpoint, response);
                }
            }
            sleep(backoff);
        };
    }

    fn make_new_mempool_txs_payload(transactions: Vec<StacksTransaction>) -> serde_json::Value {
        let raw_txs = transactions.into_iter().map(|tx| {
            serde_json::Value::String(
                format!("0x{}", &bytes_to_hex(&tx.serialize_to_vec())))
        }).collect();

        serde_json::Value::Array(raw_txs)
    }

    fn send_new_mempool_txs(&self, payload: &serde_json::Value) {
        self.send_payload(payload, PATH_MEMPOOL_TX_SUBMIT);
    }

    fn send(&mut self, filtered_events: Vec<&(bool, Txid, &StacksTransactionEvent)>, chain_tip: &ChainTip,
            parent_index_hash: &StacksBlockId) {
        // Serialize events to JSON
        let serialized_events: Vec<serde_json::Value> = filtered_events.iter().map(|(committed, txid, event)|
            event.json_serialize(txid, *committed)
        ).collect();

        let mut tx_index: u32 = 0;
        let serialized_txs: Vec<serde_json::Value> = chain_tip.receipts.iter().map(|receipt| {
            let tx = &receipt.transaction;

            let (success, result) = match (receipt.post_condition_aborted, &receipt.result) {
                (false, Value::Response(response_data)) => {
                    let status = if response_data.committed {
                        STATUS_RESP_TRUE
                    } else {
                        STATUS_RESP_NOT_COMMITTED
                    };
                    (status, response_data.data.clone())
                },
                (true, Value::Response(response_data)) => {
                    (STATUS_RESP_POST_CONDITION, response_data.data.clone())
                },
                _ => unreachable!(), // Transaction results should always be a Value::Response type
            };

            let raw_tx = {
                let mut bytes = vec![];
                tx.consensus_serialize(&mut bytes).unwrap();
                let formatted_bytes: Vec<String> = bytes.iter().map(|b| format!("{:02x}", b)).collect();
                formatted_bytes
            };
            
            let raw_result = {
                let mut bytes = vec![];
                result.consensus_serialize(&mut bytes).unwrap();
                let formatted_bytes: Vec<String> = bytes.iter().map(|b| format!("{:02x}", b)).collect();
                formatted_bytes
            };
            let contract_interface_json = {
                match &receipt.contract_analysis {
                    Some(analysis) => json!(build_contract_interface(analysis)),
                    None => json!(null)
                }
            };
            let val = json!({
                "txid": format!("0x{}", tx.txid()),
                "tx_index": tx_index,
                "status": success,
                "raw_result": format!("0x{}", raw_result.join("")),
                "raw_tx": format!("0x{}", raw_tx.join("")),
                "contract_abi": contract_interface_json,
            });
            tx_index += 1;
            val
        }).collect();
        
        // Wrap events
        let payload = json!({
            "block_hash": format!("0x{}", chain_tip.block.block_hash()),
            "block_height": chain_tip.metadata.block_height,
            "burn_block_time": chain_tip.metadata.burn_header_timestamp,
            "index_block_hash": format!("0x{}", chain_tip.metadata.index_block_hash()),
            "parent_block_hash": format!("0x{}", chain_tip.block.header.parent_block),
            "parent_index_block_hash": format!("0x{}", parent_index_hash),
            "parent_microblock": format!("0x{}", chain_tip.block.header.parent_microblock),
            "events": serialized_events,
            "transactions": serialized_txs,
        });

        // Send payload
        self.send_payload(&payload, PATH_BLOCK_PROCESSED);
    }
}

#[derive(Clone)]
pub struct EventDispatcher {
    registered_observers: Vec<EventObserver>,
    contract_events_observers_lookup: HashMap<(QualifiedContractIdentifier, String), HashSet<u16>>,
    assets_observers_lookup: HashMap<AssetIdentifier, HashSet<u16>>,
    mempool_observers_lookup: HashSet<u16>,
    stx_observers_lookup: HashSet<u16>,
    any_event_observers_lookup: HashSet<u16>,
}

impl EventDispatcher {

    pub fn new() -> EventDispatcher {
        EventDispatcher {
            registered_observers: vec![],
            contract_events_observers_lookup: HashMap::new(),
            assets_observers_lookup: HashMap::new(),
            stx_observers_lookup: HashSet::new(),
            any_event_observers_lookup: HashSet::new(),
            mempool_observers_lookup: HashSet::new(),
        }
    }

    pub fn process_chain_tip(&mut self, chain_tip: &ChainTip, parent_index_hash: &StacksBlockId) {

        let mut dispatch_matrix: Vec<HashSet<usize>> = self.registered_observers.iter().map(|_| HashSet::new()).collect();
        let mut events: Vec<(bool, Txid, &StacksTransactionEvent)> = vec![];
        let mut i: usize = 0;
        for receipt in chain_tip.receipts.iter() {
            let tx_hash = receipt.transaction.txid();
            for event in receipt.events.iter() {
                match event {
                    StacksTransactionEvent::SmartContractEvent(event_data) => {
                        if let Some(observer_indexes) = self.contract_events_observers_lookup.get(&event_data.key) {
                            for o_i in observer_indexes {
                                dispatch_matrix[*o_i as usize].insert(i);
                            }
                        }
                    },
                    StacksTransactionEvent::STXEvent(STXEventType::STXTransferEvent(_)) |
                    StacksTransactionEvent::STXEvent(STXEventType::STXMintEvent(_)) |
                    StacksTransactionEvent::STXEvent(STXEventType::STXBurnEvent(_)) => {
                        for o_i in &self.stx_observers_lookup {
                            dispatch_matrix[*o_i as usize].insert(i);
                        }
                    },
                    StacksTransactionEvent::NFTEvent(NFTEventType::NFTTransferEvent(event_data)) => {
                        self.update_dispatch_matrix_if_observer_subscribed(&event_data.asset_identifier, i, &mut dispatch_matrix);
                    },
                    StacksTransactionEvent::NFTEvent(NFTEventType::NFTMintEvent(event_data)) => {
                        self.update_dispatch_matrix_if_observer_subscribed(&event_data.asset_identifier, i, &mut dispatch_matrix);
                    },
                    StacksTransactionEvent::FTEvent(FTEventType::FTTransferEvent(event_data)) => {
                        self.update_dispatch_matrix_if_observer_subscribed(&event_data.asset_identifier, i, &mut dispatch_matrix);
                    },
                    StacksTransactionEvent::FTEvent(FTEventType::FTMintEvent(event_data)) => {
                        self.update_dispatch_matrix_if_observer_subscribed(&event_data.asset_identifier, i, &mut dispatch_matrix);
                    },
                }
                events.push((!receipt.post_condition_aborted, tx_hash, event));
                for o_i in &self.any_event_observers_lookup {
                    dispatch_matrix[*o_i as usize].insert(i);
                }
                i += 1;
            }
        }


        for (observer_id, filtered_events_ids) in dispatch_matrix.iter().enumerate() {
            let filtered_events: Vec<_> = filtered_events_ids.iter()
                .map(|event_id| &events[*event_id]).collect();

            self.registered_observers[observer_id].send(filtered_events, chain_tip, parent_index_hash);
        }
    }

    pub fn process_new_mempool_txs(&self, txs: Vec<StacksTransaction>) {
        // lazily assemble payload only if we have observers
        let interested_observers: Vec<_> = self.registered_observers.iter().enumerate().filter(
            |(obs_id, _observer)| {
                self.mempool_observers_lookup.contains(&(*obs_id as u16)) ||
                    self.any_event_observers_lookup.contains(&(*obs_id as u16))
            }).collect();
        if interested_observers.len() < 1 {
            return;
        }

        let payload = EventObserver::make_new_mempool_txs_payload(txs);

        for (_, observer) in interested_observers.iter() {
            observer.send_new_mempool_txs(&payload);
        }
    }

    fn update_dispatch_matrix_if_observer_subscribed(&self, asset_identifier: &AssetIdentifier, event_index: usize, dispatch_matrix: &mut Vec<HashSet<usize>>) {
        if let Some(observer_indexes) = self.assets_observers_lookup.get(asset_identifier) {
            for o_i in observer_indexes {
                dispatch_matrix[*o_i as usize].insert(event_index);
            }
        }
    }

    pub fn register_observer(&mut self, conf: &EventObserverConfig) {
        // let event_observer = EventObserver::new(&conf.address, conf.port);
        info!("Registering event observer at: {}", conf.endpoint);
        let event_observer = EventObserver { 
            endpoint: conf.endpoint.clone(),
        };

        let observer_index = self.registered_observers.len() as u16;

        for event_key_type in conf.events_keys.iter() {
            match event_key_type {
                EventKeyType::SmartContractEvent(event_key) => {
                    match self.contract_events_observers_lookup.entry(event_key.clone()) {
                        Entry::Occupied(observer_indexes) => {
                            observer_indexes.into_mut().insert(observer_index);
                        },
                        Entry::Vacant(v) => {
                            let mut observer_indexes = HashSet::new();
                            observer_indexes.insert(observer_index);
                            v.insert(observer_indexes);
                        }
                    };
                },
                EventKeyType::MemPoolTransactions => {
                    self.mempool_observers_lookup.insert(observer_index);
                },
                EventKeyType::STXEvent => {
                    self.stx_observers_lookup.insert(observer_index);
                },
                EventKeyType::AssetEvent(event_key) => {
                    match self.assets_observers_lookup.entry(event_key.clone()) {
                        Entry::Occupied(observer_indexes) => {
                            observer_indexes.into_mut().insert(observer_index);
                        },
                        Entry::Vacant(v) => {
                            let mut observer_indexes = HashSet::new();
                            observer_indexes.insert(observer_index);
                            v.insert(observer_indexes);
                        }
                    };
                },
                EventKeyType::AnyEvent => {
                    self.any_event_observers_lookup.insert(observer_index);
                },
            }

        }

        self.registered_observers.push(event_observer);
    }
}
