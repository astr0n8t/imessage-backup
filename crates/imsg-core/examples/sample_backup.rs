//! Write a synthetic iPhone backup for trying out imsg-extract / imsg-server.
//!
//! cargo run -p imsg-core --example sample_backup -- <dir> [stage 1|2] [password|-] [owner number]

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = args
        .get(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "sample-backup".into());
    let stage: u8 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    let password = args.get(3).map(String::as_str).filter(|p| *p != "-");
    let owner = args
        .get(4)
        .map(String::as_str)
        .unwrap_or(imsg_core::fixture::SAMPLE_OWNER);
    let udid = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    imsg_core::fixture::build_sample_backup_for(
        &dir,
        &udid,
        password,
        stage,
        chrono::Utc::now(),
        owner,
    )?;
    println!(
        "wrote stage {stage} sample backup of {owner}'s phone ({udid}) to {}",
        dir.display()
    );
    Ok(())
}
