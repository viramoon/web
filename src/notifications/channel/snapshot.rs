//! Private, immutable SMTP configuration captures. PostgreSQL stores only the opaque ID.
use super::{ChannelConfigError, Credentials, EmailChannel};
use openssl::{
    rand::rand_bytes,
    symm::{Cipher, decrypt_aead, encrypt_aead},
};
use rustix::{
    fs::{AtFlags, Dir, Mode, OFlags, RenameFlags, openat, renameat_with, unlinkat},
    process::geteuid,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions, Permissions},
    io::{self, ErrorKind, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;
use zeroize::Zeroizing;

const KEY_FILE: &str = "snapshot.key";
const HEADER: &[u8] = b"web.smtp.snapshot.v1\0";
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const MAX_SNAPSHOT_BYTES: usize = 8192;
const INVALID_DIRECTORY: ChannelConfigError =
    ChannelConfigError("SMTP snapshot directory must be an owned absolute 0700 directory");
const INVALID_KEY: ChannelConfigError =
    ChannelConfigError("SMTP snapshot key is missing, invalid or unavailable");

pub(super) struct SnapshotStore {
    directory: File,
    key: Zeroizing<[u8; 32]>,
}

#[derive(Serialize)]
struct Capture<'a> {
    relay: &'a str,
    from: &'a str,
    to: &'a str,
    alerts_url: Option<&'a str>,
    credentials: Option<&'a Credentials>,
    transfer_seconds: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recorded {
    relay: String,
    from: String,
    to: String,
    alerts_url: Option<String>,
    credentials: Option<Credentials>,
    transfer_seconds: u64,
}

impl SnapshotStore {
    pub(super) fn open(path: &Path) -> Result<Self, ChannelConfigError> {
        if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
            return Err(INVALID_DIRECTORY);
        }
        let path: PathBuf = path.components().collect();
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|_| INVALID_DIRECTORY)?;
        check_mode(&directory, true, 0o700).map_err(|_| INVALID_DIRECTORY)?;
        let key = load_key(&directory).map_err(|_| INVALID_KEY)?;
        Ok(Self { directory, key })
    }

    pub(super) fn save(&self, channel: &EmailChannel) -> io::Result<Uuid> {
        check_mode(&self.directory, true, 0o700)?;
        let id = Uuid::new_v4();
        let mut plaintext = Zeroizing::new(Vec::with_capacity(MAX_SNAPSHOT_BYTES));
        serde_json::to_writer(
            &mut *plaintext,
            &Capture {
                relay: &channel.relay,
                from: &channel.from,
                to: &channel.to,
                alerts_url: channel.alerts_url.as_deref(),
                credentials: channel.credentials.as_ref(),
                transfer_seconds: channel.transfer_timeout.as_secs(),
            },
        )?;
        let mut nonce = [0; NONCE_BYTES];
        let mut tag = [0; TAG_BYTES];
        rand_bytes(&mut nonce)?;
        let aad = [HEADER, id.as_bytes().as_slice()].concat();
        let ciphertext = encrypt_aead(
            Cipher::aes_256_gcm(),
            &*self.key,
            Some(&nonce),
            &aad,
            &plaintext,
            &mut tag,
        )?;
        let envelope = [HEADER, &nonce, &tag, &ciphertext].concat();
        if envelope.len() > MAX_SNAPSHOT_BYTES {
            return Err(invalid_data());
        }
        write_private(&self.directory, &format!("{id}.smtp"), &envelope)?;
        Ok(id)
    }

    pub(super) fn load(&self, id: Uuid) -> io::Result<EmailChannel> {
        check_mode(&self.directory, true, 0o700)?;
        let envelope = read_private(&self.directory, &format!("{id}.smtp"), MAX_SNAPSHOT_BYTES)?;
        let prefix_bytes = HEADER.len() + NONCE_BYTES + TAG_BYTES;
        if !envelope.starts_with(HEADER) || envelope.len() <= prefix_bytes {
            return Err(invalid_data());
        }
        let nonce_end = HEADER.len() + NONCE_BYTES;
        let aad = [HEADER, id.as_bytes().as_slice()].concat();
        let plaintext = Zeroizing::new(decrypt_aead(
            Cipher::aes_256_gcm(),
            &*self.key,
            Some(&envelope[HEADER.len()..nonce_end]),
            &aad,
            &envelope[prefix_bytes..],
            &envelope[nonce_end..prefix_bytes],
        )?);
        let record: Recorded = serde_json::from_slice(&plaintext)?;
        if !(1..=60).contains(&record.transfer_seconds) {
            return Err(invalid_data());
        }
        Ok(EmailChannel {
            relay: record.relay,
            from: record.from,
            to: record.to,
            alerts_url: record.alerts_url,
            credentials: record.credentials,
            transfer_timeout: Duration::from_secs(record.transfer_seconds),
        })
    }
}

fn load_key(directory: &File) -> io::Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0; 32]);
    let read = || read_private(directory, KEY_FILE, 32);
    let bytes = match read() {
        // Read the winning file below even if a different replica published it first.
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let mut has_snapshot = false;
            for entry in Dir::read_from(directory)? {
                if entry?.file_name().to_bytes().ends_with(b".smtp") {
                    has_snapshot = true;
                    break;
                }
            }
            // Never replace a missing key if encrypted captures may still need it.
            if !has_snapshot {
                rand_bytes(&mut *key)?;
                match write_private(directory, KEY_FILE, &*key) {
                    Err(error) if error.kind() != ErrorKind::AlreadyExists => return Err(error),
                    _ => {}
                }
            }
            read()?
        }
        Ok(bytes) => bytes,
        Err(error) => return Err(error),
    };
    if bytes.len() != key.len() {
        return Err(invalid_data());
    }
    key.copy_from_slice(&bytes);
    directory.sync_all()?;
    Ok(key)
}

fn read_private(directory: &File, name: &str, limit: usize) -> io::Result<Zeroizing<Vec<u8>>> {
    let file = File::from(openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    check_mode(&file, false, 0o600)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(limit + 1));
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(invalid_data());
    }
    Ok(bytes)
}

fn write_private(directory: &File, destination: &str, bytes: &[u8]) -> io::Result<()> {
    let temporary = format!(".{}.tmp", Uuid::new_v4());
    let mut file = File::from(openat(
        directory,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    let result = (|| {
        file.set_permissions(Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        renameat_with(
            directory,
            temporary.as_str(),
            directory,
            destination,
            RenameFlags::NOREPLACE,
        )?;
        directory.sync_all()
    })();
    if result.is_err() {
        let _ = unlinkat(directory, temporary.as_str(), AtFlags::empty());
    }
    result
}

fn check_mode(file: &File, directory: bool, mode: u32) -> io::Result<()> {
    let meta = file.metadata()?;
    let expected_type = if directory {
        meta.is_dir()
    } else {
        meta.is_file()
    };
    if expected_type && meta.mode() & 0o7777 == mode && meta.uid() == geteuid().as_raw() {
        Ok(())
    } else {
        Err(invalid_data())
    }
}

fn invalid_data() -> io::Error {
    io::Error::from(ErrorKind::InvalidData)
}
