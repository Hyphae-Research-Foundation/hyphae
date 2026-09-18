-- SPDX-License-Identifier: Apache-2.0
CREATE TABLE app_group_by (id bigint PRIMARY KEY, region text NOT NULL);
INSERT INTO app_group_by (id, region)
VALUES (1, 'east'), (2, 'west'), (3, 'east');
DO $$
DECLARE
  observed text[];
BEGIN
  SELECT array_agg(region || ':' || item_count ORDER BY region)
    INTO observed
    FROM (
      SELECT region, count(*) AS item_count
      FROM app_group_by
      GROUP BY region
      ORDER BY region
      LIMIT 10
    ) AS grouped;
  IF observed IS DISTINCT FROM ARRAY['east:2', 'west:1'] THEN
    RAISE EXCEPTION 'bounded GROUP BY changed';
  END IF;
END
$$;
DROP TABLE app_group_by;
SELECT 'ok:group-by';
