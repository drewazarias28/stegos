//! Tests.

//
// Copyright (c) 2019 Stegos AG
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use crate::block::{BaseBlockHeader, Block, MacroBlock, MicroBlock, VERSION};
use crate::blockchain::Blockchain;
use crate::election::mix;
use crate::genesis::genesis;
use crate::multisignature::create_multi_signature;
use crate::output::{Output, PaymentOutput, PaymentPayloadData, StakeDef, StakeOutput};
use crate::transaction::{CoinbaseTransaction, PaymentTransaction, Transaction};
use std::collections::btree_map::BTreeMap;
use std::time::SystemTime;
use stegos_crypto::curve1174::{self, Fr};
use stegos_crypto::hash::Hash;
use stegos_crypto::pbc;

#[derive(Clone, Debug)]
pub struct KeyChain {
    /// Wallet Secret Key.
    pub wallet_skey: curve1174::SecretKey,
    /// Wallet Public Key.
    pub wallet_pkey: curve1174::PublicKey,
    /// Network Secret Key.
    pub network_skey: pbc::SecretKey,
    /// Network Public Key.
    pub network_pkey: pbc::PublicKey,
}

impl KeyChain {
    pub fn new() -> Self {
        let (wallet_skey, wallet_pkey) = curve1174::make_random_keys();
        let (network_skey, network_pkey) = pbc::make_random_keys();
        Self {
            wallet_skey,
            wallet_pkey,
            network_skey,
            network_pkey,
        }
    }
}

pub fn fake_genesis(
    stake: i64,
    coins: i64,
    num_nodes: usize,
    timestamp: SystemTime,
) -> (Vec<KeyChain>, Vec<Block>) {
    let mut keychains = Vec::with_capacity(num_nodes);
    for _i in 0..num_nodes {
        let keychain = KeyChain::new();
        keychains.push(keychain);
    }
    let mut stakes = Vec::with_capacity(num_nodes);
    for i in 0..num_nodes {
        let stake_def = StakeDef {
            recipient_pkey: &keychains[i].wallet_pkey,
            network_skey: &keychains[i].network_skey,
            network_pkey: &keychains[i].network_pkey,
            amount: stake,
        };
        stakes.push(stake_def);
    }
    let genesis = genesis(&stakes, coins, timestamp);
    (keychains, genesis)
}

pub fn create_fake_macro_block(
    chain: &Blockchain,
    keychains: &[KeyChain],
    timestamp: SystemTime,
) -> MacroBlock {
    let version = VERSION;
    let previous = chain.last_block_hash().clone();
    let height = chain.height();
    let view_change = chain.view_change();
    let key = chain.select_leader(view_change);
    let keys = keychains.iter().find(|p| p.network_pkey == key).unwrap();
    let seed = mix(chain.last_random(), view_change);
    let random = pbc::make_VRF(&keys.network_skey, &seed);
    let base = BaseBlockHeader::new(version, previous, height, view_change, timestamp, random);
    let mut block = MacroBlock::empty(base, keys.network_pkey);
    let block_hash = Hash::digest(&block);
    let validators = chain.validators();
    let mut signatures: BTreeMap<pbc::PublicKey, pbc::Signature> = BTreeMap::new();
    for keychain in keychains {
        let sig = pbc::sign_hash(&block_hash, &keychain.network_skey);
        signatures.insert(keychain.network_pkey.clone(), sig);
    }
    let (multisig, multisigmap) = create_multi_signature(&validators, &signatures);
    block.body.multisig = multisig;
    block.body.multisigmap = multisigmap;
    block
}

pub fn create_fake_micro_block(
    chain: &Blockchain,
    keychains: &[KeyChain],
    timestamp: SystemTime,
    block_reward: i64,
) -> (MicroBlock, Vec<Hash>, Vec<Hash>) {
    let version: u64 = VERSION;
    let height = chain.height();
    let view_change = chain.view_change();
    let key = chain.select_leader(view_change);
    let keys = keychains.iter().find(|p| p.network_pkey == key).unwrap();
    let previous = chain.last_block_hash().clone();
    let seed = mix(chain.last_random(), view_change);
    let random = pbc::make_VRF(&keys.network_skey, &seed);

    let mut input_hashes: Vec<Hash> = Vec::new();
    let mut inputs: Vec<Output> = Vec::new();
    let mut monetary_balance: i64 = 0;
    let mut staking_balance: i64 = 0;
    for input_hash in chain.unspent() {
        let input = chain
            .output_by_hash(&input_hash)
            .expect("no disk errors")
            .expect("exists");
        input.validate().expect("Valid input");
        match input {
            Output::PaymentOutput(ref o) => {
                let payload = o.decrypt_payload(&keys.wallet_skey).unwrap();
                monetary_balance += payload.amount;
            }
            Output::PublicPaymentOutput(ref o) => {
                monetary_balance += o.amount;
            }
            Output::StakeOutput(ref o) => {
                staking_balance += o.amount;
            }
        }
        input_hashes.push(input_hash.clone());
        inputs.push(input);
    }

    let mut outputs: Vec<Output> = Vec::new();
    let mut outputs_gamma = Fr::zero();
    // Payments.
    if monetary_balance > 0 {
        let (output, output_gamma) =
            PaymentOutput::new(&keys.wallet_pkey, monetary_balance).expect("keys are valid");
        outputs.push(Output::PaymentOutput(output));
        outputs_gamma += output_gamma;
    }

    // Stakes.
    if staking_balance > 0 {
        let output = StakeOutput::new(
            &keys.wallet_pkey,
            &keys.network_skey,
            &keys.network_pkey,
            staking_balance,
        )
        .expect("keys are valid");
        outputs.push(Output::StakeOutput(output));
    }

    let output_hashes: Vec<Hash> = outputs.iter().map(Hash::digest).collect();
    let block_fee: i64 = 0;
    let tx = PaymentTransaction::new(
        &keys.wallet_skey,
        &inputs,
        &outputs,
        &outputs_gamma,
        block_fee,
    )
    .expect("Invalid keys");
    tx.validate(&inputs).expect("Invalid transaction");

    let coinbase_tx = {
        let data = PaymentPayloadData::Comment(format!("Block reward"));
        let (output, gamma) = PaymentOutput::with_payload(&keys.wallet_pkey, block_reward, data)
            .expect("invalid keys");
        CoinbaseTransaction {
            block_reward,
            block_fee,
            gamma: -gamma,
            txouts: vec![Output::PaymentOutput(output)],
        }
    };
    coinbase_tx.validate().expect("Invalid transaction");

    let transactions: Vec<Transaction> = vec![coinbase_tx.into(), tx.into()];

    let base = BaseBlockHeader::new(version, previous, height, view_change, timestamp, random);
    let mut block = MicroBlock::new(base, None, transactions, keys.network_pkey);
    block.sign(&keys.network_skey, &keys.network_pkey);
    (block, input_hashes, output_hashes)
}

pub fn create_empty_micro_block(
    chain: &Blockchain,
    keychains: &[KeyChain],
    timestamp: SystemTime,
) -> MicroBlock {
    let version = VERSION;
    let previous = chain.last_block_hash().clone();
    let height = chain.height();
    let view_change = chain.view_change();
    let key = chain.select_leader(view_change);
    let keys = keychains.iter().find(|p| p.network_pkey == key).unwrap();
    let seed = mix(chain.last_random(), view_change);
    let random = pbc::make_VRF(&keys.network_skey, &seed);
    let base = BaseBlockHeader::new(version, previous, height, view_change, timestamp, random);
    let mut block = MicroBlock::empty(base, None, keys.network_pkey);
    block.sign(&keys.network_skey, &keys.network_pkey);
    block
}
