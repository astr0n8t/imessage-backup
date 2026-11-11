use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use imsg_server::auth::{Throttle, Users, hash_password};
use imsg_server::index::Index;
use imsg_server::media::Tools;
use imsg_server::queries::Names;
use imsg_server::web::{AppState, router};
use tracing::{info, warn};

/// Host archived iMessage/SMS conversations as a password-protected web app.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct Paths {
    /// Archive folder written by imsg-extract (one sub-folder per device).
    #[arg(short, long, env = "IMSG_DATA")]
    data: PathBuf,
    /// Where the index, sessions and transcode cache live (default: <data>/.imsg-server).
    #[arg(long, env = "IMSG_STATE_DIR")]
    state_dir: Option<PathBuf>,
}

impl Paths {
    fn state_dir(&self) -> PathBuf {
        self.state_dir
            .clone()
            .unwrap_or_else(|| self.data.join(".imsg-server"))
    }
}

#[derive(Subcommand)]
enum Command {
    /// Run the web server.
    Serve {
        #[command(flatten)]
        paths: Paths,
        /// Address to listen on. Put a TLS reverse proxy in front for remote access.
        #[arg(short, long, env = "IMSG_LISTEN", default_value = "127.0.0.1:8080")]
        listen: SocketAddr,
        /// File of `username:argon2hash` lines (create with `imsg-server hash-password`).
        #[arg(long, env = "IMSG_AUTH_FILE")]
        auth_file: PathBuf,
        /// Seconds between scans of the data folder for new snapshots (0 disables).
        #[arg(long, env = "IMSG_SCAN_INTERVAL", default_value_t = 300)]
        scan_interval: u64,
        /// Mark the session cookie `Secure` (use when served over HTTPS).
        #[arg(long, env = "IMSG_SECURE_COOKIES")]
        secure_cookies: bool,
        /// Use X-Forwarded-For for login throttling (only behind a trusted proxy).
        #[arg(long, env = "IMSG_TRUST_FORWARDED_FOR")]
        trust_forwarded_for: bool,
        /// Days a login stays valid.
        #[arg(long, default_value_t = 30)]
        session_days: u64,
    },
    /// Ingest new snapshots into the index once and exit.
    Ingest {
        #[command(flatten)]
        paths: Paths,
    },
    /// Hash a password and print an auth-file line.
    HashPassword {
        /// Username for the printed line.
        #[arg(short, long, default_value = "admin")]
        user: String,
        /// Read the password from the first line of stdin instead of prompting.
        #[arg(long)]
        stdin: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match Cli::parse().command {
        Command::HashPassword { user, stdin } => {
            if user.contains(':') {
                bail!("usernames cannot contain ':'");
            }
            let p1 = if stdin {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                line.trim_end_matches(['\r', '\n']).to_owned()
            } else {
                let p1 = rpassword::prompt_password("Password: ")?;
                if rpassword::prompt_password("Repeat: ")? != p1 {
                    bail!("passwords do not match");
                }
                p1
            };
            if p1.len() < 8 {
                bail!("use at least 8 characters");
            }
            println!("{user}:{}", hash_password(&p1)?);
        }
        Command::Ingest { paths } => {
            let index = Index::open(&paths.state_dir().join("index.sqlite"))?;
            let data = paths.data.clone();
            let r = tokio::task::spawn_blocking(move || index.ingest_all(&data)).await??;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        Command::Serve {
            paths,
            listen,
            auth_file,
            scan_interval,
            secure_cookies,
            trust_forwarded_for,
            session_days,
        } => {
            let users = Users::load(&auth_file)?;
            if users.is_empty() {
                bail!(
                    "{} contains no users; add one with `imsg-server hash-password`",
                    auth_file.display()
                );
            }
            if !paths.data.is_dir() {
                bail!("data folder {} does not exist", paths.data.display());
            }
            let state_dir = paths.state_dir();
            let index = Index::open(&state_dir.join("index.sqlite")).context("opening index")?;
            let names = Names::load(&index.read()?)?;
            let tools = Tools::detect();
            info!(users = users.len(), transcoding = %tools.describe(), "starting");
            let state = Arc::new(AppState {
                index,
                data: paths.data.clone(),
                cache: state_dir.join("cache"),
                users,
                throttle: Throttle::default(),
                names: RwLock::new(Arc::new(names)),
                tools,
                secure_cookies,
                trust_forwarded_for,
                session_ttl: Duration::from_secs(session_days * 86_400),
                ingest_lock: tokio::sync::Mutex::new(()),
                last_ingest: RwLock::new(None),
            });

            let scanner = state.clone();
            tokio::spawn(async move {
                loop {
                    match scanner.ingest().await {
                        Ok(r) if r.snapshots > 0 || r.waiting > 0 || r.failed > 0 => {
                            info!(?r, "scan finished")
                        }
                        Ok(_) => {}
                        Err(e) => warn!("scan failed: {e:#}"),
                    }
                    if scan_interval == 0 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(scan_interval)).await;
                }
            });

            let listener = tokio::net::TcpListener::bind(listen).await?;
            info!("listening on http://{listen}");
            axum::serve(
                listener,
                router(state).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        }
    }
    Ok(())
}
