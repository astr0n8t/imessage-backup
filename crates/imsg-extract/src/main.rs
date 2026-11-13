use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use imsg_core::backup::{BackupInfo, default_backup_root, list_backups};
use imsg_extract::{Options, run};

/// Extract Messages and Contacts from iPhone backups made by Finder/iTunes,
/// incrementally, into an archive folder that imsg-server can host.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the backups found on this machine.
    List {
        /// Folder holding backups (default: ~/Library/Application Support/MobileSync/Backup).
        #[arg(long)]
        backup_root: Option<PathBuf>,
    },
    /// Print the structure of an encrypted backup's keybag, for troubleshooting
    /// password errors. Shows tag names, lengths and counts only: no password,
    /// salts or keys, so the output is safe to share.
    Keybag {
        /// A specific backup folder (…/MobileSync/Backup/<UDID>).
        #[arg(short, long, conflicts_with = "udid")]
        backup: Option<PathBuf>,
        /// Pick the backup of this device from --backup-root.
        #[arg(long)]
        udid: Option<String>,
        /// Folder holding backups (default: ~/Library/Application Support/MobileSync/Backup).
        #[arg(long)]
        backup_root: Option<PathBuf>,
    },
    /// Extract new messages, attachments and contacts into the archive.
    Extract {
        /// Archive (or staging) folder to write into.
        #[arg(short, long)]
        out: PathBuf,
        /// A specific backup folder (…/MobileSync/Backup/<UDID>).
        #[arg(short, long, conflicts_with = "udid")]
        backup: Option<PathBuf>,
        /// Pick the backup of this device from --backup-root.
        #[arg(long)]
        udid: Option<String>,
        /// Folder holding backups (default: ~/Library/Application Support/MobileSync/Backup).
        #[arg(long)]
        backup_root: Option<PathBuf>,
        /// Progress file recording where the last extraction stopped
        /// (default: <out>/imsg-state.json).
        #[arg(short, long)]
        state: Option<PathBuf>,
        /// Ignore the state file and export everything.
        #[arg(long)]
        full: bool,
        /// Extract even if the backup has not changed since the last run.
        #[arg(long)]
        force: bool,
        /// Skip copying attachment files.
        #[arg(long)]
        no_attachments: bool,
        /// Environment variable holding the password of an encrypted backup;
        /// you are prompted when it is unset.
        #[arg(long, default_value = "IMSG_BACKUP_PASSWORD")]
        password_env: String,
        /// Print the snapshot metadata as JSON when done.
        #[arg(long)]
        json: bool,
    },
}

fn describe(info: &BackupInfo) -> String {
    format!(
        "{:<28} {:<26} iOS {:<6} {:<10} {}",
        info.udid,
        info.device_name.as_deref().unwrap_or("?"),
        info.product_version.as_deref().unwrap_or("?"),
        if info.encrypted { "encrypted" } else { "plain" },
        info.last_backup_date
            .map(|d| d.to_rfc3339())
            .unwrap_or_default(),
    )
}

/// `--backup`, or the backup of `--udid` (default: the newest) under the backup root.
fn choose_backup(
    backup: Option<PathBuf>,
    udid: Option<&str>,
    backup_root: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(b) = backup {
        return Ok(b);
    }
    let root = backup_root.unwrap_or_else(default_backup_root);
    let all = list_backups(&root)?;
    let chosen = match udid {
        Some(u) => all
            .into_iter()
            .find(|(_, i)| i.udid.eq_ignore_ascii_case(u)),
        None => all.into_iter().next(),
    };
    match chosen {
        Some((path, _)) => Ok(path),
        None => bail!("no matching backup found in {}", root.display()),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::List { backup_root } => {
            let root = backup_root.unwrap_or_else(default_backup_root);
            let backups = list_backups(&root)?;
            if backups.is_empty() {
                println!("No backups found in {}", root.display());
            }
            for (path, info) in backups {
                println!("{}\n    {}", describe(&info), path.display());
            }
        }
        Command::Keybag {
            backup,
            udid,
            backup_root,
        } => {
            let backup = choose_backup(backup, udid.as_deref(), backup_root)?;
            let info = BackupInfo::read(&backup)?;
            println!("{}", describe(&info));
            if !info.encrypted {
                println!("This backup is not encrypted; no password is needed.");
                return Ok(());
            }
            let blob = imsg_core::backup::read_keybag(&backup)?;
            println!("BackupKeyBag: {} bytes", blob.len());
            for line in imsg_core::crypto::Keybag::describe(&blob)? {
                println!("  {line}");
            }
        }
        Command::Extract {
            out,
            backup,
            udid,
            backup_root,
            state,
            full,
            force,
            no_attachments,
            password_env,
            json,
        } => {
            let backup = choose_backup(backup, udid.as_deref(), backup_root)?;
            let info = BackupInfo::read(&backup)?;
            let password = if info.encrypted {
                match std::env::var(&password_env) {
                    Ok(p) => Some(p),
                    Err(_) => Some(rpassword::prompt_password("Backup password: ")?),
                }
            } else {
                None
            };
            let opts = Options {
                backup,
                out,
                state,
                password,
                full,
                force,
                attachments: !no_attachments,
                now: chrono::Utc::now(),
            };
            let report = run(&opts, &mut |line| eprintln!("{line}"))?;
            if let Some(reason) = &report.skipped {
                eprintln!("Nothing to do: {reason}");
            }
            if let Some(dir) = &report.snapshot_dir {
                eprintln!("Wrote snapshot {}", dir.display());
            }
            if report.attachments_copied > 0 {
                eprintln!(
                    "Copied {} attachments ({:.1} MB)",
                    report.attachments_copied,
                    report.attachment_bytes as f64 / 1_048_576.0
                );
            }
            if report.contacts_updated {
                eprintln!("Contacts updated");
            }
            if json && let Some(meta) = &report.snapshot {
                println!("{}", serde_json::to_string_pretty(meta)?);
            }
        }
    }
    Ok(())
}
