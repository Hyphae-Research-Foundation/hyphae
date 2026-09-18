-- SPDX-License-Identifier: Apache-2.0
CREATE TABLE app_on_conflict (id bigint PRIMARY KEY, payload text NOT NULL);
INSERT INTO app_on_conflict (id, payload) VALUES (1, 'before');
DO $$
DECLARE
  observed text;
  affected bigint;
BEGIN
  INSERT INTO app_on_conflict (id, payload) VALUES (1, 'after')
    ON CONFLICT (id) DO UPDATE SET payload = EXCLUDED.payload
    RETURNING payload INTO observed;
  IF observed <> 'after' THEN RAISE EXCEPTION 'ON CONFLICT DO UPDATE changed'; END IF;
  INSERT INTO app_on_conflict (id, payload) VALUES (1, 'ignored')
    ON CONFLICT (id) DO NOTHING;
  GET DIAGNOSTICS affected = ROW_COUNT;
  IF affected <> 0 THEN RAISE EXCEPTION 'ON CONFLICT DO NOTHING changed'; END IF;
END
$$;
DROP TABLE app_on_conflict;
SELECT 'ok:on-conflict';
