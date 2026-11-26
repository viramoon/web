use super::{
    OpsConfig,
    quality::{
        MAX_STREAMS, MarketWatermarks, QUALITY_MAX_BYTES, QualityFile, ReportFile, StreamFile,
        token,
    },
    read_snapshot,
};
use chrono::Utc;
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, de::DeserializeOwned};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{ErrorKind, Write},
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

const INPUTS_MAX_BYTES: u64 = 32 * 1024;
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inputs {
    wal_bin: PathBuf,
    data_root: PathBuf,
    streams: Vec<ApprovedStream>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovedStream {
    source: String,
    product: String,
    license: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Product {
    source: String,
    product: String,
}

fn valid_path(path: &Path) -> bool {
    path.is_absolute()
        && path.file_name().is_some()
        && !path.components().any(|part| part == Component::ParentDir)
}

impl Inputs {
    fn valid(&self) -> bool {
        let mut ids = HashSet::new();
        valid_path(&self.wal_bin)
            && valid_path(&self.data_root)
            && self.streams.len() <= MAX_STREAMS
            && self.streams.iter().all(|stream| {
                token(&stream.source)
                    && token(&stream.product)
                    && token(&stream.license)
                    && ids.insert((&stream.source, &stream.product))
            })
    }

    async fn query<T: DeserializeOwned>(
        &self,
        operation: &str,
        stream: Option<&ApprovedStream>,
    ) -> Result<T, &'static str> {
        let mut command = Command::new(&self.wal_bin);
        command
            .arg("--data-root")
            .arg(&self.data_root)
            .arg(operation);
        if let Some(stream) = stream {
            command.args(["--source", &stream.source, "--product", &stream.product]);
        }
        let mut child = command
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| "could not start the F3 quality tool")?;
        let mut stdout = Vec::new();
        child
            .stdout
            .take()
            .expect("F3 stdout is piped")
            .take(QUALITY_MAX_BYTES + 1)
            .read_to_end(&mut stdout)
            .await
            .map_err(|_| "could not read the F3 quality tool result")?;
        if stdout.len() as u64 > QUALITY_MAX_BYTES {
            return Err("the F3 quality tool result exceeds the size limit");
        }
        if !child
            .wait()
            .await
            .map_err(|_| "could not wait for the F3 quality tool")?
            .success()
        {
            return Err("the F3 quality tool returned a failure");
        }
        serde_json::from_slice(&stdout).map_err(|_| "invalid F3 quality tool result")
    }

    async fn snapshot(&self) -> Result<QualityFile, &'static str> {
        let mut streams = Vec::new();
        let mut encoded_size = 0;
        if !self.streams.is_empty() {
            let available: Vec<Product> = self.query("products", None).await?;
            let eligible: HashSet<_> = available
                .iter()
                .map(|item| (item.source.as_str(), item.product.as_str()))
                .collect();
            for approved in &self.streams {
                if !eligible.contains(&(approved.source.as_str(), approved.product.as_str())) {
                    continue;
                }
                let reports = self
                    .query::<Vec<ReportFile>>("quality", Some(approved))
                    .await?;
                let watermarks = self
                    .query::<MarketWatermarks>("watermarks", Some(approved))
                    .await?;
                let stream = StreamFile {
                    source: approved.source.clone(),
                    product: approved.product.clone(),
                    license: approved.license.clone(),
                    reports,
                    watermarks: Some(watermarks),
                };
                let encoded = serde_json::to_vec(&stream)
                    .map_err(|_| "could not encode the F3 quality stream")?;
                encoded_size += encoded.len() as u64;
                if encoded_size > QUALITY_MAX_BYTES {
                    return Err("the aggregate quality snapshot exceeds the size limit");
                }
                streams.push(stream);
            }
        }
        let snapshot = QualityFile {
            version: 1,
            observed_at: Utc::now(),
            streams,
        };
        if !snapshot.valid() {
            return Err("invalid aggregate quality snapshot");
        }
        Ok(snapshot)
    }
}

impl OpsConfig {
    /// Publish a snapshot from the deployment-approved local F3 WAL streams.
    pub async fn publish_quality(
        &self,
        config_path: &Path,
        static_root: &Path,
    ) -> Result<(), &'static str> {
        self.validate(static_root)
            .map_err(|_| "invalid operations status configuration")?;
        if !valid_path(config_path) {
            return Err("quality input configuration requires an absolute file path");
        }
        let (inputs, _) = read_snapshot::<Inputs>(config_path, INPUTS_MAX_BYTES)
            .filter(|(inputs, _)| inputs.valid())
            .ok_or("invalid quality input configuration")?;
        let status = self
            .quality_status_file
            .as_deref()
            .ok_or("WEB_QUALITY_STATUS_FILE is required to publish a snapshot")?;
        let parent = status
            .parent()
            .expect("validated absolute status file")
            .canonicalize()
            .map_err(|_| "quality status directory is unavailable")?;
        if static_root
            .canonicalize()
            .is_ok_and(|root| parent.starts_with(root))
        {
            return Err("quality status directory must be outside the static root");
        }
        let status = parent.join(status.file_name().expect("validated status file name"));
        if config_path.canonicalize().ok().as_ref() == Some(&status) {
            return Err("quality input and output files must be distinct");
        }
        let directory =
            File::open(&parent).map_err(|_| "quality status directory is unavailable")?;
        // The lock also serializes sampling, so an earlier observation cannot overwrite a newer one.
        flock(&directory, FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| "could not lock the quality status directory")?;
        match fs::symlink_metadata(&status) {
            Ok(metadata) if metadata.is_file() => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            _ => return Err("quality status output requires a regular file"),
        }

        let snapshot = timeout(SAMPLE_TIMEOUT, inputs.snapshot())
            .await
            .map_err(|_| "F3 quality sampling deadline exceeded")??;
        let encoded = serde_json::to_vec(&snapshot)
            .map_err(|_| "could not encode the F3 quality snapshot")?;
        if encoded.len() as u64 > QUALITY_MAX_BYTES {
            return Err("the aggregate quality snapshot exceeds the size limit");
        }
        let mut pending = tempfile::NamedTempFile::new_in(&parent)
            .map_err(|_| "could not prepare the quality snapshot")?;
        pending
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| "could not set quality snapshot permissions")?;
        pending
            .write_all(&encoded)
            .and_then(|_| pending.as_file().sync_all())
            .map_err(|_| "could not write the quality snapshot")?;
        pending
            .persist(status)
            .map_err(|_| "could not publish the quality snapshot")?;
        directory
            .sync_all()
            .map_err(|_| "could not sync the quality status directory")?;
        Ok(())
    }
}
