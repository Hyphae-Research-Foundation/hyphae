-- SPDX-License-Identifier: Apache-2.0
CREATE TABLE app_like_bind (id bigint PRIMARY KEY, label text NOT NULL);
INSERT INTO app_like_bind (id, label)
VALUES (1, 'alpha'), (2, 'alpine'), (3, 'beta');
DO $$
DECLARE
  observed bigint[];
BEGIN
  EXECUTE 'SELECT array_agg(id ORDER BY id) FROM app_like_bind WHERE label LIKE $1'
    INTO observed USING 'alp%'::text;
  IF observed IS DISTINCT FROM ARRAY[1::bigint, 2::bigint] THEN
    RAISE EXCEPTION 'bound LIKE result changed';
  END IF;
END
$$;
DROP TABLE app_like_bind;
SELECT 'ok:like-bind';
