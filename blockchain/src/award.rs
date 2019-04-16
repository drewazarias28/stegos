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

use stegos_crypto::pbc::secure;
use crate::blockchain::Blockchain;
use crate::StakersGroup;

use std::collections::HashMap;



pub struct ServiceAwards {
    /// Total amount of accumulated budget.
    budget: i64,
    /// Epoch counter, since last service awarding.
    num_epochs: u64,
    /// Active epoch counter for each validators.
    validators_activity_epochs: HashMap<secure::PublicKey, u64>,
}

impl ServiceAwards {
    /// Add block award to the service awards budget.
    pub fn add_reward(&mut self, amount: i64) {
        assert!(amount > 0);
        self.budget += amount
    }
    /// Try to produce service awards.
    /// Returns None, if blockchain is not ready for awards.
    /// Returns list of validators with amount of winning pot.
    pub fn execute_awards(&mut self, chain: &Blockchain) -> Option<StakersGroup> {
        unimplemented!()
    }

    /// Check if block awarded validators according to our blockchain view.
    pub fn check_awards(&self, chain: &Blockchain, awarded: &StakersGroup) {
        unimplemented!()
    }


//    #[inline]
//    pub fn current_version(&self) -> u64 {
//        self.escrow.current_version()
//    }
//
//    #[inline]
//    pub fn checkpoint(&mut self) {
//        self.escrow.checkpoint();
//    }
//
//    #[inline]
//    pub fn rollback_to_version(&mut self, to_version: u64) {
//        self.escrow.rollback_to_version(to_version);
//    }
}

struct AwardsConfiguration {
    /// Maximum count of winners.
    count: usize,
    
}



