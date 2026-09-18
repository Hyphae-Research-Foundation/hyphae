-- SPDX-License-Identifier: Apache-2.0
CREATE TABLE app_typed_parameters (
  id bigint PRIMARY KEY,
  label text NOT NULL,
  active boolean NOT NULL
);
PREPARE app_typed_insert (bigint, text, boolean) AS
  INSERT INTO app_typed_parameters (id, label, active) VALUES ($1, $2, $3);
EXECUTE app_typed_insert(7, 'typed', true);
DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM app_typed_parameters
    WHERE id = 7 AND label = 'typed' AND active
  ) THEN
    RAISE EXCEPTION 'typed parameter values changed';
  END IF;
END
$$;
DEALLOCATE app_typed_insert;
DROP TABLE app_typed_parameters;
SELECT 'ok:typed-parameters';
