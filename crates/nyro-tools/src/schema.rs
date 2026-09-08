//! Reference DDL comes from the storage bootstrap, never from committed SQL files.
//! The caller owns a disposable, exclusively used database; we never create, clear,
//! or drop a database, and never construct a Gateway or start its background work.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Args, ValueEnum};
use nyro_core::storage::sql::config::SqlBackendConfig;
use nyro_core::storage::{MysqlStorage, PostgresStorage, Storage};
use sqlx::{MySqlPool, PgPool, Row};
use tokio::process::Command;
use url::Url;

#[derive(Args)]
pub struct DumpSchemaArgs {
    /// Storage backend to bootstrap and inspect
    #[arg(long, value_enum)]
    backend: SchemaBackend,
    /// Explicit TCP URL of an EMPTY, dedicated disposable database (never an app database)
    #[arg(long, env = "NYRO_SCHEMA_DATABASE_URL", hide_env_values = true)]
    scratch_db_url: Option<String>,
    /// Atomically replace this file after success; otherwise emit the complete SQL on stdout
    #[arg(long, value_name = "PATH")]
    output: Option<PathBuf>,
}

#[derive(ValueEnum, Clone, Copy)]
enum SchemaBackend {
    Postgres,
    Mysql,
}

pub async fn run(args: DumpSchemaArgs) -> Result<()> {
    let raw_url = args.scratch_db_url.as_deref().context(
        "dump-schema requires --scratch-db-url or NYRO_SCHEMA_DATABASE_URL pointing to an EMPTY dedicated disposable database; application database settings are never used",
    )?;
    let url = scratch_url(args.backend, raw_url)?;
    let sql = match args.backend {
        SchemaBackend::Postgres => postgres_schema(&url).await?,
        SchemaBackend::Mysql => mysql_schema(&url).await?,
    };
    // Do not open/truncate a destination, or emit any SQL, until every step succeeds.
    if let Some(path) = args.output {
        write_atomic(&path, &sql)?;
    } else {
        std::io::stdout()
            .lock()
            .write_all(sql.as_bytes())
            .context("write completed schema to stdout")?;
    }
    Ok(())
}

fn scratch_url(backend: SchemaBackend, raw: &str) -> Result<Url> {
    // Do not propagate URL parser errors: they may contain credentials.
    let mut url = Url::parse(raw).map_err(|_| anyhow!("invalid scratch database URL"))?;
    let (valid_scheme, port) = match backend {
        SchemaBackend::Postgres => (matches!(url.scheme(), "postgres" | "postgresql"), 5432),
        SchemaBackend::Mysql => (url.scheme() == "mysql", 3306),
    };
    ensure!(valid_scheme, "scratch URL scheme does not match --backend");
    ensure!(
        url.host_str().is_some() && !url.username().is_empty(),
        "scratch URL must explicitly specify a TCP host and user"
    );
    // A deliberately narrow database-name grammar avoids libpq/SQLx parsing
    // differences and prevents fallback to PGDATABASE or a user's default DB.
    let database = url.path().strip_prefix('/').unwrap_or_default();
    ensure!(
        !database.is_empty()
            && database
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "scratch URL must specify a database name containing only letters, digits, underscores or hyphens"
    );
    ensure!(
        !matches!(
            database.to_ascii_lowercase().as_str(),
            "postgres"
                | "template0"
                | "template1"
                | "mysql"
                | "sys"
                | "information_schema"
                | "performance_schema"
        ),
        "system/default databases are not dedicated scratch databases"
    );
    ensure!(
        url.fragment().is_none(),
        "scratch URL must not contain a fragment"
    );
    for (key, _) in url.query_pairs() {
        let allowed = match backend {
            SchemaBackend::Postgres => matches!(
                key.as_ref(),
                "sslmode" | "sslrootcert" | "sslcert" | "sslkey"
            ),
            SchemaBackend::Mysql => {
                matches!(key.as_ref(), "ssl-mode" | "ssl-ca" | "ssl-cert" | "ssl-key")
            }
        };
        ensure!(
            allowed,
            "scratch URL supports only backend TLS query parameters; connection overrides are not allowed"
        );
    }
    // SQLx otherwise inherits PGPORT when the URL omits it.
    if url.port().is_none() {
        url.set_port(Some(port))
            .map_err(|_| anyhow!("invalid scratch database port"))?;
    }
    Ok(url)
}

fn config(url: &Url) -> SqlBackendConfig {
    SqlBackendConfig {
        max_connections: 1,
        min_connections: 1,
        ..SqlBackendConfig::with_url(url.as_str())
    }
}

// Storage connect errors include the full URL; server errors may echo arbitrary
// values. Keep the operation and SQLSTATE/error code, not the raw error chain.
fn database_error(operation: &str, error: impl Into<anyhow::Error>) -> anyhow::Error {
    let error = error.into();
    let code = error
        .downcast_ref::<sqlx::Error>()
        .and_then(|error| match error {
            sqlx::Error::Database(error) => error.code(),
            _ => None,
        });
    let code = code.filter(|code| code.bytes().all(|b| b.is_ascii_alphanumeric()));
    anyhow!(
        "{operation} failed{}; verify scratch database permissions/connectivity and storage migrations (connection details suppressed)",
        code.map(|code| format!(" (database code {code})"))
            .unwrap_or_default()
    )
}

async fn bootstrap(storage: &impl Storage) -> Result<()> {
    storage
        .bootstrap()
        .init()
        .await
        .map_err(|e| database_error("scratch storage init", e))?;
    storage
        .bootstrap()
        .migrate()
        .await
        .map_err(|e| database_error("scratch storage migration", e))?;
    Ok(())
}

async fn postgres_schema(url: &Url) -> Result<String> {
    let version = Command::new("pg_dump")
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .await
        .context("pg_dump must be installed on PATH (same major version as the scratch PostgreSQL server recommended)")?;
    ensure!(version.status.success(), "pg_dump --version failed");
    let storage = PostgresStorage::connect(config(url))
        .await
        .map_err(|e| database_error("PostgreSQL scratch connection", e))?;
    let result = async {
        // All migrations use unqualified names; ignore inherited PGOPTIONS search_path.
        sqlx::query("SET search_path TO public").execute(storage.pool()).await
            .map_err(|e| database_error("PostgreSQL scratch search path", e))?;
        ensure_postgres_empty(storage.pool()).await?;
        bootstrap(&storage).await?;
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT tablename::text FROM pg_catalog.pg_tables WHERE schemaname = 'public' ORDER BY tablename",
        ).fetch_all(storage.pool()).await.map_err(|e| database_error("PostgreSQL table inspection", e))?;
        ensure_final_tables(&tables)?;
        let mut command = Command::new("pg_dump");
        // PGDATABASE does NOT expand URLs. Pass an explicit password-free URL
        // via --dbname, and the decoded password via the child environment only.
        // Never relay pg_dump's stderr (which can contain credentials).
        for key in ["PGHOST", "PGHOSTADDR", "PGPORT", "PGUSER", "PGDATABASE", "PGOPTIONS", "PGSERVICE", "PGSERVICEFILE"] {
            command.env_remove(key);
        }
        let (connection, password) = postgres_dump_connection(url)?;
        if let Some(password) = password {
            command.env("PGPASSWORD", password);
        }
        let output = command
            .arg("--dbname").arg(connection.as_str())
            .env("PGCONNECT_TIMEOUT", "10")
            .env("LC_ALL", "C")
            .args(["--schema-only", "--no-owner", "--no-privileges", "--no-password"])
            .stdin(Stdio::null())
            .output().await.context("could not execute pg_dump")?;
        ensure!(output.status.success(), "pg_dump failed ({}); check pg_dump/server version compatibility and scratch credentials; stderr suppressed", output.status);
        let sql = String::from_utf8(output.stdout).context("pg_dump returned non-UTF-8 SQL")?;
        ensure!(sql.contains("CREATE TABLE public.models"), "pg_dump did not return the final models table");
        Ok(format!("{}{}", header("PostgreSQL", "postgres"), normalize_postgres(&sql)))
    }.await;
    storage.pool().close().await;
    result
}

fn postgres_dump_connection(url: &Url) -> Result<(Url, Option<String>)> {
    let password = url
        .password()
        .map(|password| {
            percent_encoding::percent_decode_str(password)
                .decode_utf8()
                .map(|password| password.into_owned())
                .map_err(|_| anyhow!("scratch database password must be valid UTF-8"))
        })
        .transpose()?;
    let mut connection = url.clone();
    connection
        .set_password(None)
        .map_err(|_| anyhow!("invalid PostgreSQL scratch URL"))?;
    Ok((connection, password))
}

async fn ensure_postgres_empty(pool: &PgPool) -> Result<()> {
    // Namespace dependencies catch tables (even with zero rows), views, sequences,
    // functions, types, collations, operators, etc. Also reject custom schemas and
    // database-wide objects. Only a fresh public schema and built-in plpgsql pass.
    let occupied: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM pg_catalog.pg_namespace
            WHERE nspname NOT IN ('public', 'information_schema') AND nspname !~ '^pg_'
        ) OR EXISTS (
            SELECT 1 FROM pg_catalog.pg_depend d
            JOIN pg_catalog.pg_namespace n ON d.refobjid = n.oid
            WHERE d.refclassid = 'pg_catalog.pg_namespace'::regclass
              AND n.nspname = 'public'
        ) OR EXISTS (SELECT 1 FROM pg_catalog.pg_extension WHERE extname <> 'plpgsql')
          OR EXISTS (SELECT 1 FROM pg_catalog.pg_largeobject_metadata)
          OR EXISTS (SELECT 1 FROM pg_catalog.pg_event_trigger)
          OR EXISTS (SELECT 1 FROM pg_catalog.pg_publication)
          OR EXISTS (SELECT 1 FROM pg_catalog.pg_foreign_server)
    "#,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| database_error("PostgreSQL emptiness check", e))?;
    ensure!(
        !occupied,
        "scratch database is not empty; no bootstrap was run; supply a newly created dedicated disposable database"
    );
    Ok(())
}

fn normalize_postgres(sql: &str) -> String {
    let mut lines = Vec::new();
    for line in sql.lines() {
        if line.starts_with("-- Dumped from database version ")
            || line.starts_with("-- Dumped by pg_dump version ")
            || line.starts_with("-- Started on ")
            || line.starts_with("-- Completed on ")
            // This pg_dump 17+ session setting is not schema and breaks older clients.
            || line == "SET transaction_timeout = 0;"
        {
            continue;
        }
        // Recent pg_dump security updates add a random psql restrict key. Keep
        // the guard, but stabilize its token for our trusted migration-only DDL.
        if line.starts_with("\\restrict ") {
            lines.push("\\restrict NyroSchemaDump");
        } else if line.starts_with("\\unrestrict ") {
            lines.push("\\unrestrict NyroSchemaDump");
        } else {
            lines.push(line);
        }
    }
    format!("{}\n", lines.join("\n").trim())
}

async fn mysql_schema(url: &Url) -> Result<String> {
    let storage = MysqlStorage::connect(config(url))
        .await
        .map_err(|e| database_error("MySQL scratch connection", e))?;
    let result = async {
        ensure_mysql_empty(storage.pool()).await?;
        bootstrap(&storage).await?;
        mysql_ddl(storage.pool()).await
    }
    .await;
    storage.pool().close().await;
    result
}

async fn ensure_mysql_empty(pool: &MySqlPool) -> Result<()> {
    let occupied: i64 = sqlx::query_scalar(
        r#"
        SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = DATABASE())
            OR EXISTS (SELECT 1 FROM information_schema.routines WHERE routine_schema = DATABASE())
            OR EXISTS (SELECT 1 FROM information_schema.triggers WHERE trigger_schema = DATABASE())
            OR EXISTS (SELECT 1 FROM information_schema.events WHERE event_schema = DATABASE())
    "#,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| database_error("MySQL emptiness check", e))?;
    ensure!(
        occupied == 0,
        "scratch database is not empty; no bootstrap was run; supply a newly created dedicated disposable database"
    );
    Ok(())
}

async fn mysql_ddl(pool: &MySqlPool) -> Result<String> {
    let non_table_objects: i64 = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.routines WHERE routine_schema = DATABASE()) OR EXISTS (SELECT 1 FROM information_schema.triggers WHERE trigger_schema = DATABASE()) OR EXISTS (SELECT 1 FROM information_schema.events WHERE event_schema = DATABASE())",
    ).fetch_one(pool).await.map_err(|e| database_error("MySQL non-table object inspection", e))?;
    ensure!(
        non_table_objects == 0,
        "MySQL dump does not support routines, triggers or events; refusing an incomplete schema"
    );
    // MySQL 8 metadata is binary-collated (including TABLE_TYPE's ENUM).
    // Explicit text casts let SQLx decode names without changing any dumped DDL.
    let tables: Vec<(String, String)> = sqlx::query_as(
        "SELECT CAST(TABLE_NAME AS CHAR CHARACTER SET utf8mb4) COLLATE utf8mb4_unicode_ci, CAST(TABLE_TYPE AS CHAR CHARACTER SET utf8mb4) COLLATE utf8mb4_unicode_ci FROM information_schema.tables WHERE table_schema = DATABASE() ORDER BY TABLE_NAME",
    ).fetch_all(pool).await.map_err(|e| database_error("MySQL table inspection", e))?;
    ensure!(
        tables.iter().all(|(_, kind)| kind == "BASE TABLE"),
        "MySQL dump supports base tables only; refusing an incomplete schema"
    );
    let tables: Vec<String> = tables.into_iter().map(|(name, _)| name).collect();
    ensure_final_tables(&tables)?;
    // SHOW CREATE TABLE includes full PKs, indexes, CHECKs, FKs, column/table
    // character sets and binary collations. Never reconstruct DDL from columns.
    let foreign_keys: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT CAST(TABLE_NAME AS CHAR CHARACTER SET utf8mb4) COLLATE utf8mb4_unicode_ci, CAST(REFERENCED_TABLE_NAME AS CHAR CHARACTER SET utf8mb4) COLLATE utf8mb4_unicode_ci, CAST(REFERENCED_TABLE_SCHEMA AS CHAR CHARACTER SET utf8mb4) COLLATE utf8mb4_unicode_ci FROM information_schema.key_column_usage WHERE table_schema = DATABASE() AND REFERENCED_TABLE_NAME IS NOT NULL",
    ).fetch_all(pool).await.map_err(|e| database_error("MySQL foreign-key inspection", e))?;
    let database: String = sqlx::query_scalar("SELECT DATABASE()")
        .fetch_one(pool)
        .await
        .map_err(|e| database_error("MySQL database inspection", e))?;
    ensure!(
        foreign_keys
            .iter()
            .all(|(_, _, schema)| schema == &database),
        "MySQL schema contains cross-database foreign keys; refusing an incomplete dump"
    );
    let dependencies = foreign_keys
        .into_iter()
        .map(|(child, parent, _)| (child, parent))
        .collect::<Vec<_>>();
    let order = dependency_order(tables, dependencies)?;
    let mut sql = header("MySQL", "mysql");
    sql.push_str("SET NAMES utf8mb4;\n\n");
    // Pin quoting even if the server/session defaults change. A single pool
    // connection keeps this setting consistent across every SHOW CREATE TABLE.
    sqlx::query("SET SESSION sql_quote_show_create = 1")
        .execute(pool)
        .await
        .map_err(|e| database_error("MySQL DDL quoting setup", e))?;
    for table in order {
        let row = sqlx::query(&format!("SHOW CREATE TABLE {}", mysql_identifier(&table)))
            .fetch_one(pool)
            .await
            .map_err(|e| database_error("MySQL SHOW CREATE TABLE", e))?;
        let ddl: String = row
            .try_get(1)
            .context("MySQL SHOW CREATE TABLE did not return DDL")?;
        ensure!(
            ddl.starts_with("CREATE TABLE "),
            "MySQL returned unexpected DDL; refusing an incomplete schema"
        );
        sql.push_str(ddl.trim_end_matches(';'));
        sql.push_str(";\n\n");
    }
    sql.truncate(sql.trim_end().len());
    sql.push('\n');
    Ok(sql)
}

fn dependency_order(
    tables: Vec<String>,
    dependencies: Vec<(String, String)>,
) -> Result<Vec<String>> {
    let mut remaining: BTreeMap<String, BTreeSet<String>> = tables
        .into_iter()
        .map(|table| (table, BTreeSet::new()))
        .collect();
    for (child, parent) in dependencies {
        ensure!(
            remaining.contains_key(&parent),
            "foreign key references a table absent from the dump"
        );
        let parents = remaining
            .get_mut(&child)
            .context("foreign key child is absent from the dump")?;
        if child != parent {
            parents.insert(parent);
        }
    }
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        let Some(table) = remaining
            .iter()
            .find(|(_, parents)| parents.is_empty())
            .map(|(table, _)| table.clone())
        else {
            bail!(
                "cyclic MySQL foreign keys cannot be emitted in safe table order; no output was written"
            );
        };
        remaining.remove(&table);
        for parents in remaining.values_mut() {
            parents.remove(&table);
        }
        ordered.push(table);
    }
    Ok(ordered)
}

fn mysql_identifier(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

fn ensure_final_tables(tables: &[String]) -> Result<()> {
    ensure!(
        [
            "models",
            "model_backends",
            "api_key_models",
            "provider_model_ratings"
        ]
        .iter()
        .all(|name| tables.iter().any(|table| table == name))
            && !["routes", "route_targets", "api_key_routes"]
                .iter()
                .any(|name| tables.iter().any(|table| table == name)),
        "storage migrations did not produce the final table names; refusing an incomplete schema"
    );
    Ok(())
}

fn header(name: &str, backend: &str) -> String {
    format!(
        "-- Nyro AI Gateway — {name} final post-migration reference schema\n\
         -- Generated by nyro-tools from nyro-core StorageBootstrap::init + migrate.\n\
         -- Derived artifact: do not edit SQL by hand. No application data is included.\n\
         -- Regenerate using a NEW EMPTY disposable database for each invocation:\n\
         -- NYRO_SCHEMA_DATABASE_URL='<scratch-url>' nyro-tools dump-schema --backend {backend} --output deploy/schema/{backend}.sql\n\
         -- See docs/database/schema.md for prerequisites and safety restrictions.\n\n"
    )
}

fn write_atomic(path: &Path, sql: &str) -> Result<()> {
    let directory = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(directory)
        .context("create schema temporary file beside output")?;
    temporary
        .write_all(sql.as_bytes())
        .context("write completed schema to temporary file")?;
    temporary
        .as_file()
        .sync_all()
        .context("flush schema temporary file")?;
    temporary.persist(path).map_err(|_| {
        anyhow!("could not atomically replace schema output; previous file was not truncated")
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_explicit_matching_scratch_destination() {
        for raw in [
            "postgres://localhost/db",
            "postgres://u@localhost",
            "postgres://u@localhost/postgres",
            "postgres://u@localhost/a?dbname=other",
            "postgres://u@localhost/a?host=elsewhere",
            "postgres://u@localhost/a?options=-csearch_path=other",
            "mysql://u@localhost/db",
        ] {
            assert!(scratch_url(SchemaBackend::Postgres, raw).is_err());
        }
        assert_eq!(
            scratch_url(
                SchemaBackend::Postgres,
                "postgresql://u@localhost/nyro_schema"
            )
            .unwrap()
            .port(),
            Some(5432)
        );
        assert!(
            scratch_url(
                SchemaBackend::Mysql,
                "mysql://u@localhost/nyro_schema?ssl-mode=DISABLED"
            )
            .is_ok()
        );
        assert!(scratch_url(SchemaBackend::Mysql, "mysql://u@localhost/mysql").is_err());
    }

    #[tokio::test]
    async fn missing_url_leaves_output_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("schema.sql");
        std::fs::write(&path, "previous complete schema").unwrap();
        let result = run(DumpSchemaArgs {
            backend: SchemaBackend::Postgres,
            scratch_db_url: None,
            output: Some(path.clone()),
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "previous complete schema"
        );
    }

    #[test]
    fn errors_never_echo_connection_credentials() {
        let error = database_error(
            "scratch connection",
            anyhow!("postgres://admin:very-secret@localhost/db"),
        );
        assert!(!format!("{error:?}").contains("very-secret"));
        let error = scratch_url(
            SchemaBackend::Postgres,
            "postgres://u:very-secret@[invalid/db",
        )
        .unwrap_err();
        assert!(!format!("{error:?}").contains("very-secret"));
    }

    #[test]
    fn postgres_dump_target_is_explicit_and_password_free() {
        let url = scratch_url(
            SchemaBackend::Postgres,
            "postgresql://u:p%40ss%3Aword@127.0.0.1:25439/scratch?sslmode=disable",
        )
        .unwrap();
        let (connection, password) = postgres_dump_connection(&url).unwrap();
        assert_eq!(
            connection.as_str(),
            "postgresql://u@127.0.0.1:25439/scratch?sslmode=disable"
        );
        assert_eq!(password.as_deref(), Some("p@ss:word"));
        assert!(connection.password().is_none());
        assert!(
            scratch_url(
                SchemaBackend::Postgres,
                "postgresql://u@localhost/scratch?password=secret"
            )
            .is_err()
        );
    }

    #[test]
    fn postgres_normalization_is_deterministic_and_preserves_schema() {
        let ddl = "CREATE TABLE public.ratings (model text COLLATE pg_catalog.\"C\", score integer CHECK (score >= 0 AND score <= 100));";
        let first = format!(
            "-- Dumped from database version 16.15\n-- Dumped by pg_dump version 16.15\n\\restrict randomA\n{ddl}\n\\unrestrict randomA\n"
        );
        let second = format!(
            "-- Dumped from database version 17.7\n-- Dumped by pg_dump version 17.7\n\\restrict randomB\nSET transaction_timeout = 0;\n{ddl}\n\\unrestrict randomB\n"
        );
        let normalized = normalize_postgres(&first);
        assert_eq!(normalized, normalize_postgres(&second));
        assert!(normalized.contains(ddl));
        assert!(normalized.contains("\\restrict NyroSchemaDump"));
        assert_eq!(normalize_postgres(&normalized), normalized);
    }

    #[test]
    fn mysql_order_is_deterministic_and_parent_first() {
        let tables = vec!["ratings".into(), "providers".into(), "keys".into()];
        let dependencies = vec![
            ("ratings".into(), "providers".into()),
            ("providers".into(), "providers".into()),
        ];
        let expected = vec!["keys", "providers", "ratings"];
        assert_eq!(
            dependency_order(tables.clone(), dependencies.clone()).unwrap(),
            expected
        );
        assert_eq!(
            dependency_order(
                tables.into_iter().rev().collect(),
                dependencies.into_iter().rev().collect()
            )
            .unwrap(),
            expected
        );
        assert_eq!(mysql_identifier("a`b"), "`a``b`");
    }

    #[test]
    fn mysql_cycles_and_unknown_dependencies_fail_closed() {
        assert!(
            dependency_order(
                vec!["a".into(), "b".into()],
                vec![("a".into(), "b".into()), ("b".into(), "a".into())]
            )
            .is_err()
        );
        assert!(dependency_order(vec!["a".into()], vec![("a".into(), "missing".into())]).is_err());
    }

    #[test]
    fn legacy_or_incomplete_tables_are_not_emitted() {
        assert!(ensure_final_tables(&["routes".into()]).is_err());
        let mut tables = vec![
            "models".into(),
            "model_backends".into(),
            "api_key_models".into(),
        ];
        assert!(
            ensure_final_tables(&tables).is_err(),
            "ratings table is required"
        );
        tables.push("provider_model_ratings".into());
        assert!(ensure_final_tables(&tables).is_ok());
        tables.push("route_targets".into());
        assert!(ensure_final_tables(&tables).is_err());
    }

    // Opt-in: each URL must name a NEW EMPTY database on a disposable instance.
    // These tests intentionally leave one empty sentinel table for inspection.
    // Missing configuration is a failure, never a silently skipped DB test.
    #[tokio::test]
    #[ignore = "requires NYRO_SCHEMA_TEST_POSTGRES_URL for a new empty disposable database"]
    async fn postgres_nonempty_rejected_without_bootstrap() {
        let raw = std::env::var("NYRO_SCHEMA_TEST_POSTGRES_URL")
            .expect("explicit disposable PostgreSQL test URL required");
        let url = scratch_url(SchemaBackend::Postgres, &raw).unwrap();
        let storage = PostgresStorage::connect(config(&url)).await.unwrap();
        ensure_postgres_empty(storage.pool()).await.unwrap();
        sqlx::query("CREATE TABLE schema_dump_sentinel (id INTEGER)")
            .execute(storage.pool())
            .await
            .unwrap();
        assert_nonempty_preserves_output(SchemaBackend::Postgres, &raw).await;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_catalog.pg_tables WHERE schemaname = 'public'",
        )
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert_eq!(count, 1, "bootstrap must not create any tables");
        storage.pool().close().await;
    }

    #[tokio::test]
    #[ignore = "requires NYRO_SCHEMA_TEST_MYSQL_URL for a new empty disposable database"]
    async fn mysql_nonempty_rejected_without_bootstrap() {
        let raw = std::env::var("NYRO_SCHEMA_TEST_MYSQL_URL")
            .expect("explicit disposable MySQL test URL required");
        let url = scratch_url(SchemaBackend::Mysql, &raw).unwrap();
        let storage = MysqlStorage::connect(config(&url)).await.unwrap();
        ensure_mysql_empty(storage.pool()).await.unwrap();
        sqlx::query("CREATE TABLE schema_dump_sentinel (id INTEGER)")
            .execute(storage.pool())
            .await
            .unwrap();
        assert_nonempty_preserves_output(SchemaBackend::Mysql, &raw).await;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE()",
        )
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert_eq!(count, 1, "bootstrap must not create any tables");
        storage.pool().close().await;
    }

    async fn assert_nonempty_preserves_output(backend: SchemaBackend, raw: &str) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("schema.sql");
        std::fs::write(&path, "previous complete schema").unwrap();
        let error = run(DumpSchemaArgs {
            backend,
            scratch_db_url: Some(raw.into()),
            output: Some(path.clone()),
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("scratch database is not empty"));
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "previous complete schema"
        );
    }

    #[test]
    fn atomic_output_replaces_only_with_complete_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("schema.sql");
        std::fs::write(&path, "old").unwrap();
        write_atomic(&path, "complete new schema\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "complete new schema\n"
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
