-- SPDX-License-Identifier: Apache-2.0
CREATE TABLE app_catalog_columns (
  id bigint PRIMARY KEY,
  label text NOT NULL,
  note text
);
DO $$
BEGIN
  IF (SELECT count(*) FROM information_schema.columns
      WHERE table_schema = 'public' AND table_name = 'app_catalog_columns') <> 3 THEN
    RAISE EXCEPTION 'catalog column count changed';
  END IF;
  IF NOT EXISTS (
    SELECT 1 FROM information_schema.columns
    WHERE table_schema = 'public'
      AND table_name = 'app_catalog_columns'
      AND column_name = 'label'
      AND ordinal_position = 2
      AND is_nullable = 'NO'
      AND data_type = 'text'
  ) THEN
    RAISE EXCEPTION 'catalog column metadata changed';
  END IF;
END
$$;
DROP TABLE app_catalog_columns;
SELECT 'ok:catalog-columns';
