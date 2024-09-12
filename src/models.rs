// pub struct Miner {
//     pub id: i64,
//     pub pubkey: String,
//     pub enabled: bool,
//     pub status: MinerStatus,
// }

// struct InsertMiner<'p> {
//     /// We allow this to be `NULL`, in which case SQLite will assign a
//     /// fresh integer row ID upon insertion.
//     id: Option<i64>,
//     pubkey: &'p str,
//     enabled: bool,
//     status: MinerStatus,
// }

// enum MinerStatus {
//     Enrolled,
//     Activated,
//     Frozen,
//     Deactivated,
// }

pub struct Pool {
    pub id: i32,
    pub proof_pubkey: String,
    pub authority_pubkey: String,
    pub total_rewards: i64,
    pub claimed_rewards: i64,
}

// #[derive(Clone, Debug, Table, Param, ResultRecord)]
// #[nanosql(rename = "challenges")]
// struct Challenge {
//     #[nanosql(pk)]
//     id: i64,
//     pool_id: i32,
// 	pub submission_id: Option<i32>,
// 	pub challenge: Vec<u8>,
// 	pub rewards_earned: Option<i64>,
// }

// #[derive(Clone, Debug, Table, Param, ResultRecord)]
// pub struct ChallengeWithDifficulty {
// 	#[diesel(sql_type = Integer)]
// 	pub id: i64,
// 	#[diesel(sql_type = Nullable<BigInt>)]
// 	pub rewards_earned: Option<i64>,
// 	#[diesel(sql_type = SmallInt)]
// 	pub difficulty: i16,
// 	#[diesel(sql_type = Timestamptz)]
// 	pub updated: NaiveDateTime,
// }

// #[derive(Debug, Clone, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::challenges)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct InsertChallenge {
// 	pub pool_id: i32,
// 	pub challenge: Vec<u8>,
// 	pub rewards_earned: Option<i64>,
// }

// #[derive(Debug, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::challenges)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct UpdateChallengeRewards {
// 	pub rewards_earned: Option<i64>,
// }

// #[derive(Debug, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::claims)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct Claim {
// 	pub miner_id: i64,
// 	pub pool_id: i32,
// 	pub transaction_id: i64,
// 	pub amount: i64,
// }

// #[derive(Debug, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::claims)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct LastClaim {
// 	pub created: NaiveDateTime,
// }

// #[derive(Debug, Copy, Clone, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::claims)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct InsertClaim {
// 	pub miner_id: i64,
// 	pub pool_id: i32,
// 	pub transaction_id: i64,
// 	pub amount: i64,
// }

// #[derive(Debug, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::pools)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct Pool {
// 	pub id: i32,
// 	pub proof_pubkey: String,
// 	pub authority_pubkey: String,
// 	pub total_rewards: i64,
// 	pub claimed_rewards: i64,
// }

// #[derive(Debug, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::submissions)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct Submission {
// 	pub id: i64,
// 	pub miner_id: i64,
// 	pub challenge_id: i64,
// 	pub nonce: i64,
// 	pub difficulty: i16,
// 	pub created: NaiveDateTime,
// }

// #[derive(Debug, Deserialize, Serialize, QueryableByName)]
// pub struct SubmissionWithPubkey {
// 	#[diesel(sql_type = Integer)]
// 	pub id: i64,
// 	#[diesel(sql_type = Integer)]
// 	pub miner_id: i64,
// 	#[diesel(sql_type = Integer)]
// 	pub challenge_id: i64,
// 	#[diesel(sql_type = BigInt)]
// 	pub nonce: i64,
// 	#[diesel(sql_type = SmallInt)]
// 	pub difficulty: i16,
// 	#[diesel(sql_type = Timestamp)]
// 	pub created: NaiveDateTime,
// 	#[diesel(sql_type = Text)]
// 	pub pubkey: String,
// }

// #[derive(Debug, Clone, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::submissions)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct InsertSubmission {
// 	pub miner_id: i64,
// 	pub challenge_id: i64,
// 	pub nonce: i64,
// 	pub difficulty: i16,
// }

// #[derive(Debug, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::submissions)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct SubmissionWithId {
// 	pub id: i64,
// }

// #[derive(Debug, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::transactions)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct Transaction {
// 	pub id: i64,
// 	pub transaction_type: String,
// 	pub signature: String,
// 	pub priority_fee: i32,
// 	pub created: NaiveDateTime,
// }

// #[derive(Debug, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::transactions)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct TransactionId {
// 	pub id: i64,
// }

// #[derive(Debug, Clone, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::transactions)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct InsertTransaction {
// 	pub transaction_type: String,
// 	pub signature: String,
// 	pub priority_fee: i32,
// }

// #[derive(Debug, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::rewards)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct InsertReward {
// 	pub miner_id: i64,
// 	pub pool_id: i32,
// }

// #[derive(Debug, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::rewards)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct UpdateReward {
// 	pub miner_id: i64,
// 	pub balance: i64,
// }

// #[derive(Debug, Serialize, Deserialize, Queryable, Selectable, QueryableByName)]
// #[diesel(table_name = crate::schema::rewards)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct Reward {
// 	pub balance: i64,
// 	pub miner_id: i64,
// }

// #[derive(Debug, Copy, Clone, Deserialize, Insertable)]
// #[diesel(table_name = crate::schema::earnings)]
// #[diesel(check_for_backend(diesel::pg::Pg))]
// pub struct InsertEarning {
// 	pub miner_id: i64,
// 	pub pool_id: i32,
// 	pub challenge_id: i64,
// 	pub amount: i64,
// }
