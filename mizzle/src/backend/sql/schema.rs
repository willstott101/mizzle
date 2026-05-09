//! DDL and table creation for the SQL backend.

use anyhow::Result;
use sqlx::SqlitePool;

/// Create the schema tables if they do not already exist.
pub(super) async fn ensure_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS repositories (
            id    INTEGER PRIMARY KEY,
            path  TEXT NOT NULL UNIQUE,
            head  TEXT                       -- symref target, e.g. "refs/heads/main"
        );

        CREATE TABLE IF NOT EXISTS objects (
            repo_id  INTEGER NOT NULL REFERENCES repositories(id),
            oid      BLOB NOT NULL,          -- 20 bytes (SHA-1)
            kind     INTEGER NOT NULL,       -- 0=blob, 1=tree, 2=commit, 3=tag
            data     BLOB NOT NULL,
            PRIMARY KEY (repo_id, oid)
        );

        CREATE TABLE IF NOT EXISTS refs (
            repo_id  INTEGER NOT NULL REFERENCES repositories(id),
            name     TEXT NOT NULL,
            oid      BLOB NOT NULL,
            PRIMARY KEY (repo_id, name)
        );

        CREATE TABLE IF NOT EXISTS commit_parents (
            repo_id     INTEGER NOT NULL REFERENCES repositories(id),
            commit_oid  BLOB NOT NULL,
            parent_oid  BLOB NOT NULL,
            position    INTEGER NOT NULL,
            PRIMARY KEY (repo_id, commit_oid, position)
        );
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}
