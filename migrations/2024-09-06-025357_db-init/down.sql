DROP TRIGGER IF EXISTS update_timestamp_trigger ON ore.earnings CASCADE;
DROP TRIGGER IF EXISTS update_timestamp_trigger ON ore.rewards CASCADE;
DROP TRIGGER IF EXISTS update_timestamp_trigger ON ore.claims CASCADE;
DROP TRIGGER IF EXISTS update_timestamp_trigger ON ore.transactions CASCADE;
DROP TRIGGER IF EXISTS update_timestamp_trigger ON ore.submissions CASCADE;
DROP TRIGGER IF EXISTS update_timestamp_trigger ON ore.challenges CASCADE;
DROP TRIGGER IF EXISTS update_timestamp_trigger ON ore.pools CASCADE;
DROP TRIGGER IF EXISTS update_timestamp_trigger ON ore.miners CASCADE;

DROP FUNCTION IF EXISTS update_timestamp() CASCADE;

DROP TABLE IF EXISTS ore.earnings;
DROP TABLE IF EXISTS ore.rewards;
DROP TABLE IF EXISTS ore.claims;
DROP TABLE IF EXISTS ore.transactions;
DROP TABLE IF EXISTS ore.submissions;
DROP TABLE IF EXISTS ore.challenges;
DROP TABLE IF EXISTS ore.pools;
DROP TABLE IF EXISTS ore.miners;
