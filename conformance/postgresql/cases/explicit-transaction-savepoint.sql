-- SPDX-License-Identifier: Apache-2.0
CREATE TABLE app_transaction_savepoint (id bigint PRIMARY KEY, payload text NOT NULL);
BEGIN;
INSERT INTO app_transaction_savepoint (id, payload) VALUES (1, 'committed');
SAVEPOINT discard_second;
INSERT INTO app_transaction_savepoint (id, payload) VALUES (2, 'discarded');
ROLLBACK TO SAVEPOINT discard_second;
COMMIT;
BEGIN;
UPDATE app_transaction_savepoint SET payload = 'rolled-back' WHERE id = 1;
ROLLBACK;
DO $$
BEGIN
  IF (SELECT count(*) FROM app_transaction_savepoint) <> 1
     OR (SELECT payload FROM app_transaction_savepoint WHERE id = 1) <> 'committed' THEN
    RAISE EXCEPTION 'transaction or savepoint write set changed';
  END IF;
END
$$;
DROP TABLE app_transaction_savepoint;
SELECT 'ok:explicit-transaction-savepoint';
