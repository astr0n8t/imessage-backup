//! Write a synthetic iPhone backup for trying out imsg-extract / imsg-server.
//!
//! cargo run -p imsg-core --example sample_backup -- <dir> [stage 1|2] [password]

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = args
        .get(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "sample-backup".into());
    let stage: u8 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    let password = args.get(3).map(String::as_str);
    let date = chrono::Utc::now();
    let _ = std::fs::remove_dir_all(&dir);
    imsg_core::fixture::build_sample_backup(
        &dir,
        "00008110-SAMPLE0000000001",
        password,
        stage,
        date,
    )?;
    println!("wrote stage {stage} sample backup to {}", dir.display());
    Ok(())
}
