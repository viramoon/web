use super::{LocalProbeResult, capture, command, memory_file, private_tempdir, read_file};
use rustix::{fs::fstatvfs, process::geteuid};
use serde::{Deserialize, Serialize};
use std::{
    fmt::Write as _,
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    process::Stdio,
};
use tokio::process::Command;
use utoipa::ToSchema;
use uuid::Uuid;
use zeroize::Zeroizing;

const PAYLOAD_BYTES: usize = 96;
const CIPHERTEXT_MAGIC: &[u8] = b"RCLONE\0\0";

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LocalDrivePreview {
    mode: DriveMode,
    encrypted_round_trip: bool,
    cleaned_up: bool,
    #[schema(pattern = "^[0-9]+$")]
    available_bytes: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum DriveMode {
    LocalTest,
}

pub(super) async fn probe(
    binary: &Path,
    root: &Path,
) -> Result<LocalDrivePreview, LocalProbeResult> {
    let directory = private_root(root)?;
    let available_bytes = fstatvfs(&directory)
        .ok()
        .map(|stats| (u128::from(stats.f_bavail) * u128::from(stats.f_frsize)).to_string());
    let scratch =
        private_tempdir(".web-drive-probe-", root).map_err(|_| LocalProbeResult::RemoteFailed)?;
    round_trip(binary, scratch.path()).await?;
    scratch
        .close()
        .map_err(|_| LocalProbeResult::CleanupFailed)?;

    Ok(LocalDrivePreview {
        mode: DriveMode::LocalTest,
        encrypted_round_trip: true,
        cleaned_up: true,
        available_bytes,
    })
}

async fn round_trip(binary: &Path, scratch: &Path) -> Result<(), LocalProbeResult> {
    let storage = scratch.join("storage");
    tokio::fs::create_dir(&storage)
        .await
        .map_err(|_| LocalProbeResult::RemoteFailed)?;
    let storage_path = storage.to_str().ok_or(LocalProbeResult::InvalidFile)?;
    let (password, salt) = tokio::try_join!(obscure(binary, scratch), obscure(binary, scratch))?;
    let config = Zeroizing::new(format!(
        "[web_probe_local]\ntype = local\n\n[web_probe_crypt]\n\
         type = crypt\nremote = web_probe_local:{storage_path}\nfilename_encryption = standard\n\
         directory_name_encryption = true\nno_data_encryption = false\n\
         password = {}\npassword2 = {}\n",
        password.as_str(),
        salt.as_str()
    ));
    let blob = memory_file(config.as_bytes()).map_err(|_| LocalProbeResult::ToolUnavailable)?;

    // A fresh test object, independent of the synthetic crypt password and salt.
    let mut payload = [0_u8; PAYLOAD_BYTES];
    openssl::rand::rand_bytes(&mut payload).map_err(|_| LocalProbeResult::ToolUnavailable)?;
    let input = memory_file(&payload).map_err(|_| LocalProbeResult::ToolUnavailable)?;
    let name = format!("web-probe-{}.bin", Uuid::new_v4());
    let target = format!("web_probe_crypt:{name}");
    rclone(
        crypt_command(binary, scratch, &blob)
            .args(["rcat", "--size"])
            .arg(PAYLOAD_BYTES.to_string())
            .args(["--", &target])
            .stdin(Stdio::from(input)),
        0,
    )
    .await?;
    check_ciphertext(&storage, &name, &payload).await?;
    let downloaded = rclone(
        crypt_command(binary, scratch, &blob).args(["cat", "--", &target]),
        PAYLOAD_BYTES,
    )
    .await?;
    if downloaded != payload {
        return Err(LocalProbeResult::RoundTripMismatch);
    }
    rclone(
        crypt_command(binary, scratch, &blob).args(["deletefile", "--", &target]),
        0,
    )
    .await
    .map_err(|_| LocalProbeResult::CleanupFailed)?;
    let mut remaining = tokio::fs::read_dir(&storage)
        .await
        .map_err(|_| LocalProbeResult::CleanupFailed)?;
    if remaining
        .next_entry()
        .await
        .map_err(|_| LocalProbeResult::CleanupFailed)?
        .is_some()
    {
        return Err(LocalProbeResult::CleanupFailed);
    }
    Ok(())
}

fn private_root(path: &Path) -> Result<File, LocalProbeResult> {
    if path == Path::new("/")
        || !path.to_str().is_some_and(|value| {
            !value
                .chars()
                .any(|c| matches!(c, ':' | '\\' | '\r' | '\n' | '\0'))
        })
        || path.canonicalize().ok().as_deref() != Some(path)
    {
        return Err(LocalProbeResult::InvalidFile);
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| LocalProbeResult::InvalidFile)?;
    let info = file.metadata().map_err(|_| LocalProbeResult::InvalidFile)?;
    if info.uid() != geteuid().as_raw() || info.mode() & 0o7777 != 0o700 {
        return Err(LocalProbeResult::InvalidFile);
    }
    Ok(file)
}

fn base_command(binary: &Path, scratch: &Path) -> Command {
    let mut command = command(binary);
    command
        .args(["--retries", "1", "--low-level-retries", "1", "--cache-dir"])
        .arg(scratch)
        .current_dir(scratch);
    command
}

fn crypt_command(binary: &Path, scratch: &Path, config: &File) -> Command {
    let fd = config.as_raw_fd();
    let mut command = base_command(binary, scratch);
    command.arg("--config").arg(format!("/proc/self/fd/{fd}"));
    // SAFETY: fcntl is async-signal-safe. The config is kept live by the caller
    // and stays close-on-exec in the parent and every unrelated child process.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

async fn obscure(binary: &Path, scratch: &Path) -> Result<Zeroizing<String>, LocalProbeResult> {
    let mut random = Zeroizing::new([0_u8; 32]);
    openssl::rand::rand_bytes(random.as_mut()).map_err(|_| LocalProbeResult::ToolUnavailable)?;
    let mut plaintext = Zeroizing::new(String::with_capacity(65));
    for byte in random.iter() {
        write!(&mut *plaintext, "{byte:02x}").expect("writing into a string");
    }
    plaintext.push('\n');
    let input = memory_file(plaintext.as_bytes()).map_err(|_| LocalProbeResult::ToolUnavailable)?;
    let bytes = Zeroizing::new(
        rclone(
            base_command(binary, scratch)
                .args(["--config", "/dev/null", "obscure", "-"])
                .stdin(Stdio::from(input)),
            256,
        )
        .await?,
    );
    let value = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|value| value.strip_suffix('\n'))
        .filter(|value| {
            (32..=255).contains(&value.len())
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        })
        .ok_or(LocalProbeResult::InvalidProducerResponse)?;
    Ok(Zeroizing::new(value.into()))
}

async fn rclone(command: &mut Command, limit: usize) -> Result<Vec<u8>, LocalProbeResult> {
    let (status, output) = capture(command, limit).await?;
    if !status.success() {
        return Err(LocalProbeResult::RemoteFailed);
    }
    Ok(output)
}

async fn check_ciphertext(
    storage: &Path,
    name: &str,
    payload: &[u8],
) -> Result<(), LocalProbeResult> {
    let mismatch = LocalProbeResult::RoundTripMismatch;
    let mut entries = tokio::fs::read_dir(storage).await.map_err(|_| mismatch)?;
    let entry = entries
        .next_entry()
        .await
        .map_err(|_| mismatch)?
        .ok_or(mismatch)?;
    if entry
        .file_name()
        .as_encoded_bytes()
        .windows(name.len())
        .any(|part| part == name.as_bytes())
        || entries.next_entry().await.map_err(|_| mismatch)?.is_some()
    {
        return Err(mismatch);
    }
    let ciphertext = read_file(&entry.path(), 1024).await.map_err(|_| mismatch)?;
    if !ciphertext.starts_with(CIPHERTEXT_MAGIC)
        || ciphertext.len() <= payload.len()
        || ciphertext
            .windows(payload.len())
            .any(|part| part == payload)
    {
        return Err(mismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
