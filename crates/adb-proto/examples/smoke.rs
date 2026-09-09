//! Smoke-test the protocol implementation against a real device.
//! Usage: cargo run -p adb-proto --example smoke -- [remote-path]

use adb_proto::{shell, AdbClient, DeviceSelector, SyncSession};
use sha2::{Digest, Sha256};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/sdcard".to_string());

    let client = AdbClient::default();
    let sel = DeviceSelector::Any;

    println!("server version : {:#x}", client.server_version().await?);
    let device = client.resolve(&sel).await?;
    println!(
        "device         : {} ({})",
        device.serial,
        device.model.as_deref().unwrap_or("?")
    );

    let features = client.features(&sel).await?;
    for f in ["stat_v2", "ls_v2", "sendrecv_v2", "sendrecv_v2_zstd"] {
        println!("feature {f:<18}: {}", features.contains(f));
    }

    let out = shell::run(&client, &sel, "echo out; echo err >&2; exit 7").await?;
    println!(
        "shell v2       : stdout={:?} stderr={:?} exit={}",
        out.stdout_text().trim(),
        out.stderr_text().trim(),
        out.exit_code
    );

    let mut sync = SyncSession::open(&client, &sel).await?;
    let st = sync.stat(&path).await?;
    println!(
        "stat {path}: mode={:o} size={} mtime={} dir={} file={}",
        st.mode,
        st.size,
        st.mtime,
        st.is_dir(),
        st.is_file()
    );

    let entries = sync.list(&path).await?;
    println!("list {path}: {} entries", entries.len());
    for e in entries.iter().take(3) {
        println!(
            "  {:<28} size={:<10} mtime={} dir={}",
            e.name,
            e.stat.size,
            e.stat.mtime,
            e.stat.is_dir()
        );
    }

    let lst = sync.lstat("/sdcard").await?;
    println!(
        "lstat /sdcard  : mode={:o} symlink={} dir={}",
        lst.mode,
        lst.is_symlink(),
        lst.is_dir()
    );

    // RECV against a scratch file whose digest the device can compute for us.
    let probe = "/data/local/tmp/adbrsync-smoke.bin";
    shell::run(
        &client,
        &sel,
        &format!("dd if=/dev/urandom of={probe} bs=64k count=17 2>/dev/null"),
    )
    .await?;
    let mut buf: Vec<u8> = Vec::new();
    let n = sync.recv(probe, &mut buf).await?;
    let device_sum = shell::run(
        &client,
        &sel,
        &format!("sha256sum {}", shell::shell_quote(probe)),
    )
    .await?;
    let device_sum = device_sum
        .stdout_text()
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();
    let local_sum = format!("{:x}", <Sha256 as Digest>::digest(&buf));
    println!("recv {probe}: {n} bytes");
    println!("  device sha256 : {device_sum}");
    println!("  local  sha256 : {local_sum}");
    println!("  match         : {}", device_sum == local_sum);

    shell::run(&client, &sel, &format!("rm -f {probe}")).await?;

    sync.quit().await?;
    Ok(())
}
