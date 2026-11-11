use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use imsg_server::auth::{Accounts, hash_password};
use imsg_server::index::Index;
use imsg_server::media::Tools;
use imsg_server::totp::{self, Secret};
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
        /// Accounts file (TOML `[[account]]` entries; see README). The older
        /// `username:argon2hash` line format is also accepted.
        #[arg(long, visible_alias = "accounts", env = "IMSG_AUTH_FILE")]
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
        /// Refuse to start unless every account has a `totp_secret`.
        #[arg(long, env = "IMSG_REQUIRE_TOTP")]
        require_totp: bool,
        /// Days a login stays valid.
        #[arg(long, default_value_t = 30)]
        session_days: u64,
    },
    /// Ingest new snapshots into the index once and exit.
    Ingest {
        #[command(flatten)]
        paths: Paths,
    },
    /// Generate a TOTP (authenticator app) secret for an account: shows a QR
    /// code to scan, checks a code from the app, and prints the
    /// `totp_secret = "..."` line to add to that account in the accounts file.
    TotpSecret {
        /// Account name (shown in the authenticator app).
        #[arg(short, long)]
        user: String,
        /// Issuer label shown in the authenticator app.
        #[arg(long, default_value = "Messages Archive")]
        issuer: String,
        /// Don't ask for a code to confirm the app was set up correctly.
        #[arg(long)]
        no_verify: bool,
        /// Draw the QR code for a light terminal background.
        #[arg(long)]
        invert: bool,
    },
    /// List each archived device and the iMessage IDs / numbers its messages
    /// used (ingesting new snapshots first), to help write the accounts file.
    Identities {
        #[command(flatten)]
        paths: Paths,
    },
    /// Hash a password and print an `[[account]]` entry for the accounts file.
    HashPassword {
        /// Account name.
        #[arg(short, long, default_value = "admin")]
        user: String,
        /// Read the password from the first line of stdin instead of prompting.
        #[arg(long)]
        stdin: bool,
        /// Print a legacy `name:hash` line instead (full access, admin).
        #[arg(long)]
        line: bool,
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
        Command::HashPassword { user, stdin, line } => {
            if user.is_empty()
                || user.contains([':', '"', '\\'])
                || user.contains(char::is_whitespace)
            {
                bail!(
                    "account names cannot be empty or contain spaces, quotes, backslashes or ':'"
                );
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
            let hash = hash_password(&p1)?;
            if line {
                println!("{user}:{hash}");
            } else {
                println!(
                    "[[account]]\nname = \"{user}\"\npassword = \"{hash}\"\n\
                     # Device UDIDs whose backups this account sees (\"*\" = all); see `imsg-server identities`.\n\
                     devices = []\n\
                     # This person's phone numbers / Apple IDs; their messages from any device are visible.\n\
                     imessage_ids = []\n\
                     admin = false\n\
                     # Optional two-factor authentication: `imsg-server totp-secret --user {user}`\n\
                     # totp_secret = \"...\"\n"
                );
            }
        }
        Command::TotpSecret {
            user,
            issuer,
            no_verify,
            invert,
        } => {
            let secret = Secret::generate();
            let uri = secret.uri(&issuer, &user);
            eprintln!("Scan this QR code with your authenticator app:\n");
            eprintln!("{}", totp::qr_text(&uri, invert)?);
            eprintln!("(If it won't scan, try --invert, or add the key below by hand.)");
            eprintln!("Or enter this key manually: {}\n", secret.to_base32());
            if !no_verify {
                loop {
                    eprint!("Enter the 6-digit code the app shows (blank to skip): ");
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    let code = line.trim();
                    if code.is_empty() {
                        eprintln!("Skipped verification.");
                        break;
                    }
                    if secret.matching_step(code, totp::now()).is_some() {
                        eprintln!("Code accepted.");
                        break;
                    }
                    eprintln!(
                        "That code doesn't match. Check the app and this machine's clock, then try again."
                    );
                }
            }
            eprintln!(
                "\nAdd this line to the [[account]] entry for {user:?}, then restart the server:\n"
            );
            println!("totp_secret = \"{}\"", secret.to_base32());
        }
        Command::Identities { paths } => {
            let index = Index::open(&paths.state_dir().join("index.sqlite"))?;
            let data = paths.data.clone();
            let ids = tokio::task::spawn_blocking(move || -> Result<_> {
                index.ingest_all(&data)?;
                imsg_server::queries::identities(&index.read()?)
            })
            .await??;
            if ids.is_empty() {
                println!("No devices ingested yet.");
            }
            for d in ids {
                println!(
                    "{}  {}",
                    d.device,
                    d.device_name.as_deref().unwrap_or("(unnamed device)")
                );
                for (id, n) in d.imessage_ids {
                    let id = if id.is_empty() {
                        "(unknown)".to_owned()
                    } else {
                        id
                    };
                    println!("    {id:<36} {n} messages");
                }
            }
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
            require_totp,
            session_days,
        } => {
            let accounts = Accounts::load(&auth_file)?;
            if accounts.is_empty() {
                bail!(
                    "{} contains no accounts; add one with `imsg-server hash-password`",
                    auth_file.display()
                );
            }
            let without: Vec<&str> = accounts
                .iter()
                .filter(|a| a.totp.is_none())
                .map(|a| a.name.as_str())
                .collect();
            if require_totp && !without.is_empty() {
                bail!(
                    "--require-totp is set but these accounts have no totp_secret: {} (create one with `imsg-server totp-secret --user <name>`)",
                    without.join(", ")
                );
            }
            if !without.is_empty() {
                info!(
                    "accounts without two-factor authentication: {}",
                    without.join(", ")
                );
            }
            if !paths.data.is_dir() {
                bail!("data folder {} does not exist", paths.data.display());
            }
            let state_dir = paths.state_dir();
            let index = Index::open(&state_dir.join("index.sqlite")).context("opening index")?;
            let tools = Tools::detect();
            info!(accounts = accounts.len(), transcoding = %tools.describe(), assets = imsg_server::web::asset_version(), "starting");
            let state = Arc::new(AppState::new(
                index,
                paths.data.clone(),
                state_dir.join("cache"),
                accounts,
                tools,
                secure_cookies,
                trust_forwarded_for,
                Duration::from_secs(session_days * 86_400),
            ));

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
