/*
    DATABASE_URL = "db/ore_priv_pool.db.sqlite3"
*/

DROP TABLE IF EXISTS miners;
DROP TABLE IF EXISTS members;
DROP TABLE IF EXISTS pools;
DROP TABLE IF EXISTS challenges;
DROP TABLE IF EXISTS submissions;
DROP TABLE IF EXISTS contributions;
DROP TABLE IF EXISTS transactions;
DROP TABLE IF EXISTS claims;
DROP TABLE IF EXISTS rewards;
DROP TABLE IF EXISTS earnings;

BEGIN TRANSACTION;
/*
    id - primary key, auto created index
    PostgreSQL automatically creates a unique index when a unique constraint or primary key is defined for a table.
    user index naming convention(with prefix):
    pkey_: primary key
    ukey_: user key
    fkey_: foreign key
    uniq_: unique index
    indx_: other index (multiple values)

    status: Enrolled, Activated, Frozen, Deactivated
*/

CREATE TABLE miners (
    id INTEGER PRIMARY KEY,
    pubkey VARCHAR(44) NOT NULL,
    enabled BOOLEAN DEFAULT false NOT NULL,
    status VARCHAR(30) DEFAULT 'Enrolled' NOT NULL,
    created TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE TRIGGER IF NOT EXISTS miners_update_timestamp_trigger
AFTER UPDATE ON miners
WHEN old.updated <> current_timestamp
BEGIN
     UPDATE miners
    SET updated = CURRENT_TIMESTAMP
    WHERE id = OLD.id;
END;

CREATE UNIQUE INDEX uniq_miners_pubkey ON miners (pubkey ASC);


/*
    id - primary key, auto created index
    PostgreSQL automatically creates a unique index when a unique constraint or primary key is defined for a table.
    user index naming convention(with prefix):
    pkey_: primary key
    ukey_: user key
    fkey_: foreign key
    uniq_: unique index
    indx_: other index (multiple values)

    status: Enrolled, Activated, Frozen, Deactivated
*/

CREATE TABLE members (
    id INTEGER PRIMARY KEY,
    pubkey VARCHAR(44) NOT NULL,
    enabled BOOLEAN DEFAULT false NOT NULL,
    role_contributor BOOLEAN DEFAULT false NOT NULL,
    role_miner BOOLEAN DEFAULT false NOT NULL,
    role_operator BOOLEAN DEFAULT false NOT NULL,
    status VARCHAR(30) DEFAULT 'Enrolled' NOT NULL,
    created TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE TRIGGER IF NOT EXISTS members_update_timestamp_trigger
AFTER UPDATE ON members
WHEN old.updated <> current_timestamp
BEGIN
     UPDATE members
    SET updated = CURRENT_TIMESTAMP
    WHERE id = OLD.id;
END;

CREATE UNIQUE INDEX uniq_members_pubkey ON members (pubkey ASC);


CREATE TABLE pools (
    id INTEGER PRIMARY KEY,
    proof_pubkey VARCHAR(44) NOT NULL,
    authority_pubkey VARCHAR(44) NOT NULL,
    total_rewards BIGINT DEFAULT 0 NOT NULL,
    claimed_rewards BIGINT DEFAULT 0 NOT NULL,
    created TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE TRIGGER IF NOT EXISTS pools_update_timestamp_trigger
AFTER UPDATE ON pools
WHEN old.updated <> current_timestamp
BEGIN
     UPDATE pools
    SET updated = CURRENT_TIMESTAMP
    WHERE id = OLD.id;
END;

CREATE UNIQUE INDEX uniq_pools_proof_pubkey ON pools (proof_pubkey ASC);
CREATE INDEX indx_pools_authority_pubkey ON pools (authority_pubkey ASC);


CREATE TABLE challenges (
    id INTEGER PRIMARY KEY,
    pool_id INT NOT NULL,
    contribution_id INT,
    challenge BYTEA NOT NULL,
    rewards_earned BIGINT DEFAULT 0,
    created TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE TRIGGER IF NOT EXISTS challenges_update_timestamp_trigger
AFTER UPDATE ON challenges
WHEN old.updated <> current_timestamp
BEGIN
     UPDATE challenges
    SET updated = CURRENT_TIMESTAMP
    WHERE id = OLD.id;
END;

CREATE INDEX uniq_challenges_challenge ON challenges (challenge ASC);


CREATE TABLE submissions (
    id INTEGER PRIMARY KEY,
    miner_id INT NOT NULL,
    challenge_id INT NOT NULL,
    difficulty SMALLINT NOT NULL,
    nonce BIGINT NOT NULL,
    digest BYTEA,
    created TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE TRIGGER IF NOT EXISTS submissions_update_timestamp_trigger
AFTER UPDATE ON submissions
WHEN old.updated <> current_timestamp
BEGIN
     UPDATE submissions
    SET updated = CURRENT_TIMESTAMP
    WHERE id = OLD.id;
END;

CREATE INDEX indx_submissions_miner_challenge_ids ON submissions (miner_id ASC, challenge_id ASC);
CREATE INDEX indx_submissions_nonce ON submissions (nonce ASC);


CREATE TABLE contributions (
    id INTEGER PRIMARY KEY,
    miner_id INT NOT NULL,
    challenge_id INT NOT NULL,
    difficulty SMALLINT NOT NULL,
    nonce BIGINT NOT NULL,
    digest BYTEA,
    created TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE TRIGGER IF NOT EXISTS contributions_update_timestamp_trigger
AFTER UPDATE ON contributions
WHEN old.updated <> current_timestamp
BEGIN
     UPDATE contributions
    SET updated = CURRENT_TIMESTAMP
    WHERE id = OLD.id;
END;

CREATE INDEX indx_contributions_miner_challenge_ids ON contributions (miner_id ASC, challenge_id ASC);
CREATE INDEX indx_contributions_nonce ON contributions (nonce ASC);
CREATE INDEX indx_contributions_created ON contributions (created DESC);
CREATE INDEX indx_contributions_challenge_id ON contributions (challenge_id ASC);


CREATE TABLE transactions (
    id INTEGER PRIMARY KEY,
    transaction_type VARCHAR(30) NOT NULL,
    signature VARCHAR(200) NOT NULL,
    priority_fee INT DEFAULT 0 NOT NULL,
    pool_id INT,
    miner_id INT,
    created TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE TRIGGER IF NOT EXISTS transactions_update_timestamp_trigger
AFTER UPDATE ON transactions
WHEN old.updated <> current_timestamp
BEGIN
     UPDATE transactions
    SET updated = CURRENT_TIMESTAMP
    WHERE id = OLD.id;
END;

CREATE INDEX indx_transactions_signature ON transactions (signature ASC);
CREATE INDEX indx_transactions_pool_id_created ON transactions (pool_id ASC, transaction_type ASC, created DESC);
CREATE INDEX indx_transactions_miner_id_created ON transactions (miner_id ASC, created DESC);


CREATE TABLE claims (
    id INTEGER PRIMARY KEY,
    miner_id INT NOT NULL,
    pool_id INT NOT NULL,
    transaction_id INT NOT NULL,
    amount BIGINT NOT NULL,
    created TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE TRIGGER IF NOT EXISTS claims_update_timestamp_trigger
AFTER UPDATE ON claims
WHEN old.updated <> current_timestamp
BEGIN
     UPDATE claims
    SET updated = CURRENT_TIMESTAMP
    WHERE id = OLD.id;
END;

CREATE INDEX indx_claims_miner_pool_txn_ids ON claims (miner_id ASC, pool_id ASC, transaction_id ASC);


CREATE TABLE rewards (
    id INTEGER PRIMARY KEY,
    miner_id INT NOT NULL,
    pool_id INT NOT NULL,
    balance BIGINT DEFAULT 0 NOT NULL,
    created TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE TRIGGER IF NOT EXISTS rewards_update_timestamp_trigger
AFTER UPDATE ON rewards
WHEN old.updated <> current_timestamp
BEGIN
     UPDATE rewards
    SET updated = CURRENT_TIMESTAMP
    WHERE id = OLD.id;
END;

CREATE INDEX indx_rewards_miner_pool_ids ON rewards (miner_id ASC, pool_id ASC);


CREATE TABLE earnings (
    id INTEGER PRIMARY KEY,
    miner_id INT NOT NULL,
    pool_id INT NOT NULL,
    challenge_id INT NOT NULL,
    amount BIGINT DEFAULT 0 NOT NULL,
    created TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL
);

CREATE TRIGGER IF NOT EXISTS earnings_update_timestamp_trigger
AFTER UPDATE ON earnings
WHEN old.updated <> current_timestamp
BEGIN
     UPDATE earnings
    SET updated = CURRENT_TIMESTAMP
    WHERE id = OLD.id;
END;

CREATE INDEX indx_earnings_miner_pool_challenge_ids ON earnings (miner_id ASC, pool_id ASC, challenge_id ASC);

CREATE TABLE init_completion (
    id INTEGER PRIMARY KEY,
    init_completed BOOLEAN DEFAULT false NOT NULL
);

INSERT INTO init_completion (init_completed) VALUES (true);

COMMIT;