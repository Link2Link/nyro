use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
#[cfg(feature = "embed-webui")]
use axum::body::Body;
#[cfg(feature = "embed-webui")]
use axum::http::StatusCode;
use axum::http::{HeaderValue, Method, header};
#[cfg(feature = "embed-webui")]
use axum::response::Response;
use clap::Parser;
use nyro_core::{
    Gateway,
    config::{
        GatewayConfig, GatewayStorageConfig, SqlStorageConfig, StorageBackendKind, UpstreamTimeouts,
    },
    logging,
    storage::MemoryStorage,
    update::{RestartTarget, UpdateOptions, UpdateService},
};
use tokio::sync::watch;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};

#[cfg(feature = "embed-webui")]
use rust_embed::RustEmbed;

mod admin_routes;
mod self_update;
mod yaml_config;

/// How long in-flight requests may drain before a self-update restart.
const RESTART_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(feature = "embed-webui")]
#[derive(RustEmbed)]
#[folder = "../webui/dist/"]
struct WebUiAssets;

// ── Run mode ──────────────────────────────────────────────────────────────────

#[derive(clap::ValueEnum, Clone, Default, PartialEq)]
enum Mode {
    /// Start both proxy and admin listeners (default).
    #[default]
    All,
    /// Start only the proxy (data plane). Admin API and WebUI are disabled.
    Proxy,
    /// Start only the admin listener (control plane + optional WebUI). No proxy.
    Admin,
}

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "nyro-server", version, about = "Nyro AI Gateway — Server Mode")]
struct Args {
    // ── Server ────────────────────────────────────────────────────────────────
    #[arg(
        long,
        default_value = "all",
        env = "NYRO_MODE",
        value_enum,
        help = "Run mode: all | proxy | admin",
        help_heading = "Server"
    )]
    mode: Mode,

    #[arg(
        long,
        default_value = "127.0.0.1",
        env = "NYRO_PROXY_HOST",
        help_heading = "Server"
    )]
    proxy_host: String,

    #[arg(
        long,
        default_value_t = 19530,
        env = "NYRO_PROXY_PORT",
        help_heading = "Server"
    )]
    proxy_port: u16,

    #[arg(
        long,
        default_value = "127.0.0.1",
        env = "NYRO_ADMIN_HOST",
        help_heading = "Server"
    )]
    admin_host: String,

    #[arg(
        long,
        default_value_t = 19531,
        env = "NYRO_ADMIN_PORT",
        help_heading = "Server"
    )]
    admin_port: u16,

    #[arg(
        long,
        env = "NYRO_PROXY_AUTH_KEY",
        help = "Require `Authorization: Bearer <key>` on every proxy data-plane route (health endpoints stay open). Recommended whenever the proxy listens beyond loopback.",
        help_heading = "Server"
    )]
    proxy_auth_key: Option<String>,

    #[arg(
        long,
        default_value = "false",
        action = clap::ArgAction::SetTrue,
        env = "NYRO_LAN",
        help = "Expose the gateway to the local network: bind proxy and admin listeners to 0.0.0.0. Requires --proxy-auth-key and --admin-token (refuses to start without them).",
        help_heading = "Server"
    )]
    lan: bool,

    #[arg(
        long,
        env = "NYRO_ADMIN_TOKEN",
        help = "Bearer token for admin API authentication",
        help_heading = "Server"
    )]
    admin_token: Option<String>,

    #[arg(
        long,
        default_value = "info",
        env = "NYRO_LOG_LEVEL",
        value_parser = ["error", "warn", "info", "debug", "trace"],
        help_heading = "Server"
    )]
    log_level: String,

    // ── Upstream inference timeouts ───────────────────────────────────────────
    #[arg(
        long,
        default_value_t = UpstreamTimeouts::default().connect.as_secs(),
        env = "NYRO_UPSTREAM_CONNECT_TIMEOUT_SECS",
        value_parser = clap::value_parser!(u64).range(1..=86400),
        help = "Upstream inference connection timeout in seconds (1-86400)",
        help_heading = "Upstream timeouts"
    )]
    upstream_connect_timeout_secs: u64,

    #[arg(
        long,
        default_value_t = UpstreamTimeouts::default().response_headers.as_secs(),
        env = "NYRO_UPSTREAM_RESPONSE_HEADERS_TIMEOUT_SECS",
        value_parser = clap::value_parser!(u64).range(1..=86400),
        help = "Upstream inference response-header wait timeout in seconds (1-86400)",
        help_heading = "Upstream timeouts"
    )]
    upstream_response_headers_timeout_secs: u64,

    #[arg(
        long,
        default_value_t = UpstreamTimeouts::default().first_byte.as_secs(),
        env = "NYRO_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS",
        value_parser = clap::value_parser!(u64).range(1..=86400),
        help = "Upstream inference first non-empty body byte timeout after headers in seconds (1-86400)",
        help_heading = "Upstream timeouts"
    )]
    upstream_first_byte_timeout_secs: u64,

    #[arg(
        long,
        default_value_t = UpstreamTimeouts::default().stream_idle.as_secs(),
        env = "NYRO_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS",
        value_parser = clap::value_parser!(u64).range(1..=86400),
        help = "Upstream inference raw-body idle timeout in seconds (1-86400) (excludes downstream backpressure)",
        help_heading = "Upstream timeouts"
    )]
    upstream_stream_idle_timeout_secs: u64,

    // ── Advanced (CORS) ───────────────────────────────────────────────────────
    #[arg(
        long = "admin-cors-origin",
        action = clap::ArgAction::Append,
        help = "Allowed CORS origin for admin API (repeatable, use '*' for any)",
        help_heading = "Advanced"
    )]
    admin_cors_origins: Vec<String>,

    #[arg(
        long = "proxy-cors-origin",
        action = clap::ArgAction::Append,
        help = "Allowed CORS origin for proxy API (repeatable, use '*' for any)",
        help_heading = "Advanced"
    )]
    proxy_cors_origins: Vec<String>,

    // ── Storage ───────────────────────────────────────────────────────────────
    #[arg(
        long,
        default_value = "~/.nyro",
        env = "NYRO_DATA_DIR",
        help_heading = "Storage"
    )]
    data_dir: String,

    #[arg(long, value_parser = ["sqlite", "postgres", "mysql"], default_value = "sqlite",
          env = "NYRO_STORAGE_BACKEND", help_heading = "Storage")]
    storage_backend: String,

    #[arg(
        long,
        default_value = "true",
        action = clap::ArgAction::Set,
        env = "NYRO_MIGRATE_ON_START",
        help = "Run schema migrations on startup for all backends (true/false). Set false to skip DDL at runtime and use --migrate-only separately.",
        help_heading = "Storage"
    )]
    migrate_on_start: bool,

    #[arg(
        long,
        default_value = "false",
        action = clap::ArgAction::SetTrue,
        help = "Run database migrations then exit (useful as a K8S init Job or initContainer). Requires --storage-backend and corresponding DSN.",
        help_heading = "Storage"
    )]
    migrate_only: bool,

    #[arg(
        long,
        env = "NYRO_POSTGRES_DSN",
        help = "PostgreSQL connection string (required when --storage-backend=postgres)",
        help_heading = "Storage"
    )]
    postgres_dsn: Option<String>,

    #[arg(
        long,
        default_value_t = 10,
        help = "Postgres: max connection pool size",
        help_heading = "Storage"
    )]
    postgres_max_connections: u32,

    #[arg(
        long,
        default_value_t = 1,
        help = "Postgres: min connection pool size",
        help_heading = "Storage"
    )]
    postgres_min_connections: u32,

    #[arg(
        long,
        help = "Postgres: idle connection timeout (seconds)",
        help_heading = "Storage"
    )]
    postgres_idle_timeout: Option<u64>,

    // ── MySQL ─────────────────────────────────────────────────────────────────
    #[arg(
        long,
        env = "NYRO_MYSQL_DSN",
        help = "MySQL connection string (required when --storage-backend=mysql)",
        help_heading = "Storage"
    )]
    mysql_dsn: Option<String>,

    #[arg(
        long,
        default_value_t = 10,
        help = "MySQL: max connection pool size",
        help_heading = "Storage"
    )]
    mysql_max_connections: u32,

    #[arg(
        long,
        default_value_t = 1,
        help = "MySQL: min connection pool size",
        help_heading = "Storage"
    )]
    mysql_min_connections: u32,

    #[arg(
        long,
        help = "MySQL: idle connection timeout (seconds)",
        help_heading = "Storage"
    )]
    mysql_idle_timeout: Option<u64>,

    // ── Multi-replica ─────────────────────────────────────────────────────────
    #[arg(
        long,
        default_value_t = 3,
        env = "NYRO_CONFIG_POLL_INTERVAL",
        help = "Seconds between config epoch polls for multi-replica cache sync (0 = disabled)",
        help_heading = "Multi-replica"
    )]
    config_poll_interval: u64,

    #[arg(
        long,
        env = "NYRO_WEBUI_DIR",
        help = "Serve WebUI from this directory instead of the embedded assets (optional; applies to admin/all modes)",
        help_heading = "Multi-replica"
    )]
    webui_dir: Option<PathBuf>,

    // ── Updates ───────────────────────────────────────────────────────────────
    #[arg(
        long,
        default_value = "true",
        action = clap::ArgAction::Set,
        env = "NYRO_UPDATE_CHECK",
        help = "Allow the admin console to check for new releases (true/false). Set false for offline deployments.",
        help_heading = "Updates"
    )]
    update_check: bool,

    #[arg(
        long,
        default_value = "true",
        action = clap::ArgAction::Set,
        env = "NYRO_SELF_UPDATE",
        help = "Allow installing new releases in place from the admin console (true/false). Set false when upgrades are managed by images or packages.",
        help_heading = "Updates"
    )]
    self_update: bool,

    #[arg(
        long,
        default_value = nyro_core::update::DEFAULT_MANIFEST_URL,
        env = "NYRO_UPDATE_MANIFEST_URL",
        help = "Signed release manifest URL used for update checks",
        help_heading = "Updates"
    )]
    update_manifest_url: String,

    // ── Standalone ────────────────────────────────────────────────────────────
    #[arg(
        long = "config",
        short = 'c',
        help = "Path to YAML config file for standalone mode (no DB, no admin API); --mode is ignored when this flag is set",
        help_heading = "Standalone"
    )]
    config_file: Option<String>,
}

fn build_upstream_timeouts(args: &Args) -> UpstreamTimeouts {
    UpstreamTimeouts {
        connect: Duration::from_secs(args.upstream_connect_timeout_secs),
        response_headers: Duration::from_secs(args.upstream_response_headers_timeout_secs),
        first_byte: Duration::from_secs(args.upstream_first_byte_timeout_secs),
        stream_idle: Duration::from_secs(args.upstream_stream_idle_timeout_secs),
    }
}

#[cfg(test)]
mod timeout_config_tests {
    use super::*;
    use clap::CommandFactory;

    const TIMEOUT_ARGS: [(&str, &str, u64); 4] = [
        (
            "upstream_connect_timeout_secs",
            "NYRO_UPSTREAM_CONNECT_TIMEOUT_SECS",
            30,
        ),
        (
            "upstream_response_headers_timeout_secs",
            "NYRO_UPSTREAM_RESPONSE_HEADERS_TIMEOUT_SECS",
            600,
        ),
        (
            "upstream_first_byte_timeout_secs",
            "NYRO_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS",
            600,
        ),
        (
            "upstream_stream_idle_timeout_secs",
            "NYRO_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS",
            120,
        ),
    ];

    #[test]
    fn timeout_defaults_and_environment_names_match_core_config() {
        let command = Args::command();
        for (id, env, seconds) in TIMEOUT_ARGS {
            let arg = command
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .unwrap();
            assert_eq!(arg.get_env(), Some(std::ffi::OsStr::new(env)));
            assert_eq!(arg.get_default_values().len(), 1);
            assert_eq!(arg.get_default_values()[0], seconds.to_string().as_str());
        }
        assert_eq!(
            GatewayConfig::default().upstream_timeouts,
            UpstreamTimeouts {
                connect: Duration::from_secs(30),
                response_headers: Duration::from_secs(600),
                first_byte: Duration::from_secs(600),
                stream_idle: Duration::from_secs(120),
            }
        );
    }

    #[test]
    fn timeout_overrides_apply_in_database_and_yaml_modes() {
        for standalone in [false, true] {
            let mut argv = vec![
                "nyro-server",
                "--upstream-connect-timeout-secs",
                "11",
                "--upstream-response-headers-timeout-secs",
                "22",
                "--upstream-first-byte-timeout-secs",
                "33",
                "--upstream-stream-idle-timeout-secs",
                "44",
            ];
            if standalone {
                argv.extend(["--config", "fixture.yaml"]);
            }
            let args = Args::try_parse_from(argv).expect("positive timeout options should parse");
            assert_eq!(
                build_upstream_timeouts(&args),
                UpstreamTimeouts {
                    connect: Duration::from_secs(11),
                    response_headers: Duration::from_secs(22),
                    first_byte: Duration::from_secs(33),
                    stream_idle: Duration::from_secs(44),
                }
            );
        }
    }

    #[test]
    fn update_options_default_on_and_accept_explicit_values() {
        let args = Args::try_parse_from(["nyro-server"]).unwrap();
        let options = build_update_options(&args, PathBuf::from("/data"));
        assert!(options.check_enabled && options.self_update_enabled);
        assert_eq!(
            options.manifest_url,
            nyro_core::update::DEFAULT_MANIFEST_URL
        );
        assert_eq!(options.current_version, env!("CARGO_PKG_VERSION"));

        let args = Args::try_parse_from([
            "nyro-server",
            "--update-check",
            "false",
            "--self-update",
            "false",
            "--update-manifest-url",
            " https://mirror.example/nyro-server-update.json ",
        ])
        .unwrap();
        let options = build_update_options(&args, PathBuf::from("/data"));
        assert!(!options.check_enabled && !options.self_update_enabled);
        assert_eq!(
            options.manifest_url,
            "https://mirror.example/nyro-server-update.json"
        );

        let command = Args::command();
        for (id, env) in [
            ("update_check", "NYRO_UPDATE_CHECK"),
            ("self_update", "NYRO_SELF_UPDATE"),
            ("update_manifest_url", "NYRO_UPDATE_MANIFEST_URL"),
        ] {
            let arg = command
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .unwrap();
            assert_eq!(arg.get_env(), Some(std::ffi::OsStr::new(env)));
        }
    }

    #[test]
    fn timeout_options_reject_zero_negative_and_non_integer_values() {
        for (id, _, _) in TIMEOUT_ARGS {
            let flag = format!("--{}", id.replace('_', "-"));
            for invalid in ["0", "-1", "1.5", "invalid", "86401", "18446744073709551615"] {
                assert!(
                    Args::try_parse_from(["nyro-server", flag.as_str(), invalid]).is_err(),
                    "{flag} must reject {invalid}"
                );
            }
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// How the async server body finished.
enum RunOutcome {
    Exit,
    Restart(RestartTarget),
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let filter = format!("nyro={level},tower_http={level}", level = args.log_level);
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Before any listener binds: hand off to an installed newer binary.
    self_update::bootstrap(&PathBuf::from(
        shellexpand::tilde(&args.data_dir).to_string(),
    ));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let outcome = runtime.block_on(run(&args));
    // Stop background tasks before the process image is replaced.
    runtime.shutdown_timeout(Duration::from_secs(2));

    match outcome? {
        RunOutcome::Exit => Ok(()),
        RunOutcome::Restart(target) => {
            let error = self_update::restart_into(&target);
            Err(anyhow::anyhow!(
                "failed to restart into nyro-server {}: {error}",
                target.version
            ))
        }
    }
}

async fn run(args: &Args) -> anyhow::Result<RunOutcome> {
    if let Some(ref config_path) = args.config_file {
        if args.mode != Mode::All {
            tracing::warn!(
                "standalone mode (--config) is active; --mode={} is ignored",
                match args.mode {
                    Mode::Proxy => "proxy",
                    Mode::Admin => "admin",
                    Mode::All => "all",
                }
            );
        }
        run_standalone(config_path, args).await?;
        return Ok(RunOutcome::Exit);
    }

    if args.migrate_only {
        run_migrate(args).await?;
        return Ok(RunOutcome::Exit);
    }

    run_full(args).await
}

// ── Migrate-only mode ─────────────────────────────────────────────────────────

async fn run_migrate(args: &Args) -> anyhow::Result<()> {
    let data_dir = shellexpand::tilde(&args.data_dir).to_string();

    tracing::info!(
        "migrate-only: connecting to {} backend",
        args.storage_backend
    );

    let mut storage_config = build_storage_config(args)?;
    // Ensure migrations run even if --migrate-on-start=false was passed
    storage_config.migrate_on_start = true;

    let config = GatewayConfig {
        data_dir: PathBuf::from(data_dir),
        storage: storage_config,
        ..Default::default()
    };

    // Gateway::new connects to storage, runs init() + migrate(), then returns.
    // Background tasks are spawned but the runtime exits immediately after.
    let (_gateway, _log_rx) = Gateway::new(config).await?;

    tracing::info!("migrate-only: migrations completed successfully");
    Ok(())
}

// ── Standalone mode ───────────────────────────────────────────────────────────

async fn run_standalone(config_path: &str, args: &Args) -> anyhow::Result<()> {
    tracing::info!("standalone mode: loading {config_path}");
    let yaml = yaml_config::YamlConfig::load(config_path)?;

    // --lan overrides the YAML server section and requires proxy auth.
    let proxy_host = if args.lan {
        if args
            .proxy_auth_key
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            anyhow::bail!(
                "--lan requires --proxy-auth-key (or NYRO_PROXY_AUTH_KEY): \
                 exposing the proxy without auth would let any LAN device call your models"
            );
        }
        tracing::info!(
            "--lan: overriding YAML proxy_host {} → 0.0.0.0",
            yaml.server.proxy_host
        );
        "0.0.0.0".to_string()
    } else {
        yaml.server.proxy_host.clone()
    };
    let proxy_port = yaml.server.proxy_port;

    let providers = yaml_config::build_providers(&yaml);
    let models = yaml_config::build_models(&yaml, &providers);
    let settings: Vec<(String, String)> = yaml.settings.into_iter().collect();

    tracing::info!(
        "loaded {} providers, {} models from YAML",
        providers.len(),
        models.len()
    );

    let storage: nyro_core::storage::DynStorage =
        Arc::new(MemoryStorage::new(providers, models, settings));

    let data_dir = shellexpand::tilde(&args.data_dir).to_string();
    let proxy_cors_origins = if args.proxy_cors_origins.is_empty() {
        default_local_origins(&[proxy_port])
    } else {
        args.proxy_cors_origins.clone()
    };

    let config = GatewayConfig {
        proxy_host: proxy_host.clone(),
        proxy_port,
        proxy_cors_origins,
        auth_key: args.proxy_auth_key.clone().filter(|k| !k.trim().is_empty()),
        upstream_timeouts: build_upstream_timeouts(args),
        data_dir: PathBuf::from(data_dir),
        storage: GatewayStorageConfig::default(),
        ..Default::default()
    };

    let (gateway, log_rx) = Gateway::from_storage(config, storage).await?;
    let storage_for_logs = gateway.storage.clone();

    tokio::spawn(async move {
        logging::run_collector(log_rx, storage_for_logs).await;
    });

    tracing::info!("proxy  → http://{}:{}", proxy_host, proxy_port);
    tracing::info!("standalone mode: admin API and WebUI are disabled");

    gateway.start_proxy_with_shutdown(shutdown_signal()).await?;
    Ok(())
}

// ── Full mode (DB-backed, mode-aware) ─────────────────────────────────────────

async fn run_full(args: &Args) -> anyhow::Result<RunOutcome> {
    let data_dir = shellexpand::tilde(&args.data_dir).to_string();
    let admin_token = args.admin_token.clone().filter(|t| !t.trim().is_empty());

    // Warn about args that are irrelevant in proxy-only mode.
    if args.mode == Mode::Proxy {
        if admin_token.is_some() {
            tracing::warn!("--mode proxy: --admin-token is ignored (no admin listener)");
        }
        if args.webui_dir.is_some() {
            tracing::warn!("--mode proxy: --webui-dir is ignored (no admin listener)");
        }
    }

    // ── LAN mode: bind every listener to 0.0.0.0 and refuse to run without auth ──
    let (proxy_host, admin_host) = if args.lan {
        if args
            .proxy_auth_key
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            anyhow::bail!(
                "--lan requires --proxy-auth-key (or NYRO_PROXY_AUTH_KEY): \
                 exposing the proxy without auth would let any LAN device call your models"
            );
        }
        if matches!(args.mode, Mode::Admin | Mode::All) && admin_token.is_none() {
            anyhow::bail!(
                "--lan requires --admin-token: the admin API would otherwise be open to the whole LAN"
            );
        }
        tracing::info!(
            "--lan: proxy {}/{} → 0.0.0.0, admin {}/{} → 0.0.0.0",
            args.proxy_host,
            args.proxy_port,
            args.admin_host,
            args.admin_port
        );
        ("0.0.0.0".to_string(), "0.0.0.0".to_string())
    } else {
        (args.proxy_host.clone(), args.admin_host.clone())
    };

    // Admin token enforcement only when the admin listener is active.
    if matches!(args.mode, Mode::Admin | Mode::All)
        && !is_loopback_host(&admin_host)
        && admin_token.is_none()
    {
        anyhow::bail!(
            "--admin-token is required when --admin-host is not loopback (localhost/127.0.0.1/::1)"
        );
    }

    let admin_cors_origins = if args.admin_cors_origins.is_empty() {
        default_local_origins(&[args.admin_port])
    } else {
        args.admin_cors_origins.clone()
    };
    let proxy_cors_origins = if args.proxy_cors_origins.is_empty() {
        default_local_origins(&[args.proxy_port, args.admin_port])
    } else {
        args.proxy_cors_origins.clone()
    };

    let config = GatewayConfig {
        proxy_host: proxy_host.clone(),
        proxy_port: args.proxy_port,
        proxy_cors_origins,
        auth_key: args.proxy_auth_key.clone().filter(|k| !k.trim().is_empty()),
        upstream_timeouts: build_upstream_timeouts(args),
        data_dir: PathBuf::from(data_dir),
        storage: build_storage_config(args)?,
        config_poll_interval: Duration::from_secs(args.config_poll_interval),
    };

    let (gateway, log_rx) = Gateway::new(config).await?;

    let storage_for_logs = gateway.storage.clone();
    tokio::spawn(async move {
        logging::run_collector(log_rx, storage_for_logs).await;
    });

    // Self-update needs the admin API; proxy-only replicas are upgraded by
    // whatever manages them.
    let updater = if args.mode == Mode::Proxy {
        None
    } else {
        Some(UpdateService::new(
            gateway.storage.clone(),
            build_update_options(args, gateway.config.data_dir.clone()),
        )?)
    };
    let shutdown = Shutdown::new();
    shutdown.trigger_on(shutdown_signal());
    if let Some(updater) = updater.clone() {
        shutdown.trigger_on(async move {
            updater.restart_requested().await;
        });
    }

    match args.mode {
        Mode::Proxy => {
            let proxy_addr = format!("{}:{}", proxy_host, args.proxy_port);
            tracing::info!("mode=proxy  proxy → http://{proxy_addr}");
            tracing::info!("admin API and WebUI are disabled in proxy mode");
            gateway
                .start_proxy_with_shutdown(shutdown.clone().wait())
                .await?;
        }

        Mode::Admin => {
            let admin_addr = format!("{}:{}", admin_host, args.admin_port);
            let admin_router = admin_routes::create_router_with_updater(
                gateway.clone(),
                admin_token.clone(),
                updater.clone(),
            );
            let app = build_admin_app(admin_router, &args.webui_dir, &admin_cors_origins);
            let listener = tokio::net::TcpListener::bind(&admin_addr).await?;
            tracing::info!("mode=admin  admin → http://{admin_addr}");
            if admin_token.is_none() {
                tracing::warn!("admin API auth disabled: set --admin-token for production");
            }
            let serve = axum::serve(listener, app).with_graceful_shutdown(shutdown.clone().wait());
            drain_bounded(serve.into_future(), restart_drain_deadline(&updater)).await?;
        }

        Mode::All => {
            let gw_proxy = gateway.clone();
            let proxy_shutdown = shutdown.clone();
            let proxy_deadline = restart_drain_deadline(&updater);
            let proxy_task = tokio::spawn(async move {
                let serve = gw_proxy.start_proxy_with_shutdown(proxy_shutdown.wait());
                if let Err(e) = drain_bounded(serve, proxy_deadline).await {
                    tracing::error!("proxy server error: {e}");
                }
            });

            let admin_router = admin_routes::create_router_with_updater(
                gateway.clone(),
                admin_token.clone(),
                updater.clone(),
            );
            let app = build_admin_app(admin_router, &args.webui_dir, &admin_cors_origins);
            let admin_addr = format!("{}:{}", admin_host, args.admin_port);
            let listener = tokio::net::TcpListener::bind(&admin_addr).await?;

            let proxy_addr = format!("{}:{}", proxy_host, args.proxy_port);
            tracing::info!("mode=all  proxy → http://{proxy_addr}");
            tracing::info!("mode=all  admin → http://{admin_addr}");

            if admin_token.is_none() {
                tracing::warn!("admin API auth disabled: set --admin-token for production");
            }

            let serve = axum::serve(listener, app).with_graceful_shutdown(shutdown.clone().wait());
            let admin_result =
                drain_bounded(serve.into_future(), restart_drain_deadline(&updater)).await;

            // Stop the proxy too if the admin listener ended on its own.
            shutdown.trigger();
            if let Err(error) = proxy_task.await {
                tracing::error!("proxy server task failed: {error}");
            }

            admin_result?;
        }
    }

    if let Some(target) = updater.as_ref().and_then(UpdateService::pending_restart) {
        if tokio::time::timeout(Duration::from_secs(5), gateway.close_storage())
            .await
            .is_err()
        {
            tracing::warn!("timed out closing storage before restart");
        }
        return Ok(RunOutcome::Restart(target));
    }
    Ok(RunOutcome::Exit)
}

fn build_update_options(args: &Args, data_dir: PathBuf) -> UpdateOptions {
    let mut options = UpdateOptions::new(data_dir);
    options.check_enabled = args.update_check;
    options.self_update_enabled = args.self_update;
    options.manifest_url = args.update_manifest_url.trim().to_string();
    options
}

/// Resolves once a self-update restart has been requested plus the drain
/// budget; never resolves for ordinary shutdowns, which keep draining fully.
fn restart_drain_deadline(
    updater: &Option<UpdateService>,
) -> impl std::future::Future<Output = ()> + Send + 'static {
    let updater = updater.clone();
    async move {
        match updater {
            Some(updater) => {
                updater.restart_requested().await;
                tokio::time::sleep(RESTART_DRAIN_TIMEOUT).await;
            }
            None => std::future::pending::<()>().await,
        }
    }
}

/// Await a graceful-shutdown server future, abandoning it at `deadline` so
/// long-lived streams cannot hold a restart hostage.
async fn drain_bounded<E>(
    serve: impl std::future::Future<Output = Result<(), E>>,
    deadline: impl std::future::Future<Output = ()>,
) -> Result<(), E> {
    tokio::select! {
        result = serve => result,
        () = deadline => {
            tracing::warn!(
                "in-flight requests did not finish within {}s; restarting anyway",
                RESTART_DRAIN_TIMEOUT.as_secs()
            );
            Ok(())
        }
    }
}

// ── Admin app builder (three-state webui) ─────────────────────────────────────

fn build_admin_app(
    admin_router: Router,
    webui_dir: &Option<PathBuf>,
    cors_origins: &[String],
) -> Router {
    if let Some(dir) = webui_dir {
        let index = dir.join("index.html");
        tracing::info!("webui  serving from directory: {}", dir.display());
        admin_router
            .fallback_service(ServeDir::new(dir).fallback(ServeFile::new(index)))
            .layer(build_cors_layer(cors_origins))
    } else {
        #[cfg(feature = "embed-webui")]
        {
            admin_router
                .fallback(serve_webui)
                .layer(build_cors_layer(cors_origins))
        }
        #[cfg(not(feature = "embed-webui"))]
        {
            admin_router.layer(build_cors_layer(cors_origins))
        }
    }
}

#[cfg(test)]
mod webui_navigation_tests {
    use super::*;
    use tower::ServiceExt;

    #[tokio::test]
    async fn model_rating_spa_deep_link_returns_html_with_success_status() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("index.html"),
            "<!doctype html><title>Nyro</title>",
        )?;
        let app = build_admin_app(Router::new(), &Some(directory.path().to_path_buf()), &[]);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/model-ratings")
                    .body(axum::body::Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024).await?;
        assert_eq!(bytes.as_ref(), b"<!doctype html><title>Nyro</title>");
        Ok(())
    }
}

// ── Shutdown helpers ──────────────────────────────────────────────────────────

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!("failed to listen for shutdown signal: {error}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::warn!("failed to listen for SIGTERM: {error}"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}

/// One shutdown switch shared by every listener; flipped by OS signals or a
/// self-update restart request.
#[derive(Clone)]
struct Shutdown(Arc<watch::Sender<bool>>);

impl Shutdown {
    fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    fn trigger(&self) {
        self.0.send_replace(true);
    }

    fn trigger_on(&self, reason: impl std::future::Future<Output = ()> + Send + 'static) {
        let shutdown = self.clone();
        tokio::spawn(async move {
            reason.await;
            shutdown.trigger();
        });
    }

    async fn wait(self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|stopped| *stopped).await;
    }
}

// ── WebUI (embedded, feature-gated) ──────────────────────────────────────────

#[cfg(feature = "embed-webui")]
async fn serve_webui(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let file_path = if path.is_empty() { "index.html" } else { path };

    match WebUiAssets::get(file_path) {
        Some(content) => {
            let mime = infer_mime(file_path);
            Response::builder()
                .header(header::CONTENT_TYPE, mime)
                .body(Body::from(content.data.into_owned()))
                .unwrap()
        }
        None => match WebUiAssets::get("index.html") {
            Some(content) => Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(content.data.into_owned()))
                .unwrap(),
            None => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap(),
        },
    }
}

#[cfg(feature = "embed-webui")]
fn infer_mime(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") || path.ends_with(".mjs") {
        "application/javascript"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".woff") {
        "font/woff"
    } else if path.ends_with(".json") || path.ends_with(".map") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

// ── Storage ───────────────────────────────────────────────────────────────────

fn build_storage_config(args: &Args) -> anyhow::Result<GatewayStorageConfig> {
    let backend = parse_storage_backend(&args.storage_backend)?;

    let postgres_url = if matches!(backend, StorageBackendKind::Postgres) {
        let dsn = args
            .postgres_dsn
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "--postgres-dsn (or env NYRO_POSTGRES_DSN) is required \
                     when --storage-backend=postgres"
                )
            })?;
        Some(dsn.to_string())
    } else {
        None
    };

    let postgres = SqlStorageConfig {
        url: postgres_url,
        max_connections: args.postgres_max_connections,
        min_connections: args.postgres_min_connections,
        idle_timeout: args.postgres_idle_timeout.map(Duration::from_secs),
    };

    let mysql_url = if matches!(backend, StorageBackendKind::Mysql) {
        let dsn = args
            .mysql_dsn
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "--mysql-dsn (or env NYRO_MYSQL_DSN) is required \
                     when --storage-backend=mysql"
                )
            })?;
        Some(dsn.to_string())
    } else {
        None
    };

    let mysql = SqlStorageConfig {
        url: mysql_url,
        max_connections: args.mysql_max_connections,
        min_connections: args.mysql_min_connections,
        idle_timeout: args.mysql_idle_timeout.map(Duration::from_secs),
    };

    Ok(GatewayStorageConfig {
        backend,
        migrate_on_start: args.migrate_on_start,
        postgres,
        mysql,
    })
}

fn parse_storage_backend(value: &str) -> anyhow::Result<StorageBackendKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "sqlite" => Ok(StorageBackendKind::Sqlite),
        "postgres" => Ok(StorageBackendKind::Postgres),
        "mysql" => Ok(StorageBackendKind::Mysql),
        other => anyhow::bail!("unsupported storage backend: {other}"),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn default_local_origins(ports: &[u16]) -> Vec<String> {
    let mut origins = vec![
        "tauri://localhost".to_string(),
        "http://tauri.localhost".to_string(),
    ];
    for port in ports {
        origins.push(format!("http://127.0.0.1:{port}"));
        origins.push(format!("http://localhost:{port}"));
    }
    origins
}

fn parse_allow_origin(origins: &[String]) -> AllowOrigin {
    if origins.iter().any(|o| o.trim() == "*") {
        return AllowOrigin::any();
    }

    let values = origins
        .iter()
        .filter_map(|o| HeaderValue::from_str(o.trim()).ok())
        .collect::<Vec<_>>();

    if values.is_empty() {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(values)
    }
}

fn build_cors_layer(origins: &[String]) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(parse_allow_origin(origins))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            header::HeaderName::from_static("x-api-key"),
            header::HeaderName::from_static("anthropic-version"),
        ])
}
