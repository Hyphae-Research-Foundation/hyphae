-- SPDX-License-Identifier: Apache-2.0
CREATE TABLE app_constraint_parent (id bigint PRIMARY KEY);
CREATE TABLE app_constraint_child (
  id bigint PRIMARY KEY,
  parent_id bigint NOT NULL REFERENCES app_constraint_parent(id),
  balance bigint NOT NULL CHECK (balance >= 0),
  email text UNIQUE
);
INSERT INTO app_constraint_parent (id) VALUES (1);
INSERT INTO app_constraint_child (id, parent_id, balance, email)
VALUES (1, 1, 0, 'one@example.test');
DO $$
DECLARE
  observed text;
BEGIN
  BEGIN
    INSERT INTO app_constraint_child (id, parent_id, balance, email)
    VALUES (2, 1, 0, 'one@example.test');
    RAISE EXCEPTION 'expected unique violation';
  EXCEPTION WHEN unique_violation THEN
    GET STACKED DIAGNOSTICS observed = RETURNED_SQLSTATE;
    IF observed <> '23505' THEN RAISE EXCEPTION 'unique SQLSTATE %', observed; END IF;
  END;
  BEGIN
    INSERT INTO app_constraint_child (id, parent_id, balance, email)
    VALUES (3, NULL, 0, 'three@example.test');
    RAISE EXCEPTION 'expected not-null violation';
  EXCEPTION WHEN not_null_violation THEN
    GET STACKED DIAGNOSTICS observed = RETURNED_SQLSTATE;
    IF observed <> '23502' THEN RAISE EXCEPTION 'not-null SQLSTATE %', observed; END IF;
  END;
  BEGIN
    INSERT INTO app_constraint_child (id, parent_id, balance, email)
    VALUES (4, 1, -1, 'four@example.test');
    RAISE EXCEPTION 'expected check violation';
  EXCEPTION WHEN check_violation THEN
    GET STACKED DIAGNOSTICS observed = RETURNED_SQLSTATE;
    IF observed <> '23514' THEN RAISE EXCEPTION 'check SQLSTATE %', observed; END IF;
  END;
  BEGIN
    INSERT INTO app_constraint_child (id, parent_id, balance, email)
    VALUES (5, 99, 0, 'five@example.test');
    RAISE EXCEPTION 'expected foreign-key violation';
  EXCEPTION WHEN foreign_key_violation THEN
    GET STACKED DIAGNOSTICS observed = RETURNED_SQLSTATE;
    IF observed <> '23503' THEN RAISE EXCEPTION 'foreign-key SQLSTATE %', observed; END IF;
  END;
END
$$;
DROP TABLE app_constraint_child;
DROP TABLE app_constraint_parent;
SELECT 'ok:constraint-sqlstate';
