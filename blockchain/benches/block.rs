#![feature(test)]
use stegos_blockchain::{Blockchain, BlockchainConfig, ListDb,
                        genesis, MonetaryBlock, Output, PaymentOutput, StakeOutput,
VERSION, BaseBlockHeader};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use simple_logger;
use std::collections::BTreeMap;
use std::time::Duration;
use std::time::SystemTime;
use stegos_keychain::KeyChain;
use stegos_crypto::pbc::secure;
use stegos_crypto::hash::Hash;
use stegos_crypto::curve1174::fields::Fr;
use tempdir::TempDir;

#[macro_use]
extern crate criterion;

use criterion::{Bencher, Criterion};
use criterion::black_box;


fn create_monetary_block(
    chain: &mut Blockchain,
    keys: &KeyChain,
    timestamp: SystemTime,
    view_change: u32,
    stake: i64,
) -> (MonetaryBlock, Vec<Hash>, Vec<Hash>) {
    let mut input_hashes: Vec<Hash> = Vec::new();
    let mut gamma: Fr = Fr::zero();
    let mut amount: i64 = 0;
    for input_hash in chain.unspent() {
        let input = chain
            .output_by_hash(&input_hash)
            .expect("no disk errors")
            .expect("exists");
        match input {
            Output::PaymentOutput(ref o) => {
                let payload = o.decrypt_payload(&keys.wallet_skey).unwrap();
                gamma += payload.gamma;
                amount += payload.amount;
                input_hashes.push(input_hash.clone());
            }
            Output::StakeOutput(ref o) => {
                o.validate_pkey().expect("valid network pkey signature");
                o.decrypt_payload(&keys.wallet_skey).unwrap();
                amount += o.amount;
                input_hashes.push(input_hash.clone());
            }
        }
    }

    let mut outputs: Vec<Output> = Vec::new();
    let (output, output_gamma) = PaymentOutput::new(
        timestamp,
        &keys.wallet_skey,
        &keys.wallet_pkey,
        amount - stake,
    )
        .expect("keys are valid");
    outputs.push(Output::PaymentOutput(output));
    gamma -= output_gamma;
    let output = StakeOutput::new(
        timestamp,
        &keys.wallet_skey,
        &keys.wallet_pkey,
        &keys.network_pkey,
        &keys.network_skey,
        stake,
    )
        .expect("keys are valid");
    outputs.push(Output::StakeOutput(output));

    let output_hashes: Vec<Hash> = outputs.iter().map(Hash::digest).collect();
    let version = VERSION;
    let previous = chain.last_block_hash().clone();
    let height = chain.height();
    let base = BaseBlockHeader::new(version, previous, height, view_change, timestamp);
    let mut block = MonetaryBlock::new(base, gamma, 0, &input_hashes, &outputs, None);
    let block_hash = Hash::digest(&block);
    block.body.sig = secure::sign_hash(&block_hash, &keys.network_skey);
    (block, input_hashes, output_hashes)
}

fn create_blocks(b: &mut Bencher) {

    simple_logger::init_with_level(log::Level::Debug).unwrap_or_default();

    let keychains = [KeyChain::new_mem()];

    let timestamp_at_start = SystemTime::now();
    let mut timestamp = timestamp_at_start;
    let mut cfg: BlockchainConfig = Default::default();
    let genesis = genesis(&keychains, cfg.min_stake_amount, 1_000_000, timestamp);
    let temp_prefix: String = thread_rng().sample_iter(&Alphanumeric).take(30).collect();
    let temp_dir = TempDir::new(&temp_prefix).expect("couldn't create temp dir");
    let database = ListDb::new(&temp_dir.path());
    let mut chain = Blockchain::with_db(cfg.clone(), database, genesis.clone(), timestamp);

    let mut blocks = Vec::new();
    let height = chain.height();
    // create valid blocks.
    for i in 0..10 {
        timestamp += cfg.bonding_time + Duration::from_millis(1);
        // Non-empty block.
        let (block, input_hashes, output_hashes) =
            create_monetary_block(&mut chain, &keychains[0], timestamp, i, cfg.min_stake_amount);
        let block_hash = Hash::digest(&block);

        chain.push_monetary_block(block.clone(), timestamp.clone()).unwrap();

        blocks.push((block, timestamp));
    }


    println!("start tracking");
    b.iter_with_setup(|| {

        // start bench to other blockchain
        let temp_prefix: String = thread_rng().sample_iter(&Alphanumeric).take(30).collect();
        let temp_dir = TempDir::new(&temp_prefix).expect("couldn't create temp dir");
        let database = ListDb::new(&temp_dir.path());

        Blockchain::with_db(cfg.clone(), database, genesis.clone(), timestamp_at_start)
    },|mut chain| {


            for (b, t) in & blocks {
                chain.push_monetary_block(b.clone(), t.clone()).unwrap();
            }

    });

}

fn block_benchmark(c: &mut Criterion) {
    c.bench_function("block 10", create_blocks);
}

criterion_group!{
     name = benches;
     config = Criterion::default().measurement_time(Duration::from_secs(10)).warm_up_time(Duration::from_secs(1)).sample_size(2);
     targets = block_benchmark
}

criterion_main!(benches);