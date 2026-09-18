-- SPDX-License-Identifier: Apache-2.0
CREATE TABLE app_returning (id bigint PRIMARY KEY, payload text NOT NULL);
DO $$
DECLARE
  observed_id bigint;
  observed_payload text;
BEGIN
  INSERT INTO app_returning (id, payload) VALUES (1, 'before')
    RETURNING id, payload INTO observed_id, observed_payload;
  IF observed_id <> 1 OR observed_payload <> 'before' THEN
    RAISE EXCEPTION 'INSERT RETURNING changed';
  END IF;
  UPDATE app_returning SET payload = 'after' WHERE id = 1
    RETURNING payload INTO observed_payload;
  IF observed_payload <> 'after' THEN RAISE EXCEPTION 'UPDATE RETURNING changed'; END IF;
  DELETE FROM app_returning WHERE id = 1 RETURNING id INTO observed_id;
  IF observed_id <> 1 THEN RAISE EXCEPTION 'DELETE RETURNING changed'; END IF;
END
$$;
DROP TABLE app_returning;
SELECT 'ok:returning';
