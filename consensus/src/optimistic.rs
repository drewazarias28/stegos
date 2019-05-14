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

//!
//! View Changes implementation.
//!

use crate::error::ConsensusError;
use failure::Error;
use log::{debug, info};
use std::collections::HashMap;
use stegos_blockchain::view_changes::*;
use stegos_blockchain::{check_supermajority, Blockchain, ChainInfo, ValidatorId};
use stegos_crypto::hash::{Hash, Hashable, Hasher};
use stegos_crypto::pbc::secure;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewChangeMessage {
    pub chain: ChainInfo,
    pub validator_id: ValidatorId,
    pub signature: secure::Signature,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SealedViewChangeProof {
    pub chain: ChainInfo,
    pub proof: ViewChangeProof,
}

impl Hashable for ViewChangeMessage {
    fn hash(&self, state: &mut Hasher) {
        self.chain.hash(state);
        self.validator_id.hash(state);
        self.signature.hash(state);
    }
}

impl ViewChangeMessage {
    pub fn new(chain: ChainInfo, validator_id: ValidatorId, skey: &secure::SecretKey) -> Self {
        let hash = Hash::digest(&chain);
        let signature = secure::sign_hash(&hash, skey);
        ViewChangeMessage {
            chain,
            validator_id,
            signature,
        }
    }

    #[must_use]
    pub fn validate(&self, blockchain: &Blockchain) -> Result<(), ConsensusError> {
        let validator_id = self.validator_id;
        if (validator_id as usize) >= blockchain.validators().len() {
            return Err(ConsensusError::InvalidValidatorId(validator_id));
        }
        let hash = Hash::digest(&self.chain);
        let author = blockchain.validators()[validator_id as usize].0;
        if let Err(_e) = secure::check_hash(&hash, &self.signature, &author) {
            return Err(ConsensusError::InvalidViewChangeSignature);
        }
        Ok(())
    }
}

//Collect ViewChange for current height only.
#[derive(Debug)]
pub struct ViewChangeCollector {
    /// Keeps `ViewChangeMessage` for each validator,
    /// when message.view_change strict equal to our view_change.
    actual_view_changes: HashMap<ValidatorId, ViewChangeMessage>,
    collected_slots: i64,
    /// validator_id of current node.
    /// If None, ignore events for current epoch.
    validator_id: Option<ValidatorId>,
    pkey: secure::PublicKey,
    skey: secure::SecretKey,
}

impl ViewChangeCollector {
    pub fn new(
        blockchain: &Blockchain,
        pkey: secure::PublicKey,
        skey: secure::SecretKey,
    ) -> ViewChangeCollector {
        let mut collector = ViewChangeCollector {
            pkey,
            skey,
            collected_slots: 0,
            validator_id: None,
            actual_view_changes: Default::default(),
        };
        collector.on_new_consensus(blockchain);
        collector
    }
    //
    // External events
    //
    pub fn handle_message(
        &mut self,
        blockchain: &Blockchain,
        message: ViewChangeMessage,
    ) -> Result<Option<ViewChangeProof>, ConsensusError> {
        if !self.is_validator() {
            return Ok(None);
        }

        if message.chain.height != blockchain.height() {
            return Err(ConsensusError::InvalidViewChangeHeight(
                message.chain.height,
                blockchain.height(),
            ));
        }

        if message.chain.last_block != blockchain.last_block_hash() {
            return Err(ConsensusError::InvalidLastBlockHash(
                message.chain.last_block,
                blockchain.last_block_hash(),
            ));
        }
        //TODO: Implement catch-up
        if message.chain.view_change != blockchain.view_change() {
            return Err(ConsensusError::InvalidViewChangeCounter(
                message.chain.view_change,
                blockchain.view_change(),
            ));
        }

        // checks if id exist, and signature.
        message.validate(&blockchain)?;

        info!(
            "Received valid view_change message: view_change={}, validator_id={},",
            message.chain.view_change, message.validator_id
        );
        let id = message.validator_id;
        if self.actual_view_changes.get(&id).is_none() {
            self.actual_view_changes.insert(id, message.clone());
            self.collected_slots += blockchain.validators()[id as usize].1;
        }
        info!(
            "Collected view_changes: collected={}, total={},",
            self.collected_slots,
            blockchain.total_slots()
        );
        // return proof only about first 2/3rd of validators
        if check_supermajority(self.collected_slots, blockchain.total_slots()) {
            let signatures = self
                .actual_view_changes
                .iter()
                .map(|(k, v)| (*k, &v.signature));
            let proof = ViewChangeProof::new(signatures);
            self.reset();
            return Ok(Some(proof));
        }
        Ok(None)
    }

    /// Handle block timeout, starting mooving to the next view change.
    pub fn handle_timeout(
        &mut self,
        blockchain: &Blockchain,
    ) -> Result<Option<ViewChangeMessage>, Error> {
        if !self.is_validator() {
            return Ok(None);
        }
        let id = self.validator_id.unwrap();

        debug!(
            "Timeout at block receiving, trying to collect view changes: validator_id = {}",
            id
        );
        // on timeout, create view change message.
        let chain = ChainInfo::from_blockchain(blockchain);
        let msg = ViewChangeMessage::new(chain, id, &self.skey);
        Ok(Some(msg))
    }

    //
    // Internal events
    //
    /// Process new payment block.
    pub fn on_new_payment_block(&mut self, _blockchain: &Blockchain) {
        if !self.is_validator() {
            return ();
        }

        self.reset()
    }

    /// Handle new group creation.
    pub fn on_new_consensus(&mut self, blockchain: &Blockchain) {
        // get validator id, by public_key
        let validator_id = blockchain
            .validators()
            .iter()
            .enumerate()
            .find(|(_id, validator)| validator.0 == self.pkey)
            .map(|(id, _)| id as ValidatorId);

        self.reset();
        self.validator_id = validator_id;
    }

    //
    // Other methods
    //
    /// Is current node active validator.
    pub fn is_validator(&self) -> bool {
        self.validator_id.is_some()
    }

    /// Reset collector to initial state.
    fn reset(&mut self) {
        self.actual_view_changes.clear();
        self.collected_slots = 0;
    }
}
