use super::*;
use rusqlite::{OpenFlags, config::DbConfig};
use std::{
    fs::{self, OpenOptions, Permissions},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA_V5: &str = r#"
CREATE TABLE store_meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE operations (
    owner_digest   BLOB NOT NULL CHECK(length(owner_digest) = 32),
    operation_id   BLOB NOT NULL CHECK(length(operation_id) = 16),
    fingerprint    BLOB NOT NULL CHECK(length(fingerprint) = 32),
    lease_token    BLOB NOT NULL CHECK(length(lease_token) = 16),
    state          INTEGER NOT NULL CHECK(state IN (0, 1, 2)),
    terminal_state INTEGER CHECK(terminal_state IN (0, 1, 2)),
    http_status    INTEGER,
    error_code     TEXT,
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL,
    expires_at_ms  INTEGER,
    PRIMARY KEY(owner_digest, operation_id),
    CHECK(error_code IS NULL OR length(error_code) BETWEEN 1 AND 128),
    CHECK(
        (state IN (0, 1)
         AND terminal_state IS NULL
         AND http_status IS NULL
         AND error_code IS NULL
         AND expires_at_ms IS NULL)
        OR
        (state = 2
         AND terminal_state IS NOT NULL
         AND http_status BETWEEN 100 AND 599
         AND expires_at_ms IS NOT NULL
         AND (
             (terminal_state = 0
              AND http_status BETWEEN 200 AND 299
              AND error_code IS NULL)
             OR
             (terminal_state IN (1, 2)
              AND NOT (http_status BETWEEN 200 AND 299)
              AND error_code IS NOT NULL)
         ))
    )
) STRICT, WITHOUT ROWID;

CREATE INDEX operations_expiry
ON operations(expires_at_ms) WHERE state = 2;

CREATE TABLE upload_sessions (
    owner_digest    BLOB NOT NULL CHECK(length(owner_digest) = 32),
    upload_id       BLOB NOT NULL CHECK(length(upload_id) = 16),
    target_path     BLOB NOT NULL CHECK(length(target_path) BETWEEN 1 AND 65536),
    stage_path      BLOB NOT NULL CHECK(length(stage_path) BETWEEN 1 AND 65536),
    upload_length   INTEGER NOT NULL CHECK(upload_length >= 0),
    durable_offset  INTEGER NOT NULL CHECK(durable_offset >= 0),
    state           INTEGER NOT NULL CHECK(state IN (0, 1, 2, 3, 4, 5)),
    stage_device_be BLOB CHECK(stage_device_be IS NULL OR length(stage_device_be) = 8),
    stage_inode_be  BLOB CHECK(stage_inode_be IS NULL OR length(stage_inode_be) = 8),
    target_revision BLOB CHECK(target_revision IS NULL OR length(target_revision) = 32),
    updated_at_ms   INTEGER NOT NULL,
    expires_at_ms   INTEGER NOT NULL,
    PRIMARY KEY(owner_digest, upload_id),
    CHECK(target_path != stage_path),
    CHECK(durable_offset <= upload_length),
    CHECK((stage_device_be IS NULL) = (stage_inode_be IS NULL)),
    CHECK(state != 2 OR durable_offset = upload_length),
    CHECK(state != 1 OR durable_offset = upload_length),
    CHECK(state != 5 OR durable_offset = upload_length)
) STRICT, WITHOUT ROWID;

CREATE INDEX upload_sessions_expiry
ON upload_sessions(expires_at_ms);

CREATE TABLE purge_jobs (
    owner_digest      BLOB NOT NULL CHECK(length(owner_digest) = 32),
    job_id            BLOB NOT NULL CHECK(length(job_id) = 16),
    target_path       BLOB NOT NULL CHECK(length(target_path) BETWEEN 1 AND 65536),
    trash_path        BLOB NOT NULL UNIQUE CHECK(length(trash_path) BETWEEN 1 AND 65536),
    source_device_be  BLOB NOT NULL CHECK(length(source_device_be) = 8),
    source_inode_be   BLOB NOT NULL CHECK(length(source_inode_be) = 8),
    trash_revision    BLOB CHECK(trash_revision IS NULL OR length(trash_revision) = 32),
    is_directory      INTEGER NOT NULL CHECK(is_directory IN (0, 1)),
    state             INTEGER NOT NULL CHECK(state IN (0, 1, 2)),
    attempts          INTEGER NOT NULL CHECK(attempts BETWEEN 0 AND 4294967295),
    next_attempt_at_ms INTEGER NOT NULL,
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL,
    PRIMARY KEY(owner_digest, job_id),
    CHECK(target_path != trash_path)
) STRICT, WITHOUT ROWID;

CREATE INDEX purge_jobs_due
ON purge_jobs(state, next_attempt_at_ms, created_at_ms)
WHERE state = 1;

CREATE INDEX purge_jobs_prepared
ON purge_jobs(created_at_ms)
WHERE state = 0;
"#;

const MIGRATE_V2_UPLOADS_TO_V3: &str = r#"
CREATE TABLE upload_sessions_v3 (
    owner_digest    BLOB NOT NULL CHECK(length(owner_digest) = 32),
    upload_id       BLOB NOT NULL CHECK(length(upload_id) = 16),
    target_path     BLOB NOT NULL CHECK(length(target_path) BETWEEN 1 AND 65536),
    stage_path      BLOB NOT NULL CHECK(length(stage_path) BETWEEN 1 AND 65536),
    upload_length   INTEGER NOT NULL CHECK(upload_length >= 0),
    durable_offset  INTEGER NOT NULL CHECK(durable_offset >= 0),
    state           INTEGER NOT NULL CHECK(state IN (0, 1, 2, 3, 4, 5)),
    stage_device_be BLOB CHECK(stage_device_be IS NULL OR length(stage_device_be) = 8),
    stage_inode_be  BLOB CHECK(stage_inode_be IS NULL OR length(stage_inode_be) = 8),
    target_revision BLOB CHECK(target_revision IS NULL OR length(target_revision) = 32),
    updated_at_ms   INTEGER NOT NULL,
    expires_at_ms   INTEGER NOT NULL,
    PRIMARY KEY(owner_digest, upload_id),
    CHECK(target_path != stage_path),
    CHECK(durable_offset <= upload_length),
    CHECK((stage_device_be IS NULL) = (stage_inode_be IS NULL)),
    CHECK(state != 2 OR durable_offset = upload_length),
    CHECK(state != 1 OR durable_offset = upload_length),
    CHECK(state != 5 OR durable_offset = upload_length)
) STRICT, WITHOUT ROWID;

INSERT INTO upload_sessions_v3(
    owner_digest, upload_id, target_path, stage_path, upload_length,
    durable_offset, state, stage_device_be, stage_inode_be,
    target_revision, updated_at_ms, expires_at_ms
)
SELECT owner_digest, upload_id, target_path, stage_path, upload_length,
       durable_offset, state, stage_device_be, stage_inode_be,
       NULL, updated_at_ms, expires_at_ms
  FROM upload_sessions;

DROP TABLE upload_sessions;
ALTER TABLE upload_sessions_v3 RENAME TO upload_sessions;
CREATE INDEX upload_sessions_expiry ON upload_sessions(expires_at_ms);
"#;

const MIGRATE_V3_PURGES_TO_V4: &str = r#"
ALTER TABLE purge_jobs ADD COLUMN trash_revision BLOB
    CHECK(trash_revision IS NULL OR length(trash_revision) = 32);
"#;

// Build the one historical shape that cannot be obtained from v5 with a
// column drop. This is used only to construct the trusted schema snapshot for
// a v2 migration input; production data is migrated by the statements above.
const BUILD_EXPECTED_V2_FROM_V5: &str = r#"
DROP INDEX upload_sessions_expiry;
DROP TABLE upload_sessions;
CREATE TABLE upload_sessions (
    owner_digest    BLOB NOT NULL CHECK(length(owner_digest) = 32),
    upload_id       BLOB NOT NULL CHECK(length(upload_id) = 16),
    target_path     BLOB NOT NULL CHECK(length(target_path) BETWEEN 1 AND 65536),
    stage_path      BLOB NOT NULL CHECK(length(stage_path) BETWEEN 1 AND 65536),
    upload_length   INTEGER NOT NULL CHECK(upload_length >= 0),
    durable_offset  INTEGER NOT NULL CHECK(durable_offset >= 0),
    state           INTEGER NOT NULL CHECK(state IN (0, 1, 2, 3, 4)),
    stage_device_be BLOB CHECK(stage_device_be IS NULL OR length(stage_device_be) = 8),
    stage_inode_be  BLOB CHECK(stage_inode_be IS NULL OR length(stage_inode_be) = 8),
    updated_at_ms   INTEGER NOT NULL,
    expires_at_ms   INTEGER NOT NULL,
    PRIMARY KEY(owner_digest, upload_id),
    CHECK(target_path != stage_path),
    CHECK(durable_offset <= upload_length),
    CHECK((stage_device_be IS NULL) = (stage_inode_be IS NULL)),
    CHECK(state != 2 OR durable_offset = upload_length),
    CHECK(state != 1 OR durable_offset = upload_length)
) STRICT, WITHOUT ROWID;
CREATE INDEX upload_sessions_expiry ON upload_sessions(expires_at_ms);
ALTER TABLE purge_jobs DROP COLUMN trash_revision;
"#;

#[derive(Eq, PartialEq)]
struct SchemaSnapshot {
    objects: Vec<SchemaObjectSnapshot>,
    tables: Vec<TableSchemaSnapshot>,
}

#[derive(Eq, PartialEq)]
struct SchemaObjectSnapshot {
    object_type: String,
    name: String,
    table_name: String,
    sql: Option<String>,
}

#[derive(Eq, PartialEq)]
struct TableSchemaSnapshot {
    name: String,
    column_count: i64,
    without_rowid: i64,
    strict: i64,
    columns: Vec<ColumnSchemaSnapshot>,
    indexes: Vec<IndexSchemaSnapshot>,
    foreign_key_count: i64,
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct ColumnSchemaSnapshot {
    name: String,
    declared_type: String,
    not_null: i64,
    default_value: Option<String>,
    primary_key_position: i64,
    hidden: i64,
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct IndexSchemaSnapshot {
    name: String,
    unique: i64,
    origin: String,
    partial: i64,
    key_columns: Vec<IndexColumnSchemaSnapshot>,
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct IndexColumnSchemaSnapshot {
    position: i64,
    name: Option<String>,
    descending: i64,
    collation: Option<String>,
}

pub(super) fn prepare_database_file(path: &Path, root: &RootIdentity) -> Result<()> {
    ensure!(
        path.file_name().is_some(),
        "State database path must name a file"
    );
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent_metadata = fs::metadata(parent).with_context(|| {
        format!(
            "Failed to inspect state database directory `{}`",
            parent.display()
        )
    })?;
    ensure!(
        parent_metadata.is_dir(),
        "State database parent `{}` is not a directory",
        parent.display()
    );

    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => {
            file.set_permissions(Permissions::from_mode(0o600))
                .with_context(|| {
                    format!(
                        "Failed to set state database permissions on `{}`",
                        path.display()
                    )
                })?;
            file.sync_all().with_context(|| {
                format!(
                    "Failed to synchronize new state database `{}`",
                    path.display()
                )
            })?;
            fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .with_context(|| {
                    format!(
                        "Failed to synchronize state database directory `{}`",
                        parent.display()
                    )
                })?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path).with_context(|| {
                format!("Failed to inspect state database `{}`", path.display())
            })?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "State database `{}` cannot be a symbolic link",
                path.display()
            );
            ensure!(
                metadata.is_file(),
                "State database `{}` is not a regular file",
                path.display()
            );
            ensure!(
                metadata.nlink() == 1,
                "State database `{}` cannot have multiple hard links",
                path.display()
            );

            // Validate before changing permissions or any persistent SQLite
            // setting so an unrelated file is never silently adopted.
            preflight_existing_database(path, *root)?;
            fs::set_permissions(path, Permissions::from_mode(0o600)).with_context(|| {
                format!(
                    "Failed to set state database permissions on `{}`",
                    path.display()
                )
            })?;
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Failed to create state database `{}`", path.display()));
        }
    }
    Ok(())
}

pub(super) fn open_initialized_connection(
    path: &Path,
    root: RootIdentity,
    operation_ttl_ms: i64,
    upload_ttl_ms: i64,
    recovery_now_ms: i64,
) -> Result<Connection> {
    let mut connection = open_connection(path)?;
    initialize_database(
        &mut connection,
        root,
        operation_ttl_ms,
        upload_ttl_ms,
        recovery_now_ms,
    )?;
    Ok(connection)
}

fn open_connection(path: &Path) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_EXRESCODE;
    let connection = Connection::open_with_flags(path, flags)
        .with_context(|| format!("Failed to open state database `{}`", path.display()))?;
    harden_connection(&connection)?;
    Ok(connection)
}

fn open_existing_database_read_only(path: &Path) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_EXRESCODE;
    let connection = Connection::open_with_flags(path, flags).with_context(|| {
        format!(
            "Failed to inspect existing state database `{}`",
            path.display()
        )
    })?;
    harden_connection(&connection)?;
    Ok(connection)
}

fn harden_connection(connection: &Connection) -> Result<()> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    ensure!(
        connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?,
        "SQLite defensive mode could not be enabled"
    );
    ensure!(
        !connection.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?,
        "SQLite trusted schema mode could not be disabled"
    );
    connection.pragma_update(None, "trusted_schema", false)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "mmap_size", 0_i64)?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(())
}

fn configure_validated_connection(connection: &Connection) -> Result<()> {
    let mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))?;
    ensure!(
        mode.eq_ignore_ascii_case("delete"),
        "SQLite refused rollback DELETE journal mode and returned `{mode}`"
    );
    connection.pragma_update(None, "synchronous", "EXTRA")?;

    let synchronous: i64 = connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
    let foreign_keys: i64 =
        connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    let trusted_schema: i64 =
        connection.pragma_query_value(None, "trusted_schema", |row| row.get(0))?;
    let mmap_size: i64 = connection.query_row("PRAGMA mmap_size", [], |row| row.get(0))?;
    ensure!(synchronous == 3, "SQLite synchronous mode is not EXTRA");
    ensure!(
        foreign_keys == 1,
        "SQLite foreign key enforcement is disabled"
    );
    ensure!(trusted_schema == 0, "SQLite trusted schema mode is enabled");
    ensure!(mmap_size == 0, "SQLite memory-mapped I/O is enabled");
    Ok(())
}

fn initialize_database(
    connection: &mut Connection,
    root: RootIdentity,
    operation_ttl_ms: i64,
    upload_ttl_ms: i64,
    recovery_now_ms: i64,
) -> Result<()> {
    // Repeat the read-only path preflight on the actor connection before the
    // first persistent pragma, schema mutation, or recovery write.
    validate_database_before_mutation(connection, root)?;
    configure_validated_connection(connection)?;
    initialize_schema(connection, root)?;
    recover_database(connection, operation_ttl_ms, upload_ttl_ms, recovery_now_ms)
}

fn preflight_existing_database(path: &Path, root: RootIdentity) -> Result<()> {
    let connection = open_existing_database_read_only(path)?;
    validate_database_before_mutation(&connection, root).with_context(|| {
        format!(
            "Refusing to modify unrecognized state database `{}`",
            path.display()
        )
    })
}

fn validate_exact_schema(connection: &Connection, version: i32) -> Result<()> {
    let actual = inspect_schema(connection)
        .with_context(|| format!("Failed to inspect state database schema version {version}"))?;
    let expected = expected_schema(version).with_context(|| {
        format!("Failed to construct the trusted schema version {version} definition")
    })?;
    ensure!(
        actual == expected,
        "State database schema version {version} does not exactly match the supported DUFS schema (tables, columns, constraints, indexes, triggers, or views differ)"
    );
    Ok(())
}

fn expected_schema(version: i32) -> Result<SchemaSnapshot> {
    let connection = Connection::open_in_memory()?;
    connection.execute_batch(SCHEMA_V5)?;
    match version {
        2 => connection.execute_batch(BUILD_EXPECTED_V2_FROM_V5)?,
        3 => connection.execute_batch("ALTER TABLE purge_jobs DROP COLUMN trash_revision;")?,
        4 | SCHEMA_VERSION => {}
        _ => bail!("Schema version {version} has no trusted definition"),
    }
    inspect_schema(&connection)
}

fn inspect_schema(connection: &Connection) -> Result<SchemaSnapshot> {
    let mut statement = connection.prepare(
        "SELECT type, name, tbl_name, sql
           FROM sqlite_schema
          WHERE type IN ('table', 'index', 'trigger', 'view')
            AND name NOT GLOB 'sqlite_*'
          ORDER BY type, name",
    )?;
    let objects = statement
        .query_map([], |row| {
            let sql: Option<String> = row.get(3)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                sql,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(object_type, name, table_name, sql)| {
            Ok(SchemaObjectSnapshot {
                object_type,
                name,
                table_name,
                sql: sql.map(|sql| canonical_schema_sql(&sql)).transpose()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut table_names = objects
        .iter()
        .filter(|object| object.object_type == "table")
        .map(|object| object.name.clone())
        .collect::<Vec<_>>();
    table_names.sort_unstable();
    let tables = table_names
        .into_iter()
        .map(|name| inspect_table_schema(connection, name))
        .collect::<Result<Vec<_>>>()?;

    Ok(SchemaSnapshot { objects, tables })
}

fn inspect_table_schema(connection: &Connection, name: String) -> Result<TableSchemaSnapshot> {
    let (column_count, without_rowid, strict) = connection.query_row(
        "SELECT ncol, wr, strict
           FROM pragma_table_list
          WHERE schema = 'main' AND name = ?1 AND type = 'table'",
        [&name],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;

    let mut column_statement = connection.prepare(
        "SELECT name, type, \"notnull\", dflt_value, pk, hidden
           FROM pragma_table_xinfo(?1)",
    )?;
    let mut columns = column_statement
        .query_map([&name], |row| {
            let default_value: Option<String> = row.get(3)?;
            Ok(ColumnSchemaSnapshot {
                name: row.get(0)?,
                declared_type: row.get::<_, String>(1)?.to_ascii_uppercase(),
                not_null: row.get(2)?,
                default_value,
                primary_key_position: row.get(4)?,
                hidden: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for column in &mut columns {
        column.default_value = column
            .default_value
            .take()
            .map(|value| canonical_schema_sql(&value))
            .transpose()?;
    }
    columns.sort_unstable();

    let mut index_statement = connection.prepare(
        "SELECT name, \"unique\", origin, partial
           FROM pragma_index_list(?1)",
    )?;
    let index_rows = index_statement
        .query_map([&name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut indexes = index_rows
        .into_iter()
        .map(|(index_name, unique, origin, partial)| {
            let mut key_statement = connection.prepare(
                "SELECT seqno, name, \"desc\", coll
                   FROM pragma_index_xinfo(?1)
                  WHERE key = 1
                  ORDER BY seqno",
            )?;
            let key_columns = key_statement
                .query_map([&index_name], |row| {
                    Ok(IndexColumnSchemaSnapshot {
                        position: row.get(0)?,
                        name: row.get(1)?,
                        descending: row.get(2)?,
                        collation: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let snapshot_name = if index_name.starts_with("sqlite_autoindex_") {
                // SQLite may retain the generated name's pre-rename table
                // component after the v2 upload-table migration. Its origin
                // and ordered key contract are the stable schema identity.
                format!("sqlite_autoindex:{origin}")
            } else {
                index_name
            };
            Ok(IndexSchemaSnapshot {
                name: snapshot_name,
                unique,
                origin,
                partial,
                key_columns,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    indexes.sort_unstable();

    let foreign_key_count = connection.query_row(
        "SELECT COUNT(*) FROM pragma_foreign_key_list(?1)",
        [&name],
        |row| row.get(0),
    )?;

    Ok(TableSchemaSnapshot {
        name,
        column_count,
        without_rowid,
        strict,
        columns,
        indexes,
        foreign_key_count,
    })
}

fn canonical_schema_sql(sql: &str) -> Result<String> {
    let mut compact = String::with_capacity(sql.len());
    let mut characters = sql.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            character if character.is_ascii_whitespace() => {}
            '\'' => {
                compact.push(character);
                let mut terminated = false;
                while let Some(quoted) = characters.next() {
                    compact.push(quoted);
                    if quoted == '\'' {
                        if characters.peek() == Some(&'\'') {
                            if let Some(escaped) = characters.next() {
                                compact.push(escaped);
                            }
                        } else {
                            terminated = true;
                            break;
                        }
                    }
                }
                ensure!(
                    terminated,
                    "SQLite schema contains an unterminated string literal"
                );
            }
            '"' => {
                let mut terminated = false;
                while let Some(quoted) = characters.next() {
                    if quoted == '"' {
                        if characters.peek() == Some(&'"') {
                            compact.push('"');
                            characters.next();
                        } else {
                            terminated = true;
                            break;
                        }
                    } else {
                        compact.push(quoted.to_ascii_lowercase());
                    }
                }
                ensure!(
                    terminated,
                    "SQLite schema contains an unterminated identifier"
                );
            }
            _ => compact.push(character.to_ascii_lowercase()),
        }
    }
    while compact.ends_with(';') {
        compact.pop();
    }
    if compact.starts_with("createtable") {
        canonical_create_table_sql(compact)
    } else {
        Ok(compact)
    }
}

fn canonical_create_table_sql(compact: String) -> Result<String> {
    ensure!(
        compact.is_ascii(),
        "SQLite project table definitions must use ASCII syntax"
    );
    let bytes = compact.as_bytes();
    let open = bytes
        .iter()
        .position(|byte| *byte == b'(')
        .ok_or_else(|| anyhow!("SQLite table definition is missing its column list"))?;
    let mut depth = 0_u32;
    let mut in_string = false;
    let mut close = None;
    let mut index = open;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' => {
                if in_string && bytes.get(index + 1) == Some(&b'\'') {
                    index += 1;
                } else {
                    in_string = !in_string;
                }
            }
            b'(' if !in_string => depth += 1,
            b')' if !in_string => {
                ensure!(
                    depth > 0,
                    "SQLite table definition has unbalanced parentheses"
                );
                depth -= 1;
                if depth == 0 {
                    close = Some(index);
                    break;
                }
            }
            _ => {}
        }
        index += 1;
    }
    ensure!(
        !in_string,
        "SQLite table definition has an unterminated string"
    );
    let close =
        close.ok_or_else(|| anyhow!("SQLite table definition has unbalanced parentheses"))?;

    let body = &compact[open + 1..close];
    let mut clauses = Vec::new();
    let mut clause_start = 0;
    depth = 0;
    in_string = false;
    let body_bytes = body.as_bytes();
    let mut body_index = 0;
    while body_index < body_bytes.len() {
        match body_bytes[body_index] {
            b'\'' => {
                if in_string && body_bytes.get(body_index + 1) == Some(&b'\'') {
                    body_index += 1;
                } else {
                    in_string = !in_string;
                }
            }
            b'(' if !in_string => depth += 1,
            b')' if !in_string => {
                ensure!(depth > 0, "SQLite table clause has unbalanced parentheses");
                depth -= 1;
            }
            b',' if !in_string && depth == 0 => {
                clauses.push(&body[clause_start..body_index]);
                clause_start = body_index + 1;
            }
            _ => {}
        }
        body_index += 1;
    }
    ensure!(
        !in_string && depth == 0,
        "SQLite table clause has invalid quoting or parentheses"
    );
    clauses.push(&body[clause_start..]);
    ensure!(
        clauses.iter().all(|clause| !clause.is_empty()),
        "SQLite table definition contains an empty clause"
    );
    clauses.sort_unstable();

    Ok(format!(
        "{}({}){}",
        &compact[..open],
        clauses.join(","),
        &compact[close + 1..]
    ))
}

fn validate_database_before_mutation(connection: &Connection, root: RootIdentity) -> Result<()> {
    let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let application_id: i32 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;

    match version {
        0 => {
            ensure!(
                application_id == 0,
                "State database schema version 0 is unsupported; this release requires schema version {SCHEMA_VERSION}"
            );
            let object_count: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_schema \
                 WHERE type IN ('table', 'index', 'trigger', 'view') \
                   AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            ensure!(
                object_count == 0,
                "Refusing to initialize a non-empty database without a DUFS schema version"
            );
        }
        2 | 3 | 4 | SCHEMA_VERSION => {
            ensure!(
                application_id == APPLICATION_ID,
                "The state database application id is invalid"
            );
            validate_exact_schema(connection, version)?;
            verify_root_identity(connection, root)?;
        }
        version => {
            ensure!(
                application_id == APPLICATION_ID,
                "The configured database application id belongs to another application"
            );
            bail!(
                "State database schema version {version} is unsupported; this release requires schema version {SCHEMA_VERSION} and supports migration only from schema versions 2, 3, and 4"
            );
        }
    }

    let quick_check: String = connection
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .context("Failed to check state database integrity")?;
    ensure!(
        quick_check.eq_ignore_ascii_case("ok"),
        "State database integrity check failed: {quick_check}"
    );
    Ok(())
}

fn initialize_schema(connection: &mut Connection, root: RootIdentity) -> Result<()> {
    let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let application_id: i32 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;

    match version {
        0 => {
            ensure!(
                application_id == 0,
                "State database schema version 0 is unsupported; this release requires schema version {SCHEMA_VERSION}"
            );
            let object_count: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_schema \
                 WHERE type IN ('table', 'index', 'trigger', 'view') \
                   AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            ensure!(
                object_count == 0,
                "Refusing to initialize a non-empty database without a DUFS schema version"
            );

            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(SCHEMA_V5)?;
            transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            insert_root_identity(&transaction, root)?;
            validate_exact_schema(&transaction, SCHEMA_VERSION)?;
            transaction.commit()?;
        }
        2 => {
            ensure!(
                application_id == APPLICATION_ID,
                "The state database application id is invalid"
            );
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            validate_exact_schema(&transaction, 2)?;
            transaction.execute_batch(MIGRATE_V2_UPLOADS_TO_V3)?;
            transaction.execute_batch(MIGRATE_V3_PURGES_TO_V4)?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            validate_exact_schema(&transaction, SCHEMA_VERSION)?;
            transaction.commit()?;
        }
        3 => {
            ensure!(
                application_id == APPLICATION_ID,
                "The state database application id is invalid"
            );
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            validate_exact_schema(&transaction, 3)?;
            transaction.execute_batch(MIGRATE_V3_PURGES_TO_V4)?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            validate_exact_schema(&transaction, SCHEMA_VERSION)?;
            transaction.commit()?;
        }
        4 => {
            ensure!(
                application_id == APPLICATION_ID,
                "The state database application id is invalid"
            );
            // Schema v5 changes the durable meaning of upload stage paths:
            // the listener-start reconciliation moves v4 stages into their
            // private directory. Bump first so an older binary can never open
            // a database after any part of that filesystem migration.
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            validate_exact_schema(&transaction, 4)?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            validate_exact_schema(&transaction, SCHEMA_VERSION)?;
            transaction.commit()?;
        }
        SCHEMA_VERSION => {
            ensure!(
                application_id == APPLICATION_ID,
                "The state database application id is invalid"
            );
            validate_exact_schema(connection, SCHEMA_VERSION)?;
        }
        version => {
            ensure!(
                application_id == APPLICATION_ID,
                "The configured database application id belongs to another application"
            );
            bail!(
                "State database schema version {version} is unsupported; this release requires schema version {SCHEMA_VERSION} and supports migration only from schema versions 2, 3, and 4"
            );
        }
    }

    verify_root_identity(connection, root)?;
    Ok(())
}

fn insert_root_identity(transaction: &Transaction<'_>, root: RootIdentity) -> Result<()> {
    transaction.execute(
        "INSERT INTO store_meta(key, value) VALUES \
         ('root-device-be', ?1), ('root-inode-be', ?2)",
        params![
            root.device.to_be_bytes().as_slice(),
            root.inode.to_be_bytes().as_slice()
        ],
    )?;
    Ok(())
}

fn verify_root_identity(connection: &Connection, expected: RootIdentity) -> Result<()> {
    let device = load_meta(connection, "root-device-be")?;
    let inode = load_meta(connection, "root-inode-be")?;
    ensure!(
        device.as_slice() == expected.device.to_be_bytes(),
        "The state database is bound to a different shared root device"
    );
    ensure!(
        inode.as_slice() == expected.inode.to_be_bytes(),
        "The state database is bound to a different shared root inode"
    );
    Ok(())
}

fn load_meta(connection: &Connection, key: &str) -> Result<Vec<u8>> {
    connection
        .query_row(
            "SELECT value FROM store_meta WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("State database metadata `{key}` is missing"))
}

fn recover_database(
    connection: &mut Connection,
    operation_ttl_ms: i64,
    upload_ttl_ms: i64,
    now: i64,
) -> Result<()> {
    let operation_expires_at = expiration_time(now, operation_ttl_ms)?;
    let upload_expires_at = expiration_time(now, upload_ttl_ms)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    super::operation::purge_expired(&transaction, now)?;
    transaction.execute(
        "DELETE FROM operations WHERE state = ?1",
        [OPERATION_RESERVED],
    )?;
    transaction.execute(
        "UPDATE operations
            SET state = ?1,
                terminal_state = ?2,
                http_status = ?3,
                error_code = ?4,
                updated_at_ms = ?5,
                expires_at_ms = ?6
          WHERE state = ?7",
        params![
            OPERATION_COMPLETED,
            StoredTerminalState::Unknown as i64,
            i64::from(UNKNOWN_STATUS),
            UNKNOWN_CODE,
            now,
            operation_expires_at,
            OPERATION_COMMIT_STARTED,
        ],
    )?;
    transaction.execute(
        "UPDATE operations SET expires_at_ms = ?1
          WHERE state = ?2 AND expires_at_ms > ?1",
        params![operation_expires_at, OPERATION_COMPLETED],
    )?;
    transaction.execute(
        "UPDATE upload_sessions
            SET state = ?1,
                updated_at_ms = ?2,
                expires_at_ms = ?3
          WHERE state = ?4",
        params![
            UPLOAD_UNKNOWN,
            now,
            upload_expires_at,
            UPLOAD_COMMIT_STARTED
        ],
    )?;
    transaction.execute(
        "UPDATE upload_sessions SET expires_at_ms = ?1
          WHERE expires_at_ms > ?1",
        [upload_expires_at],
    )?;
    transaction.execute(
        "UPDATE purge_jobs
            SET state = ?1,
                next_attempt_at_ms = ?2,
                updated_at_ms = ?2
          WHERE state = ?3",
        params![PURGE_READY, now, PURGE_CLAIMED],
    )?;
    transaction.execute(
        "UPDATE purge_jobs
            SET next_attempt_at_ms = ?1,
                updated_at_ms = ?1
          WHERE state = ?2 AND next_attempt_at_ms > ?1",
        params![now, PURGE_READY],
    )?;
    transaction.commit()?;
    Ok(())
}
