-- SPDX-License-Identifier: Apache-2.0
CREATE TABLE CoreIdentifiers (ItemId bigint PRIMARY KEY);
CREATE TABLE "CoreIdentifiersQuoted" ("ItemId" bigint PRIMARY KEY);
INSERT INTO COREIDENTIFIERS (ITEMID) VALUES (1);
INSERT INTO "CoreIdentifiersQuoted" ("ItemId") VALUES (2);
DO $$
BEGIN
  IF (SELECT itemid FROM coreidentifiers) <> 1 THEN
    RAISE EXCEPTION 'unquoted identifier folding changed';
  END IF;
  IF (SELECT "ItemId" FROM "CoreIdentifiersQuoted") <> 2 THEN
    RAISE EXCEPTION 'quoted identifier preservation changed';
  END IF;
  IF to_regclass('coreidentifiers') IS NULL
     OR to_regclass('"CoreIdentifiersQuoted"') IS NULL
     OR to_regclass('coreidentifiersquoted') IS NOT NULL THEN
    RAISE EXCEPTION 'identifier catalog identity changed';
  END IF;
END
$$;
DROP TABLE coreidentifiers;
DROP TABLE "CoreIdentifiersQuoted";
SELECT 'ok:identifiers';
