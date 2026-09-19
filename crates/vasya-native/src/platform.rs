use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use tokio::{io::AsyncWriteExt, sync::watch};

pub fn open_file(path: PathBuf) {
    tokio::spawn(async move {
        #[cfg(target_os = "macos")]
        let _ = tokio::process::Command::new("/usr/bin/open")
            .arg(path)
            .status()
            .await;
        #[cfg(not(target_os = "macos"))]
        let _ = tokio::process::Command::new("xdg-open")
            .arg(path)
            .status()
            .await;
    });
}
pub fn notify(sender: String, text: String, sound: bool, preview: bool) {
    tokio::spawn(async move {
        let body = if preview {
            format!("{sender}: {}", text.chars().take(180).collect::<String>())
        } else {
            "New message".into()
        };
        #[cfg(target_os = "macos")]
        let _=tokio::process::Command::new("/usr/bin/osascript").arg("-e")
            .arg("on run argv\nif item 2 of argv is \"true\" then\ndisplay notification (item 1 of argv) with title \"Vasya\" sound name \"Glass\"\nelse\ndisplay notification (item 1 of argv) with title \"Vasya\"\nend if\nend run")
            .arg(body).arg(sound.to_string()).status().await;
    });
}

pub async fn capture(
    voice: bool,
    dir: PathBuf,
    mut stop: watch::Receiver<bool>,
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(&dir).await?;
    let file = dir.join(format!(
        "capture-{}.{}",
        chrono::Utc::now().timestamp_millis(),
        if voice { "m4a" } else { "jpg" }
    ));
    let exe = std::env::current_exe()?;
    let parent = exe.parent().context("Executable directory unavailable")?;
    let candidates = [
        parent.join("vasya-capture"),
        parent.join("../Resources/vasya-capture"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/native-tools/vasya-capture"),
    ];
    let helper = candidates
        .iter()
        .find(|p| p.is_file())
        .context("Camera/audio helper unavailable. Build with scripts/build-macos.sh.")?;
    let mut child = tokio::process::Command::new(helper)
        .arg(if voice { "record" } else { "photo" })
        .arg(&file)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if voice {
        tokio::select! { _=stop.changed()=>{}, _=tokio::time::sleep(std::time::Duration::from_secs(300))=>{} }
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(b"stop\n").await?;
        }
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(40), child.wait_with_output())
        .await??;
    if !output.status.success() {
        bail!(
            "Capture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if !file.is_file() {
        bail!("Capture produced no file");
    }
    Ok(file)
}
