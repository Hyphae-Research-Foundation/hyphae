-- SPDX-License-Identifier: Apache-2.0
CREATE TABLE app_limit_lookup (id bigint PRIMARY KEY, payload text NOT NULL);
INSERT INTO app_limit_lookup (id, payload) VALUES (1, 'one'), (2, 'two');
DO $$
DECLARE
  observed text;
BEGIN
  EXECUTE 'SELECT payload FROM app_limit_lookup WHERE id = $1 LIMIT 1'
    INTO observed USING 2::bigint;
  IF observed IS DISTINCT FROM 'two' THEN
    RAISE EXCEPTION 'redundant LIMIT changed point lookup';
  END IF;
END
$$;
DROP TABLE app_limit_lookup;
SELECT 'ok:redundant-limit-point-lookup';
