use crate::models;
use deadpool_sqlite::{Config, Pool, Runtime};
use rusqlite::Connection;
use std::{default, fmt, io, str::FromStr};
use tracing::{error, info};

pub struct PoweredByParams<'p> {
    // File path for the ore private pool database file relative to current directory
    // pub db_file_path: &'p str,
    pub database_uri: &'p str,
    pub initialized: bool,
    pub corrupted: bool,
    // pub connection: Option<Connection>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
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

pub struct Database {
    connection_pool: Pool,
}

impl Database {
    pub fn new(database_uri: String) -> Self {
        let db_config = Config::new(database_uri);
        let connection_pool = db_config.create_pool(Runtime::Tokio1).unwrap();

        Database { connection_pool }
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
}
