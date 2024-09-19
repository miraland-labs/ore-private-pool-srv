use {
    chrono::NaiveDateTime,
    serde::{Deserialize, Serialize},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pool {
    pub id: i32,
    pub proof_pubkey: String,
    pub authority_pubkey: String,
    pub total_rewards: i64,
    pub claimed_rewards: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Miner {
    pub id: i64,
    pub pubkey: String,
    pub enabled: bool,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Contribution {
    pub id: i64,
    pub miner_id: i64,
    pub challenge_id: i64,
    pub nonce: u64,
    pub difficulty: i16,
    pub created: NaiveDateTime,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ContributionWithPubkey {
    pub id: i64,
    pub miner_id: i64,
    pub challenge_id: i64,
    pub nonce: u64,
    pub difficulty: i16,
    pub created: NaiveDateTime,
    pub pubkey: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertContribution {
    pub miner_id: i64,
    pub challenge_id: i64,
    pub nonce: u64,
    pub difficulty: i16,
}

// #[derive(Debug, Serialize, Deserialize)]
// pub struct ContributionId {
//     pub id: i64,
// }

#[derive(Debug, Serialize, Deserialize)]
pub struct Transaction {
    pub id: i64,
    pub transaction_type: String,
    pub signature: String,
    pub priority_fee: i32,
    pub pool_id: i32,
    pub created: NaiveDateTime,
}

// #[derive(Debug, Serialize, Deserialize)]
// pub struct TransactionId {
//     pub id: i64,
// }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertTransaction {
    pub transaction_type: String,
    pub signature: String,
    pub priority_fee: i32,
    pub pool_id: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Challenge {
    pub id: i64,
    pub pool_id: i32,
    pub contribution_id: Option<i32>,
    pub challenge: Vec<u8>,
    pub rewards_earned: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertChallenge {
    pub pool_id: i32,
    pub challenge: Vec<u8>,
    pub rewards_earned: Option<i64>,
}

#[derive(Debug, Copy, Clone, Deserialize)]
pub struct InsertEarning {
    pub miner_id: i64,
    pub pool_id: i32,
    pub challenge_id: i64,
    pub amount: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InsertReward {
    pub miner_id: i64,
    pub pool_id: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateReward {
    pub miner_id: i64,
    pub balance: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Reward {
    pub balance: i64,
    pub miner_id: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Summary {
    pub miner_pubkey: String,
    pub num_of_contributions: i32,
    pub min_diff: i16,
    pub avg_diff: f64,
    pub max_diff: i16,
    pub earning_sub_total: i64,
    pub percent: String,
}
