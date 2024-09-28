use {
    crate::models::{self, *},
    // deadpool_sqlite::{Config, Pool, Runtime},
    // rusqlite::params,
    // serde::Deserialize,
    std::{fmt, io, str::FromStr},
    tracing::{error, info, warn},
};
#[cfg(feature = "powered-by-dbms-postgres")]
use {
    deadpool_postgres::{Client, Config, ManagerConfig, Pool, RecyclingMethod, Runtime},
    rustls::{Certificate, ClientConfig as RustlsClientConfig},
    std::{fs::File, io::BufReader},
    tokio_postgres::NoTls,
    tokio_postgres_rustls::MakeRustlsConnect,
    tracing::debug,
};
#[cfg(feature = "powered-by-dbms-sqlite")]
use {
    deadpool_sqlite::{Config, Connection, Pool, Runtime},
    rusqlite::params,
};

pub struct PoweredByParams<'p> {
    // File path for the ore private pool database file relative to current directory
    // pub db_file_path: &'p str,
    pub database_uri: &'p str,
    pub initialized: bool,
    pub corrupted: bool,
    // pub connection: Option<Connection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PoweredByDbms {
    Mysql,
    Postgres,
    Sqlite,
    Unavailable,
}

impl fmt::Display for PoweredByDbms {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PoweredByDbms::Mysql => write!(f, "mysql"),
            PoweredByDbms::Postgres => write!(f, "postgres"),
            PoweredByDbms::Sqlite => write!(f, "sqlite3"),
            PoweredByDbms::Unavailable => write!(f, "unavailable"),
        }
    }
}

impl FromStr for PoweredByDbms {
    type Err = io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "mysql" => Ok(PoweredByDbms::Mysql),
            "postgres" => Ok(PoweredByDbms::Postgres),
            "sqlite3" => Ok(PoweredByDbms::Sqlite),
            _ => Ok(PoweredByDbms::Unavailable),
            // _ => Err(io::Error::new(io::ErrorKind::InvalidData, "Unsupported dbms type")),
        }
    }
}

#[derive(Debug)]
pub enum DatabaseError {
    FailedToGetConnectionFromPool,
    FailedToUpdateRow,
    FailedToInsertRow,
    InteractionFailed,
    QueryFailed,
}

#[cfg(feature = "powered-by-dbms-postgres")]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct PgConfig {
    db_ca_cert: Option<String>,
    listen: Option<String>,
    pg: deadpool_postgres::Config,
}

#[cfg(feature = "powered-by-dbms-postgres")]
impl PgConfig {
    pub fn from_env() -> Result<Self, config::ConfigError> {
        config::Config::builder()
            .add_source(config::Environment::default().separator("__"))
            .build()
            .unwrap()
            .try_deserialize()
    }
}

pub struct Database {
    connection_pool: Pool,
}

impl Database {
    #[cfg(feature = "powered-by-dbms-postgres")]
    pub fn new(database_uri: String) -> Self {
        dotenvy::dotenv().ok();
        let pg_config = PgConfig::from_env().unwrap();
        debug!("pg_config settings: {:?}", pg_config);
        // let connection_pool =
        //     pg_config.pg.create_pool(Some(Runtime::Tokio1), tokio_postgres::NoTls).unwrap();

        let connection_pool = if let Some(ca_cert) = pg_config.db_ca_cert {
            let cert_file = File::open(ca_cert).unwrap();
            let mut buf = BufReader::new(cert_file);
            let mut root_store = rustls::RootCertStore::empty();
            let certs: Vec<_> = rustls_pemfile::certs(&mut buf).collect();
            let my_certs: Vec<_> = certs.iter().map(|c| Certificate(c.unwrap().to_vec())).collect();
            for cert in my_certs {
                root_store.add(&cert).unwrap();
            }

            // for cert in rustls_pemfile::certs(&mut buf) {
            //     root_store.add(&cert.unwrap())?;
            // }

            let tls_config = RustlsClientConfig::builder()
                .with_safe_defaults()
                .with_root_certificates(root_store)
                .with_no_client_auth();

            let tls = MakeRustlsConnect::new(tls_config);
            pg_config.pg.create_pool(Some(Runtime::Tokio1), tls)?
        } else {
            pg_config.pg.create_pool(Some(Runtime::Tokio1), NoTls)?
        };

        Database { connection_pool }
    }

    #[cfg(feature = "powered-by-dbms-sqlite")]
    pub fn new(database_uri: String) -> Self {
        let db_config = Config::new(database_uri);
        let connection_pool = db_config.create_pool(Runtime::Tokio1).unwrap();

        Database { connection_pool }
    }

    #[cfg(feature = "powered-by-dbms-postgres")]
    pub async fn get_connection(&self) -> Result<Client, String> {
        // let client: Client = pool.get().await?;
        self.connection_pool.get().await.map_err(|err| err.to_string())
    }

    #[cfg(feature = "powered-by-dbms-sqlite")]
    pub async fn _get_connection(&self) -> Result<Connection, String> {
        self.connection_pool.get().await.map_err(|err| err.to_string())
    }

    pub async fn get_pool_by_authority_pubkey(
        &self,
        pool_pubkey: String,
    ) -> Result<models::Pool, DatabaseError> {
        let sql = "SELECT id, proof_pubkey, authority_pubkey, total_rewards, claimed_rewards FROM pools WHERE pools.authority_pubkey = ?";

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    let row = conn.query_row_and_then(sql, &[&pool_pubkey], |row| {
                        Ok::<models::Pool, rusqlite::Error>(models::Pool {
                            id: row.get(0)?,
                            proof_pubkey: row.get(1)?,
                            authority_pubkey: row.get(2)?,
                            total_rewards: row.get(3)?,
                            claimed_rewards: row.get(4)?,
                        })
                    });
                    row
                })
                .await;

            match res {
                Ok(Ok(pool)) => Ok(pool),
                Ok(Err(e)) => match e {
                    rusqlite::Error::QueryReturnedNoRows => {
                        warn!("Query returned no rows.");
                        return Err(DatabaseError::QueryFailed);
                    },
                    e => {
                        error!("Query error: {}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }

    pub async fn add_new_pool(
        &self,
        authority_pubkey: String,
        proof_pubkey: String,
    ) -> Result<(), DatabaseError> {
        let sql = "INSERT INTO pools (authority_pubkey, proof_pubkey) VALUES (?, ?)";

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| conn.execute(sql, &[&authority_pubkey, &proof_pubkey]))
                .await;

            match res {
                Ok(interaction) => match interaction {
                    Ok(query) => {
                        if query != 1 {
                            return Err(DatabaseError::FailedToInsertRow);
                        }
                        return Ok(());
                    },
                    Err(e) => {
                        error!("{:?}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        };
    }

    pub async fn update_pool_rewards(
        &self,
        pool_authority_pubkey: String,
        earned_rewards: i64,
    ) -> Result<(), DatabaseError> {
        let sql = "UPDATE pools SET total_rewards = total_rewards + ? WHERE authority_pubkey = ?";

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    conn.execute(
                        sql,
                        // &[&earned_rewards, &pool_authority_pubkey]
                        params![&earned_rewards, &pool_authority_pubkey],
                    )
                })
                .await;

            match res {
                Ok(interaction) => match interaction {
                    Ok(query) => {
                        if query != 1 {
                            return Err(DatabaseError::FailedToUpdateRow);
                        }
                        info!("Successfully updated pool rewards");
                        return Ok(());
                    },
                    Err(e) => {
                        error!("{:?}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }

    pub async fn add_new_miner(
        &self,
        miner_pubkey: String,
        is_enabled: bool,
        status: String,
    ) -> Result<(), DatabaseError> {
        let sql = "INSERT INTO miners (pubkey, enabled, status) VALUES (?, ?, ?)";

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    conn.execute(
                        sql,
                        // &[&miner_pubkey, &is_enabled, &status]
                        params![&miner_pubkey, &is_enabled, &status],
                    )
                })
                .await;

            match res {
                Ok(interaction) => match interaction {
                    Ok(query) => {
                        if query != 1 {
                            return Err(DatabaseError::FailedToInsertRow);
                        }
                        return Ok(());
                    },
                    Err(e) => {
                        error!("{:?}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }

    pub async fn get_miner_by_pubkey_str(
        &self,
        miner_pubkey: String,
    ) -> Result<Miner, DatabaseError> {
        let sql = "SELECT id, pubkey, enabled, status FROM miners WHERE miners.pubkey = ?";

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    let row = conn.query_row_and_then(sql, &[&miner_pubkey], |row| {
                        Ok::<Miner, rusqlite::Error>(Miner {
                            id: row.get(0)?,
                            pubkey: row.get(1)?,
                            enabled: row.get(2)?,
                            status: row.get(3)?,
                        })
                    });
                    row
                })
                .await;

            match res {
                Ok(Ok(miner)) => Ok(miner),
                Ok(Err(e)) => match e {
                    rusqlite::Error::QueryReturnedNoRows => {
                        warn!("Query returned no rows.");
                        return Err(DatabaseError::QueryFailed);
                    },
                    e => {
                        error!("Query error: {}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }

    pub async fn get_challenge_by_challenge(
        &self,
        challenge: Vec<u8>,
    ) -> Result<Challenge, DatabaseError> {
        let sql = "SELECT id, pool_id, contribution_id, challenge, rewards_earned FROM challenges WHERE challenges.challenge = ?";

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    let row = conn.query_row_and_then(sql, &[&challenge], |row| {
                        Ok::<Challenge, rusqlite::Error>(Challenge {
                            id: row.get(0)?,
                            pool_id: row.get(1)?,
                            contribution_id: row.get(2)?,
                            challenge: row.get(3)?,
                            rewards_earned: row.get(4)?,
                        })
                    });
                    row
                })
                .await;

            match res {
                Ok(Ok(challenge)) => Ok(challenge),
                Ok(Err(e)) => match e {
                    rusqlite::Error::QueryReturnedNoRows => {
                        warn!("Query returned no rows.");
                        return Err(DatabaseError::QueryFailed);
                    },
                    e => {
                        error!("Query error: {}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }

    pub async fn get_contribution_id_with_nonce(&self, nonce: u64) -> Result<i64, DatabaseError> {
        let sql = "SELECT id FROM contributions WHERE contributions.nonce = ? ORDER BY id DESC";

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    let row = conn.query_row_and_then(sql, &[&(nonce as i64)], |row| {
                        Ok::<i64, rusqlite::Error>(row.get(0)?)
                    });
                    row
                })
                .await;

            match res {
                Ok(Ok(contribution_id)) => Ok(contribution_id),
                Ok(Err(e)) => match e {
                    rusqlite::Error::QueryReturnedNoRows => {
                        error!("Query returned no rows.");
                        return Err(DatabaseError::QueryFailed);
                    },
                    e => {
                        error!("Query error: {}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }

    pub async fn update_challenge_rewards(
        &self,
        challenge: Vec<u8>,
        contribution_id: i64,
        rewards: i64,
    ) -> Result<(), DatabaseError> {
        let sql =
            "UPDATE challenges SET rewards_earned = ?, contribution_id = ? WHERE challenge = ?";

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    conn.execute(
                        sql,
                        // &[&pool_authority_pubkey, &earned_rewards]
                        params![&rewards, &contribution_id, &challenge],
                    )
                })
                .await;

            match res {
                Ok(interaction) => match interaction {
                    Ok(query) => {
                        if query != 1 {
                            return Err(DatabaseError::FailedToUpdateRow);
                        }
                        info!("Successfully updated challenge rewards");
                        return Ok(());
                    },
                    Err(e) => {
                        error!("{:?}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }

    pub async fn add_new_challenge(&self, challenge: InsertChallenge) -> Result<(), DatabaseError> {
        let sql = r#"INSERT INTO challenges (pool_id, challenge, rewards_earned) VALUES (?, ?, ?)"#;

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    conn.execute(
                        sql,
                        // &[&challenge.pool_id, &challenge.challenge, &challenge.rewards_earned],
                        params![challenge.pool_id, challenge.challenge, challenge.rewards_earned],
                    )
                })
                .await;

            match res {
                Ok(interaction) => match interaction {
                    Ok(query) => {
                        if query != 1 {
                            return Err(DatabaseError::FailedToInsertRow);
                        }
                        return Ok(());
                    },
                    Err(e) => {
                        error!("{:?}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }

    pub async fn add_new_transaction(&self, txn: InsertTransaction) -> Result<(), DatabaseError> {
        let sql = r#"INSERT INTO transactions (transaction_type, signature, priority_fee, pool_id) VALUES (?, ?, ?, ?)"#;

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    conn.execute(
                        sql,
                        // &[&txn.transaction_type, &txn.signature, &txn.priority_fee,
                        // &txn.pool_id],
                        params![txn.transaction_type, txn.signature, txn.priority_fee, txn.pool_id],
                    )
                })
                .await;

            match res {
                Ok(interaction) => match interaction {
                    Ok(query) => {
                        if query != 1 {
                            return Err(DatabaseError::FailedToInsertRow);
                        }
                        return Ok(());
                    },
                    Err(e) => {
                        error!("{:?}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }

    pub async fn add_new_earnings_batch(
        &self,
        earnings: Vec<InsertEarning>,
    ) -> Result<(), DatabaseError> {
        let sql =
            r#"INSERT INTO earnings (miner_id, pool_id, challenge_id, amount) VALUES (?, ?, ?, ?)"#;

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    // conn.execute(
                    //     sql,
                    //     &[&earnings.miner_id, &earings.pool_id, &earnings.challenge_id,
                    // &earnings.amount], )

                    let mut rows_added = 0;
                    // or conn.unchecked_transaction() if you don't have &mut Connection
                    let tx = conn.transaction().unwrap();
                    let mut stmt = tx.prepare(sql).unwrap();
                    for earning in &earnings {
                        rows_added += stmt
                            .execute((
                                &earning.miner_id,
                                &earning.pool_id,
                                &earning.challenge_id,
                                &earning.amount,
                            ))
                            .unwrap();
                    }
                    drop(stmt);
                    tx.commit().unwrap();

                    Ok::<usize, rusqlite::Error>(rows_added)
                })
                .await;

            match res {
                // Ok(()) => Ok(()),
                Ok(interaction) => match interaction {
                    Ok(rows) => {
                        info!("Earnings inserted: {}", rows);
                        if rows == 0 {
                            return Err(DatabaseError::FailedToInsertRow);
                        }
                        return Ok(());
                    },
                    Err(e) => {
                        error!("{:?}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }

    pub async fn add_new_contributions_batch(
        &self,
        contributions: Vec<InsertContribution>,
    ) -> Result<(), DatabaseError> {
        let sql = r#"INSERT INTO contributions (miner_id, challenge_id, nonce, difficulty) VALUES (?, ?, ?, ?)"#;

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    let mut rows_added = 0;
                    // or conn.unchecked_transaction() if you don't have &mut Connection
                    let tx = conn.transaction().unwrap();
                    let mut stmt = tx.prepare(sql).unwrap();
                    for contribution in &contributions {
                        rows_added += stmt
                            .execute((
                                &contribution.miner_id,
                                &contribution.challenge_id,
                                &(contribution.nonce as i64),
                                &contribution.difficulty,
                            ))
                            .unwrap();
                    }
                    drop(stmt);
                    tx.commit().unwrap();

                    Ok::<usize, rusqlite::Error>(rows_added)
                })
                .await;

            match res {
                // Ok(()) => Ok(()),
                Ok(interaction) => match interaction {
                    Ok(rows) => {
                        info!("Contributions inserted: {}", rows);
                        if rows == 0 {
                            return Err(DatabaseError::FailedToInsertRow);
                        }
                        return Ok(());
                    },
                    Err(e) => {
                        error!("{:?}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }

    pub async fn _get_miner_rewards(&self, miner_pubkey: String) -> Result<Reward, DatabaseError> {
        let sql = r#"SELECT r.balance, r.miner_id FROM miners m JOIN rewards r ON m.id = r.miner_id WHERE m.pubkey = ?"#;

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    let row = conn.query_row_and_then(sql, &[&miner_pubkey], |row| {
                        Ok::<Reward, rusqlite::Error>(Reward {
                            balance: row.get(0)?,
                            miner_id: row.get(1)?,
                        })
                    });
                    row
                })
                .await;

            match res {
                Ok(Ok(reward)) => Ok(reward),
                Ok(Err(e)) => match e {
                    rusqlite::Error::QueryReturnedNoRows => {
                        error!("Query returned no rows.");
                        return Err(DatabaseError::QueryFailed);
                    },
                    e => {
                        error!("Query error: {}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }

    pub async fn add_new_reward(&self, reward: InsertReward) -> Result<(), DatabaseError> {
        let sql = r#"INSERT INTO rewards (miner_id, pool_id) VALUES (?, ?)"#;

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    conn.execute(
                        sql,
                        // &[&reward.miner_id, &reward.pool_id],
                        params![reward.miner_id, reward.pool_id,],
                    )
                })
                .await;

            match res {
                Ok(interaction) => match interaction {
                    Ok(query) => {
                        if query != 1 {
                            return Err(DatabaseError::FailedToInsertRow);
                        }
                        return Ok(());
                    },
                    Err(e) => {
                        error!("{:?}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }

    pub async fn update_rewards(&self, rewards: Vec<UpdateReward>) -> Result<(), DatabaseError> {
        let sql = r#"UPDATE rewards SET balance = balance + ? WHERE miner_id = ?"#;

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    let mut rows_added = 0;
                    // or conn.unchecked_transaction() if you don't have &mut Connection
                    let tx = conn.transaction().unwrap();
                    let mut stmt = tx.prepare(sql).unwrap();

                    for reward in rewards {
                        rows_added += stmt.execute((&reward.balance, &reward.miner_id)).unwrap();
                    }
                    drop(stmt);
                    tx.commit().unwrap();

                    Ok::<usize, rusqlite::Error>(rows_added)
                })
                .await;

            match res {
                // Ok(()) => Ok(()),
                Ok(interaction) => match interaction {
                    Ok(rows) => {
                        info!("Rewards updated: {}", rows);
                        if rows == 0 {
                            return Err(DatabaseError::FailedToUpdateRow);
                        }
                        return Ok(());
                    },
                    Err(e) => {
                        error!("{:?}", e);
                        return Err(DatabaseError::QueryFailed);
                    },
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }

    pub async fn get_summaries_for_last_24_hours(
        &self,
        pool_id: i32,
    ) -> Result<Vec<Summary>, DatabaseError> {
        let sql = r#"
SELECT
        m.pubkey                                      as miner_pubkey,
        COUNT(c.id)                                   as num_of_contributions,
        MIN(c.difficulty)                             as min_diff,
        ROUND(CAST(AVG(c.difficulty) AS REAL), 2)     as avg_diff,
        MAX(c.difficulty)                             as max_diff,
        SUM(e.amount)                                 as earning_sub_total,
        ROUND(CAST(SUM(e.amount) AS REAL) / CAST(SUM(SUM(e.amount)) OVER () AS REAL) * 100, 2) AS percent
    FROM
        contributions c
            INNER JOIN miners m ON c.miner_id = m.id
            INNER JOIN earnings e ON c.challenge_id = e.challenge_id AND c.miner_id = e.miner_id AND e.pool_id = ?
            INNER JOIN pools p ON e.pool_id = p.id
    WHERE
        c.created >= datetime('now', '-24 hour') and
        c.created < 'now' and
        m.enabled = true
    GROUP BY m.pubkey
    ORDER BY percent DESC
        "#;

        if let Ok(db_conn) = self.connection_pool.get().await {
            let res = db_conn
                .interact(move |conn| {
                    // let row = conn.query_row_and_then(sql, &[&pool_id], |row| {
                    //     Ok::<Summary, rusqlite::Error>(Summary {
                    //         miner_pubkey: row.get(0)?,
                    //         num_of_contributions: row.get(1)?,
                    //         min_diff: row.get(2)?,
                    //         avg_diff: row.get(3)?,
                    //         max_diff: row.get(4)?,
                    //         earning_sub_total: row.get(5)?,
                    //         percent: row.get(6)?,
                    //     })
                    // });
                    // row

                    let mut summaries = vec![];
                    let mut stmt = conn.prepare(sql).unwrap();
                    let summary_iter = stmt
                        .query_map(&[&pool_id], |row| {
                            Ok::<Summary, rusqlite::Error>(Summary {
                                miner_pubkey: row.get(0)?,
                                num_of_contributions: row.get(1)?,
                                min_diff: row.get(2)?,
                                avg_diff: row.get(3)?,
                                max_diff: row.get(4)?,
                                earning_sub_total: row.get(5)?,
                                percent: row.get(6)?,
                            })
                        })
                        .unwrap();

                    for summary in summary_iter {
                        summaries.push(summary);
                    }
                    summaries
                })
                .await;

            match res {
                Ok(items) => match &items[..] {
                    [Ok(_), ..] => Ok(items.into_iter().map(|s| s.unwrap()).collect()),
                    [Err(e), ..] => match e {
                        rusqlite::Error::QueryReturnedNoRows => {
                            warn!("Query returned no rows.");
                            return Err(DatabaseError::QueryFailed);
                        },
                        e => {
                            error!("Query error: {}", e);
                            return Err(DatabaseError::QueryFailed);
                        },
                    },
                    [] => todo!(),
                },
                Err(e) => {
                    error!("{:?}", e);
                    return Err(DatabaseError::InteractionFailed);
                },
            }
        } else {
            return Err(DatabaseError::FailedToGetConnectionFromPool);
        }
    }
}
