-- SPDX-License-Identifier: Apache-2.0
CREATE TABLE app_join_teams (id bigint PRIMARY KEY, label text NOT NULL);
CREATE TABLE app_join_users (
  id bigint PRIMARY KEY,
  team_id bigint NOT NULL REFERENCES app_join_teams(id),
  email text NOT NULL UNIQUE
);
INSERT INTO app_join_teams (id, label) VALUES (1, 'core'), (2, 'edge');
INSERT INTO app_join_users (id, team_id, email)
VALUES (10, 1, 'one@example.test'), (20, 2, 'two@example.test');
DO $$
DECLARE
  observed text;
BEGIN
  EXECUTE 'SELECT teams.label FROM app_join_users AS users
           INNER JOIN app_join_teams AS teams ON users.team_id = teams.id
           WHERE users.email = $1'
    INTO observed USING 'two@example.test'::text;
  IF observed IS DISTINCT FROM 'edge' THEN RAISE EXCEPTION 'inner join changed'; END IF;
END
$$;
DROP TABLE app_join_users;
DROP TABLE app_join_teams;
SELECT 'ok:basic-joins';
