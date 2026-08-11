// SPDX-License-Identifier: AGPL-3.0-only

//! Immediate MATCH SIMPLE foreign keys over native primary keys.

use hyphae_native_runtime::{GroupCommitOutcome, NativeDatabase, NativeRuntimeError, SqlError};
use hyphae_native_types::DurabilityClass;

#[test]
fn foreign_key_rejects_missing_parent_and_survives_reopen() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = std::env::temp_dir().join(format!("hyphae-native-fk-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut tx = database.begin_sql(1, DurabilityClass::Strict)?;
    tx.execute_sql("CREATE TABLE parents (id BIGINT PRIMARY KEY)", &[])?;
    tx.execute_sql(
        "CREATE TABLE children (id BIGINT PRIMARY KEY, parent_id BIGINT, CONSTRAINT children_parent_fk FOREIGN KEY (parent_id) REFERENCES parents (id))",
        &[],
    )?;
    assert!(matches!(
        tx.execute_sql("INSERT INTO children (id, parent_id) VALUES (1, 99)", &[]),
        Err(SqlError::ForeignKeyViolation)
    ));
    tx.execute_sql("INSERT INTO parents (id) VALUES (99)", &[])?;
    tx.execute_sql("INSERT INTO children (id, parent_id) VALUES (1, 99)", &[])?;
    assert!(matches!(
        tx.execute_sql("UPDATE children SET parent_id = 100 WHERE id = 1", &[]),
        Err(SqlError::ForeignKeyViolation)
    ));
    assert!(matches!(
        tx.execute_sql("DELETE FROM parents WHERE id = 99", &[]),
        Err(SqlError::ForeignKeyViolation)
    ));
    tx.execute_sql("INSERT INTO children (id, parent_id) VALUES (2, NULL)", &[])?;
    tx.execute_sql(
        "CREATE TABLE nodes (id BIGINT PRIMARY KEY, parent_id BIGINT, FOREIGN KEY (parent_id) REFERENCES nodes (id))",
        &[],
    )?;
    tx.execute_sql("INSERT INTO nodes (id, parent_id) VALUES (1, 1)", &[])?;
    tx.execute_sql(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT NOT NULL)",
        &[],
    )?;
    tx.execute_sql("CREATE UNIQUE INDEX users_email ON users (email)", &[])?;
    tx.execute_sql(
        "CREATE TABLE invites (id BIGINT PRIMARY KEY, email TEXT, FOREIGN KEY (email) REFERENCES users (email))",
        &[],
    )?;
    tx.execute_sql(
        "INSERT INTO users (id, email) VALUES (1, 'a@example.com')",
        &[],
    )?;
    tx.execute_sql(
        "INSERT INTO invites (id, email) VALUES (1, 'a@example.com')",
        &[],
    )?;
    tx.execute_sql(
        "CREATE TABLE nullable_users (id BIGINT PRIMARY KEY, email TEXT)",
        &[],
    )?;
    tx.execute_sql(
        "CREATE UNIQUE INDEX nullable_email ON nullable_users (email)",
        &[],
    )?;
    assert!(tx
        .execute_sql(
            "CREATE TABLE nullable_invites (id BIGINT PRIMARY KEY, email TEXT, FOREIGN KEY (email) REFERENCES nullable_users (email))",
            &[],
        )
        .is_err());
    assert!(matches!(
        tx.execute_sql(
            "INSERT INTO invites (id, email) VALUES (2, 'missing@example.com')",
            &[]
        ),
        Err(SqlError::ForeignKeyViolation)
    ));
    assert!(matches!(
        tx.execute_sql(
            "CREATE TABLE bad_fk (id BIGINT PRIMARY KEY, parent_id TEXT, FOREIGN KEY (parent_id) REFERENCES parents (id))",
            &[],
        ),
        Err(SqlError::TypeMismatch)
    ));
    assert!(tx
        .execute_sql(
            "CREATE TABLE bad_clause (id BIGINT PRIMARY KEY, parent_id BIGINT, FOREIGN KEY (parent_id) REFERENCES parents (id) ON DELETE CASCADE)",
            &[],
        )
        .is_err());
    assert!(tx
        .execute_sql(
            "CREATE TABLE duplicate_names (id BIGINT PRIMARY KEY, first_parent BIGINT, second_parent BIGINT, CONSTRAINT duplicate_fk FOREIGN KEY (first_parent) REFERENCES parents (id), CONSTRAINT duplicate_fk FOREIGN KEY (second_parent) REFERENCES parents (id))",
            &[],
        )
        .is_err());
    tx.commit()?;
    drop(database);

    let mut reopened = NativeDatabase::open(&temporary)?;
    let mut invalid = reopened.begin_sql(2, DurabilityClass::Memory)?;
    assert!(matches!(
        invalid.execute_sql("INSERT INTO children (id, parent_id) VALUES (3, 100)", &[]),
        Err(SqlError::ForeignKeyViolation)
    ));
    invalid.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
fn concurrent_parent_delete_wins_and_child_rebase_fails() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary =
        std::env::temp_dir().join(format!("hyphae-native-fk-race-{}", std::process::id()));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut seed = database.begin_sql(1, DurabilityClass::Strict)?;
    seed.execute_sql("CREATE TABLE parents (id BIGINT PRIMARY KEY)", &[])?;
    seed.execute_sql(
        "CREATE TABLE children (id BIGINT PRIMARY KEY, parent_id BIGINT, CONSTRAINT children_parent_fk FOREIGN KEY (parent_id) REFERENCES parents (id))",
        &[],
    )?;
    seed.execute_sql("INSERT INTO parents (id) VALUES (1)", &[])?;
    seed.commit()?;

    let mut child = database.begin_optimistic(2, DurabilityClass::Memory)?;
    child.execute_sql("INSERT INTO children (id, parent_id) VALUES (1, 1)", &[])?;
    let mut parent_delete = database.begin_optimistic(2, DurabilityClass::Memory)?;
    parent_delete.execute_sql("DELETE FROM parents WHERE id = 1", &[])?;
    database.commit_optimistic(parent_delete)?;
    assert!(matches!(
        database.commit_optimistic(child),
        Err(NativeRuntimeError::ForeignKeyConstraintViolation)
    ));
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
fn concurrent_child_commit_wins_and_parent_delete_rebase_fails()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = std::env::temp_dir().join(format!(
        "hyphae-native-fk-race-reverse-{}",
        std::process::id()
    ));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut seed = database.begin_sql(1, DurabilityClass::Strict)?;
    seed.execute_sql("CREATE TABLE parents (id BIGINT PRIMARY KEY)", &[])?;
    seed.execute_sql(
        "CREATE TABLE children (id BIGINT PRIMARY KEY, parent_id BIGINT, CONSTRAINT children_parent_fk FOREIGN KEY (parent_id) REFERENCES parents (id))",
        &[],
    )?;
    seed.execute_sql("INSERT INTO parents (id) VALUES (1)", &[])?;
    seed.commit()?;
    let mut child = database.begin_optimistic(2, DurabilityClass::Memory)?;
    child.execute_sql("INSERT INTO children (id, parent_id) VALUES (1, 1)", &[])?;
    let mut parent_delete = database.begin_optimistic(2, DurabilityClass::Memory)?;
    parent_delete.execute_sql("DELETE FROM parents WHERE id = 1", &[])?;
    database.commit_optimistic(child)?;
    assert!(matches!(
        database.commit_optimistic(parent_delete),
        Err(NativeRuntimeError::ForeignKeyConstraintViolation)
    ));
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}

#[test]
fn group_commit_rejects_second_fk_racer_in_both_orders() -> Result<(), Box<dyn std::error::Error>> {
    for child_first in [true, false] {
        let temporary = std::env::temp_dir().join(format!(
            "hyphae-native-fk-group-{child_first}-{}",
            std::process::id()
        ));
        let _ignored = std::fs::remove_dir_all(&temporary);
        let mut database = NativeDatabase::create(&temporary)?;
        let mut seed = database.begin_sql(1, DurabilityClass::Strict)?;
        seed.execute_sql("CREATE TABLE parents (id BIGINT PRIMARY KEY)", &[])?;
        seed.execute_sql(
            "CREATE TABLE children (id BIGINT PRIMARY KEY, parent_id BIGINT, CONSTRAINT children_parent_fk FOREIGN KEY (parent_id) REFERENCES parents (id))",
            &[],
        )?;
        seed.execute_sql("INSERT INTO parents (id) VALUES (1)", &[])?;
        seed.commit()?;
        let mut child = database.begin_optimistic(2, DurabilityClass::Group)?;
        child.execute_sql("INSERT INTO children (id, parent_id) VALUES (1, 1)", &[])?;
        let mut parent_delete = database.begin_optimistic(2, DurabilityClass::Group)?;
        parent_delete.execute_sql("DELETE FROM parents WHERE id = 1", &[])?;
        let batches = if child_first {
            vec![child, parent_delete]
        } else {
            vec![parent_delete, child]
        };
        let outcomes = database.commit_group(batches)?.outcomes;
        assert!(matches!(outcomes[0], GroupCommitOutcome::Committed(_)));
        assert!(matches!(
            outcomes[1],
            GroupCommitOutcome::Rejected(NativeRuntimeError::ForeignKeyConstraintViolation)
        ));
        std::fs::remove_dir_all(&temporary)?;
    }
    Ok(())
}
