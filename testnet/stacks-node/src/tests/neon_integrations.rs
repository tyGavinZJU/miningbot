use super::{make_stacks_transfer_mblock_only, SK_1, ADDR_4, to_addr, make_microblock,
            make_contract_call,
            make_contract_publish, make_contract_publish_microblock_only};
use stacks::burnchains::{ Address, PublicKey };
use stacks::util::secp256k1::Secp256k1PublicKey;
use stacks::chainstate::stacks::{
    StacksTransaction, StacksPrivateKey, StacksPublicKey, StacksAddress, db::StacksChainState, StacksBlock, StacksBlockHeader };
use stacks::chainstate::burn::ConsensusHash;
use stacks::net::StacksMessageCodec;
use stacks::vm::types::{PrincipalData};
use stacks::vm::costs::ExecutionCost;
use stacks::vm::Value;
use stacks::vm::execute;

use crate::{
    neon, Config, Keychain, config::InitialBalance, BitcoinRegtestController, BurnchainController,
    config::EventObserverConfig, config::EventKeyType, node::TESTNET_CHAIN_ID
};
use stacks::net::{AccountEntryResponse, RPCPeerInfoData};
use super::bitcoin_regtest::BitcoinCoreController;
use std::{thread, env};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, Duration};
use stacks::util::hash::bytes_to_hex;
use stacks::util::hash::Hash160;

fn neon_integration_test_conf() -> (Config, StacksAddress) {
    let mut conf = super::new_test_conf();

    let keychain = Keychain::default(conf.node.seed.clone());

    conf.node.miner = true;
    conf.node.wait_time_for_microblocks = 500;

    conf.burnchain.mode = "neon".into(); 
    conf.burnchain.username = Some("neon-tester".into());
    conf.burnchain.password = Some("neon-tester-pass".into());
    conf.burnchain.peer_host = "127.0.0.1".into();
    conf.burnchain.local_mining_public_key = Some(keychain.generate_op_signer()
        .get_public_key().to_hex());
    conf.burnchain.commit_anchor_block_within = 0;

    let miner_account = keychain.origin_address().unwrap();

    (conf, miner_account)
}

mod test_observer {
    use std::convert::Infallible;
    use std::sync::Mutex;
    use warp;
    use warp::Filter;
    use std::thread;
    use tokio;

    pub const EVENT_OBSERVER_PORT: u16 = 50303;

    lazy_static! {
        pub static ref NEW_BLOCKS: Mutex<Vec<serde_json::Value>> = Mutex::new(Vec::new());
        pub static ref MEMTXS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    }

    async fn handle_block(block: serde_json::Value) -> Result<impl warp::Reply, Infallible> {
        let mut blocks = NEW_BLOCKS.lock().unwrap();
        blocks.push(block);
        Ok(warp::http::StatusCode::OK)
    }

    async fn handle_mempool_txs(txs: serde_json::Value) -> Result<impl warp::Reply, Infallible> {
        let new_rawtxs = txs.as_array().unwrap().into_iter()
            .map(|x| x.as_str().unwrap().to_string());
        let mut memtxs = MEMTXS.lock().unwrap();
        for new_tx in new_rawtxs {
            memtxs.push(new_tx);
        }
        Ok(warp::http::StatusCode::OK)
    }

    pub fn get_memtxs() -> Vec<String> {
        MEMTXS.lock().unwrap().clone()
    }

    pub fn get_blocks() -> Vec<serde_json::Value> {
        NEW_BLOCKS.lock().unwrap().clone()
    }

    async fn serve() {
        let new_blocks = warp::path!("new_block")
            .and(warp::post())
            .and(warp::body::json())
            .and_then(handle_block);
        let mempool_txs = warp::path!("new_mempool_tx")
            .and(warp::post())
            .and(warp::body::json())
            .and_then(handle_mempool_txs);
        info!("Spawning warp server");
        warp::serve(new_blocks.or(mempool_txs))
            .run(([127, 0, 0, 1], EVENT_OBSERVER_PORT)).await
    }

    pub fn spawn() {
        thread::spawn(|| {
            let mut rt = tokio::runtime::Runtime::new().expect("Failed to initialize tokio");
            rt.block_on(serve());
        });
    }
}

const PANIC_TIMEOUT_SECS: u64 = 600;
fn next_block_and_wait(btc_controller: &mut BitcoinRegtestController, blocks_processed: &Arc<AtomicU64>) {
    let current = blocks_processed.load(Ordering::SeqCst);
    eprintln!("Issuing block, waiting for bump");
    btc_controller.build_next_block(1);
    let start = Instant::now();
    while blocks_processed.load(Ordering::SeqCst) <= current {
        if start.elapsed() > Duration::from_secs(PANIC_TIMEOUT_SECS) {
            panic!("Timed out waiting for block to process");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_runloop(blocks_processed: &Arc<AtomicU64>) {
    let start = Instant::now();
    while blocks_processed.load(Ordering::SeqCst) == 0 {
        if start.elapsed() > Duration::from_secs(PANIC_TIMEOUT_SECS) {
            panic!("Timed out waiting for run loop to start");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn get_tip_anchored_block(conf: &Config) -> (ConsensusHash, StacksBlock) {
    let http_origin = format!("http://{}", &conf.node.rpc_bind);
    let client = reqwest::blocking::Client::new();

    // get the canonical chain tip
    let path = format!("{}/v2/info", &http_origin);
    let tip_info = client.get(&path).send().unwrap().json::<RPCPeerInfoData>().unwrap();
    let stacks_tip = tip_info.stacks_tip;
    let stacks_tip_consensus_hash = ConsensusHash::from_hex(&tip_info.stacks_tip_consensus_hash).unwrap();

    let stacks_id_tip = StacksBlockHeader::make_index_block_hash(&stacks_tip_consensus_hash, &stacks_tip);

    // get the associated anchored block
    let path = format!("{}/v2/blocks/{}", &http_origin, &stacks_id_tip);
    let block_bytes = client.get(&path).send().unwrap().bytes().unwrap();
    let block = StacksBlock::consensus_deserialize(&mut block_bytes.as_ref()).unwrap();

    (stacks_tip_consensus_hash, block)
}

fn find_microblock_privkey(conf: &Config, pubkey_hash: &Hash160, max_tries: u64) -> Option<StacksPrivateKey> {
    let mut keychain = Keychain::default(conf.node.seed.clone());
    for _ in 0..max_tries {
        let privk = keychain.rotate_microblock_keypair();
        let pubkh = Hash160::from_data(&StacksPublicKey::from_private(&privk).to_bytes());
        if pubkh == *pubkey_hash {
            return Some(privk);
        }
    }
    return None;
}

#[test]
#[ignore]
fn bitcoind_integration_test() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return
    }

    let (conf, miner_account) = neon_integration_test_conf();

    let mut btcd_controller = BitcoinCoreController::new(conf.clone());
    btcd_controller.start_bitcoind().map_err(|_e| ()).expect("Failed starting bitcoind");

    let mut btc_regtest_controller = BitcoinRegtestController::new(conf.clone(), None);
    let http_origin = format!("http://{}", &conf.node.rpc_bind);

    btc_regtest_controller.bootstrap_chain(201);

    eprintln!("Chain bootstrapped...");

    let mut run_loop = neon::RunLoop::new(conf);
    let blocks_processed = run_loop.get_blocks_processed_arc();
    let client = reqwest::blocking::Client::new();

    let channel = run_loop.get_coordinator_channel().unwrap();

    thread::spawn(move || {
        run_loop.start(0)
    });

    // give the run loop some time to start up!
    wait_for_runloop(&blocks_processed);

    // first block wakes up the run loop
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // first block will hold our VRF registration
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // second block will be the first mined Stacks block
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // let's query the miner's account nonce:

    eprintln!("Miner account: {}", miner_account);

    let path = format!("{}/v2/accounts/{}?proof=0",
                       &http_origin, &miner_account);
    eprintln!("Test: GET {}", path);
    let res = client.get(&path).send().unwrap().json::<AccountEntryResponse>().unwrap();
    eprintln!("Response: {:#?}", res);
    assert_eq!(u128::from_str_radix(&res.balance[2..], 16).unwrap(), 0);
    assert_eq!(res.nonce, 1);

    channel.stop_chains_coordinator();
}

#[test]
#[ignore]
fn microblock_integration_test() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return
    }

    let spender_sk = StacksPrivateKey::from_hex(SK_1).unwrap();
    let spender_addr: PrincipalData = to_addr(&spender_sk).into();

    let (mut conf, miner_account) = neon_integration_test_conf();

    conf.initial_balances.push(InitialBalance { 
        address: spender_addr.clone(),
        amount: 100300
    });

    conf.node.mine_microblocks = true;
    
    test_observer::spawn();

    conf.events_observers.push(
        EventObserverConfig {
            endpoint: format!("localhost:{}", test_observer::EVENT_OBSERVER_PORT),
            events_keys: vec![ EventKeyType::AnyEvent ],
        });

    let mut btcd_controller = BitcoinCoreController::new(conf.clone());
    btcd_controller.start_bitcoind().map_err(|_e| ()).expect("Failed starting bitcoind");

    let mut btc_regtest_controller = BitcoinRegtestController::new(conf.clone(), None);
    let http_origin = format!("http://{}", &conf.node.rpc_bind);

    btc_regtest_controller.bootstrap_chain(201);

    eprintln!("Chain bootstrapped...");

    let mut run_loop = neon::RunLoop::new(conf.clone());
    let blocks_processed = run_loop.get_blocks_processed_arc();
    let client = reqwest::blocking::Client::new();

    let channel = run_loop.get_coordinator_channel().unwrap();

    thread::spawn(move || {
        run_loop.start(0)
    });

    // give the run loop some time to start up!
    wait_for_runloop(&blocks_processed);

    // first block wakes up the run loop
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // first block will hold our VRF registration
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // second block will be the first mined Stacks block
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // let's query the miner's account nonce:

    eprintln!("Miner account: {}", miner_account);

    let path = format!("{}/v2/accounts/{}?proof=0",
                       &http_origin, &miner_account);
    eprintln!("Test: GET {}", path);
    let res = client.get(&path).send().unwrap().json::<AccountEntryResponse>().unwrap();
    assert_eq!(u128::from_str_radix(&res.balance[2..], 16).unwrap(), 0);
    assert_eq!(res.nonce, 1);

    // and our spender

    let path = format!("{}/v2/accounts/{}?proof=0",
                       &http_origin, &spender_addr);
    let res = client.get(&path).send().unwrap().json::<AccountEntryResponse>().unwrap();
    assert_eq!(u128::from_str_radix(&res.balance[2..], 16).unwrap(), 100300);
    assert_eq!(res.nonce, 0);

    // okay, let's push a transaction that is marked microblock only!
    let recipient = StacksAddress::from_string(ADDR_4).unwrap();
    let tx = make_stacks_transfer_mblock_only(&spender_sk, 0, 1000, &recipient.into(), 1000);

    let path = format!("{}/v2/transactions", &http_origin);
    let res: String = client.post(&path)
        .header("Content-Type", "application/octet-stream")
        .body(tx.clone())
        .send()
        .unwrap()
        .json()
        .unwrap();

    assert_eq!(res, StacksTransaction::consensus_deserialize(&mut &tx[..]).unwrap().txid().to_string());

    // now let's mine a couple blocks, and then check the sender's nonce.
    // this one wakes up our node, so that it'll mine a microblock _and_ an anchor block.
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    // this one will contain the sortition from above anchor block,
    //    which *should* have also confirmed the microblock.
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // I guess let's push another block for good measure?
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // microblock must have bumped our nonce
    let path = format!("{}/v2/accounts/{}?proof=0",
                       &http_origin, &spender_addr);
    let res = client.get(&path).send().unwrap().json::<AccountEntryResponse>().unwrap();
    assert_eq!(res.nonce, 1);

    // push another transaction that is marked microblock only
    let recipient = StacksAddress::from_string(ADDR_4).unwrap();
    let unconfirmed_tx_bytes = make_stacks_transfer_mblock_only(&spender_sk, 1, 1000, &recipient.into(), 1000);
    let unconfirmed_tx = StacksTransaction::consensus_deserialize(&mut &unconfirmed_tx_bytes[..]).unwrap();

    // TODO (hack) instantiate the sortdb in the burnchain
    let _ = btc_regtest_controller.sortdb_mut();

    // put it into a microblock
    let microblock = {
        let (consensus_hash, stacks_block) = get_tip_anchored_block(&conf);
        let privk = find_microblock_privkey(&conf, &stacks_block.header.microblock_pubkey_hash, 1024).unwrap();
        let (mut chainstate, _) = StacksChainState::open(false, TESTNET_CHAIN_ID, &conf.get_chainstate_path()).unwrap();

        // NOTE: it's not a zero execution cost, but there's currently not an easy way to get the
        // block's cost (and it's not like we're going to overflow the block budget in this test).
        make_microblock(&privk, &mut chainstate, &btc_regtest_controller.sortdb_ref().index_conn(), consensus_hash, stacks_block, ExecutionCost::zero(), vec![unconfirmed_tx])
    };

    let mut microblock_bytes = vec![];
    microblock.consensus_serialize(&mut microblock_bytes).unwrap();

    // post it
    let path = format!("{}/v2/microblocks",
                       &http_origin);
    let res : String = client.post(&path)
        .header("Content-Type", "application/octet-stream")
        .body(microblock_bytes.clone())
        .send()
        .unwrap()
        .json()
        .unwrap();

    assert_eq!(res, format!("{}", &microblock.block_hash()));

    eprintln!("\n\nBegin testing\nmicroblock: {:?}\n\n", &microblock);

    let path = format!("{}/v2/accounts/{}?proof=0",
                       &http_origin, &spender_addr);
    let res = client.get(&path).send().unwrap().json::<AccountEntryResponse>().unwrap();
    eprintln!("{:#?}", res);
    assert_eq!(res.nonce, 1);
    assert_eq!(u128::from_str_radix(&res.balance[2..], 16).unwrap(), 98300);

    let path = format!("{}/v2/info", &http_origin);
    let tip_info = client.get(&path).send().unwrap().json::<RPCPeerInfoData>().unwrap();
    assert!(tip_info.stacks_tip_height >= 3);

    eprintln!("{:#?}", client.get(&path).send().unwrap().json::<serde_json::Value>().unwrap());

    let memtx_events = test_observer::get_memtxs();
    assert_eq!(memtx_events.len(), 1);
    assert_eq!(&memtx_events[0], &format!("0x{}", &bytes_to_hex(&tx)));

    // let's make sure the returned blocks all point at each other.
    let blocks_observed = test_observer::get_blocks();
    // we at least mined 5 blocks
    assert!(blocks_observed.len() >= 3, "Blocks observed {} should be >= 3", blocks_observed.len());
    assert_eq!(blocks_observed.len() as u64, tip_info.stacks_tip_height);

    let mut prior = None;
    for block in blocks_observed.iter() {
        let parent_index_hash = block.get("parent_index_block_hash")
            .unwrap().as_str().unwrap().to_string();
        let my_index_hash = block.get("index_block_hash")
            .unwrap().as_str().unwrap().to_string();
        if let Some(ref previous_index_hash) = prior {
            assert_eq!(&parent_index_hash, previous_index_hash);
        }

        prior = Some(my_index_hash);
    }
    
    // we can query unconfirmed state from the microblock we announced
    let path = format!("{}/v2/accounts/{}?proof=0&tip={}",
                       &http_origin, &spender_addr, &tip_info.unanchored_tip);
    let res = client.get(&path).send().unwrap().json::<AccountEntryResponse>().unwrap();
    eprintln!("{:?}", &path);
    eprintln!("{:#?}", res);
    assert_eq!(res.nonce, 2);
    assert_eq!(u128::from_str_radix(&res.balance[2..], 16).unwrap(), 96300);

    channel.stop_chains_coordinator();
}

#[test]
#[ignore]
fn size_check_integration_test() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return
    }

    // used to specify how long to wait in between blocks.
    //   we could _probably_ add a hook to the neon node that
    //   would remove some of the need for this
    let mut giant_contract = "(define-public (f) (ok 1))".to_string();
    for _i in 0..(1024*1024 + 500) {
        giant_contract.push_str(" ");
    }

    let spender_sks: Vec<_> = (0..10).into_iter().map(|_| StacksPrivateKey::new()).collect();
    let spender_addrs: Vec<PrincipalData> =
        spender_sks.iter().map(|x| to_addr(x).into()).collect();
    // make a bunch of txs that will only fit one per block.
    let txs: Vec<_> = spender_sks.iter().enumerate().map(
        |(ix, spender_sk)| {
            if ix % 2 == 0 {
                make_contract_publish(spender_sk, 0, 1049230, "large-0",
                                      &giant_contract)
            } else {
                make_contract_publish_microblock_only(
                    spender_sk, 0, 1049230, "large-0",
                    &giant_contract)
            }
        }).collect();

    let (mut conf, miner_account) = neon_integration_test_conf();

    for spender_addr in spender_addrs.iter() {
        conf.initial_balances.push(InitialBalance { 
            address: spender_addr.clone(),
            amount: 1049230
        });
    }

    conf.node.mine_microblocks = true;

    let mut btcd_controller = BitcoinCoreController::new(conf.clone());
    btcd_controller.start_bitcoind().map_err(|_e| ()).expect("Failed starting bitcoind");

    let mut btc_regtest_controller = BitcoinRegtestController::new(conf.clone(), None);
    let http_origin = format!("http://{}", &conf.node.rpc_bind);

    btc_regtest_controller.bootstrap_chain(201);

    eprintln!("Chain bootstrapped...");

    let mut run_loop = neon::RunLoop::new(conf);
    let blocks_processed = run_loop.get_blocks_processed_arc();
    let client = reqwest::blocking::Client::new();
    let channel = run_loop.get_coordinator_channel().unwrap();

    thread::spawn(move || {
        run_loop.start(0)
    });

    // give the run loop some time to start up!
    wait_for_runloop(&blocks_processed);

    // first block wakes up the run loop
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // first block will hold our VRF registration
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // second block will be the first mined Stacks block
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // let's query the miner's account nonce:

    eprintln!("Miner account: {}", miner_account);

    let path = format!("{}/v2/accounts/{}?proof=0",
                       &http_origin, &miner_account);
    eprintln!("Test: GET {}", path);
    let res = client.get(&path).send().unwrap().json::<AccountEntryResponse>().unwrap();
    assert_eq!(u128::from_str_radix(&res.balance[2..], 16).unwrap(), 0);
    assert_eq!(res.nonce, 1);

    // and our potential spenders:

    for spender_addr in spender_addrs.iter() {
        let path = format!("{}/v2/accounts/{}?proof=0",
                           &http_origin, spender_addr);
        let res = client.get(&path).send().unwrap().json::<AccountEntryResponse>().unwrap();
        assert_eq!(u128::from_str_radix(&res.balance[2..], 16).unwrap(), 1049230);
        assert_eq!(res.nonce, 0);
    }

    for tx in txs.iter() {
        // okay, let's push a bunch of transactions that can only fit one per block!
        let path = format!("{}/v2/transactions", &http_origin);
        let res = client.post(&path)
            .header("Content-Type", "application/octet-stream")
            .body(tx.clone())
            .send()
            .unwrap();
        eprintln!("{:#?}", res);
        if res.status().is_success() {
            let res: String = res
                .json()
                .unwrap();
            assert_eq!(res, StacksTransaction::consensus_deserialize(&mut &tx[..]).unwrap().txid().to_string());
        } else {
            eprintln!("{}", res.text().unwrap());
            panic!("");
        }
    }

    // now let's mine a couple blocks, and then check the sender's nonce.
    //  at the end of mining three blocks, there should be _one_ transaction from the microblock
    //  only set that got mined (since the block before this one was empty, a microblock can
    //  be added),
    //  and _two_ transactions from the two anchor blocks that got mined (and processed)
    //
    // this one wakes up our node, so that it'll mine a microblock _and_ an anchor block.
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    // this one will contain the sortition from above anchor block,
    //    which *should* have also confirmed the microblock.
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // let's figure out how many micro-only and anchor-only txs got accepted
    //   by examining our account nonces:
    let mut micro_block_txs = 0;
    let mut anchor_block_txs = 0;
    for (ix, spender_addr) in spender_addrs.iter().enumerate() {
        let path = format!("{}/v2/accounts/{}?proof=0",
                           &http_origin, spender_addr);
        let res = client.get(&path).send().unwrap().json::<AccountEntryResponse>().unwrap();
        if res.nonce == 1 {
            if ix % 2 == 0 {
                anchor_block_txs += 1;
            } else {
                micro_block_txs += 1;
            }
        } else if res.nonce != 0 {
            panic!("Spender address nonce incremented past 1");
        }
    }

    assert_eq!(anchor_block_txs, 2);
    assert_eq!(micro_block_txs, 1);

    channel.stop_chains_coordinator();
}


#[test]
#[ignore]
fn pox_integration_test() {
    if env::var("BITCOIND_TEST") != Ok("1".into()) {
        return
    }

    let spender_sk = StacksPrivateKey::new();
    let spender_addr: PrincipalData = to_addr(&spender_sk).into();

    let pox_pubkey = Secp256k1PublicKey::from_hex("02f006a09b59979e2cb8449f58076152af6b124aa29b948a3714b8d5f15aa94ede").unwrap();
    let pox_pubkey_hash = bytes_to_hex(&Hash160::from_data(&pox_pubkey.to_bytes()).to_bytes().to_vec());

    let (mut conf, miner_account) = neon_integration_test_conf();

    let total_bal = 10_000_000_000;
    let stacked_bal = 1_000_000_000;

    conf.initial_balances.push(InitialBalance { 
        address: spender_addr.clone(),
        amount: total_bal,
    });

    let mut btcd_controller = BitcoinCoreController::new(conf.clone());
    btcd_controller.start_bitcoind().map_err(|_e| ()).expect("Failed starting bitcoind");

    let mut btc_regtest_controller = BitcoinRegtestController::new(conf.clone(), None);
    let http_origin = format!("http://{}", &conf.node.rpc_bind);

    btc_regtest_controller.bootstrap_chain(201);

    eprintln!("Chain bootstrapped...");

    let mut run_loop = neon::RunLoop::new(conf);
    let blocks_processed = run_loop.get_blocks_processed_arc();
    let client = reqwest::blocking::Client::new();
    let channel = run_loop.get_coordinator_channel().unwrap();

    thread::spawn(move || {
        run_loop.start(0)
    });

    // give the run loop some time to start up!
    wait_for_runloop(&blocks_processed);

    // first block wakes up the run loop
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // first block will hold our VRF registration
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // second block will be the first mined Stacks block
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // let's query the miner's account nonce:

    eprintln!("Miner account: {}", miner_account);

    let path = format!("{}/v2/accounts/{}?proof=0",
                       &http_origin, &miner_account);
    eprintln!("Test: GET {}", path);
    let res = client.get(&path).send().unwrap().json::<AccountEntryResponse>().unwrap();
    assert_eq!(u128::from_str_radix(&res.balance[2..], 16).unwrap(), 0);
    assert_eq!(res.nonce, 1);

    // and our potential spenders:

    let path = format!("{}/v2/accounts/{}?proof=0",
                       &http_origin, spender_addr);
    let res = client.get(&path).send().unwrap().json::<AccountEntryResponse>().unwrap();
    assert_eq!(u128::from_str_radix(&res.balance[2..], 16).unwrap(), total_bal as u128);
    assert_eq!(res.nonce, 0);

    let tx = make_contract_call(&spender_sk, 0, 243, &StacksAddress::from_string("ST000000000000000000002AMW42H").unwrap(),
                                "pox", "stack-stx", &[Value::UInt(stacked_bal),
                                                      execute(&format!("{{ hashbytes: 0x{}, version: 0x00 }}", pox_pubkey_hash)).unwrap().unwrap(),
                                                      Value::UInt(3)]);

    // okay, let's push that stacking transaction!
    let path = format!("{}/v2/transactions", &http_origin);
    let res = client.post(&path)
        .header("Content-Type", "application/octet-stream")
        .body(tx.clone())
        .send()
        .unwrap();
    eprintln!("{:#?}", res);
    if res.status().is_success() {
        let res: String = res
            .json()
            .unwrap();
        assert_eq!(res, StacksTransaction::consensus_deserialize(&mut &tx[..]).unwrap().txid().to_string());
    } else {
        eprintln!("{}", res.text().unwrap());
        panic!("");
    }

    // now let's mine a couple blocks, and then check the sender's nonce.
    //  at the end of mining three blocks, there should be _one_ transaction from the microblock
    //  only set that got mined (since the block before this one was empty, a microblock can
    //  be added),
    //  and _two_ transactions from the two anchor blocks that got mined (and processed)
    //
    // this one wakes up our node, so that it'll mine a microblock _and_ an anchor block.
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    // this one will contain the sortition from above anchor block,
    //    which *should* have also confirmed the microblock.
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // let's figure out how many micro-only and anchor-only txs got accepted
    //   by examining our account nonces:
    let path = format!("{}/v2/accounts/{}?proof=0",
                       &http_origin, spender_addr);
    let res = client.get(&path).send().unwrap().json::<AccountEntryResponse>().unwrap();
    if res.nonce != 1 {        
        assert_eq!(res.nonce, 1, "Spender address nonce should be 1");
    }

    // now let's mine until the next reward cycle starts ...
    for _i in 0..35 {
        next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);
    }

    // we should have received a Bitcoin commitment
    let utxos = btc_regtest_controller.get_utxos(
        &pox_pubkey, 1).expect("Should have been able to retrieve UTXOs for PoX recipient");

    eprintln!("Got UTXOs: {}", utxos.len());
    assert!(utxos.len() > 0, "Should have received an output during PoX");

    channel.stop_chains_coordinator();
}
