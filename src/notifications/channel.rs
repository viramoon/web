mod snapshot;

use chrono::Utc;
use rustix::{
    fs::{MemfdFlags, memfd_create},
    process::geteuid,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, Write},
    net::IpAddr,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::Path,
    process::{Output, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{process::Command, time::timeout};
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::model::Condition;
use snapshot::SnapshotStore;

const CURL_BIN: &str = "/usr/bin/curl";
const TRANSFER_GRACE: Duration = Duration::from_secs(1);
const SNAPSHOT_LOAD_DEADLINE: Duration = Duration::from_secs(2);
pub(super) const PRIMARY_REQUIRED: ChannelConfigError =
    ChannelConfigError("a usable SMTP primary profile is required");
const SMTP_ENV: [&str; 6] = [
    "WEB_SMTP_PORT",
    "WEB_SMTP_TLS",
    "WEB_SMTP_FROM",
    "WEB_SMTP_TO",
    "WEB_SMTP_CREDENTIAL_FILE",
    "WEB_SMTP_TIMEOUT_SECONDS",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Accepted,
    Timeout,
    Transient,
    Rejected,
    ConfigurationError,
    /// A test could not load its already-bound SMTP configuration.
    SnapshotUnavailable,
}

#[derive(Clone, Copy, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum SmtpProfile {
    Primary,
    Secondary,
}

/// The entire input to the alert template. Market data and producer strings have no path into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailKind {
    Alert { condition: Condition },
    Reminder { condition: Condition },
    Recovery { condition: Condition },
    Digest { count: u64 },
    TransportRecovered { count: u64 },
}

pub trait NotificationChannel: Sync + Send {
    fn validate(&self) -> Result<(), ChannelConfigError>;
    fn test(&self, id: Uuid) -> impl Future<Output = DeliveryOutcome> + Send;
    fn send(&self, id: Uuid, mail: MailKind) -> impl Future<Output = DeliveryOutcome> + Send;

    fn primary_available(&self) -> bool {
        true
    }

    fn snapshot_storage_available(&self) -> bool {
        true
    }

    fn prepare_snapshot(
        &self,
        snapshot: Uuid,
    ) -> impl Future<Output = Option<PreparedSnapshot>> + Send {
        async move {
            self.snapshot_storage_available()
                .then_some(PreparedSnapshot::ById(snapshot))
        }
    }

    fn capture_profile(&self, _profile: SmtpProfile) -> impl Future<Output = Option<Uuid>> + Send {
        async { None }
    }

    fn test_snapshot(
        &self,
        _snapshot: Uuid,
        _id: Uuid,
    ) -> impl Future<Output = DeliveryOutcome> + Send {
        async { DeliveryOutcome::ConfigurationError }
    }

    fn send_snapshot(
        &self,
        _snapshot: Uuid,
        _id: Uuid,
        _mail: MailKind,
    ) -> impl Future<Output = DeliveryOutcome> + Send {
        async { DeliveryOutcome::ConfigurationError }
    }
}

pub enum PreparedSnapshot {
    ById(Uuid),
    Loaded(EmailChannel),
}

impl PreparedSnapshot {
    pub(super) async fn test(
        &self,
        channel: &impl NotificationChannel,
        id: Uuid,
    ) -> DeliveryOutcome {
        match self {
            Self::ById(snapshot) => channel.test_snapshot(*snapshot, id).await,
            Self::Loaded(snapshot) => snapshot.test(id).await,
        }
    }

    pub(super) async fn send(
        &self,
        channel: &impl NotificationChannel,
        id: Uuid,
        mail: MailKind,
    ) -> DeliveryOutcome {
        match self {
            Self::ById(snapshot) => channel.send_snapshot(*snapshot, id, mail).await,
            Self::Loaded(snapshot) => snapshot.send(id, mail).await,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelConfigError(&'static str);

impl ChannelConfigError {
    pub fn reason(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ChannelConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ChannelConfigError {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    #[serde(
        deserialize_with = "protected_string",
        serialize_with = "serialize_secret"
    )]
    username: Zeroizing<String>,
    #[serde(
        deserialize_with = "protected_string",
        serialize_with = "serialize_secret"
    )]
    password: Zeroizing<String>,
}

fn protected_string<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Zeroizing<String>, D::Error> {
    String::deserialize(deserializer).map(Zeroizing::new)
}

fn serialize_secret<S: Serializer>(
    value: &Zeroizing<String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(value)
}

impl Credentials {
    fn load(path: &Path) -> Result<Self, ChannelConfigError> {
        const MAX_FILE: u64 = 8192;
        const INVALID_FILE: ChannelConfigError =
            ChannelConfigError("SMTP credential file must be an owned absolute regular 0600 file");
        const INVALID_CONTENT: ChannelConfigError =
            ChannelConfigError("SMTP credential file must contain valid username and password");
        if !path.is_absolute() {
            return Err(INVALID_FILE);
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| INVALID_FILE)?;
        let metadata = file.metadata().map_err(|_| INVALID_FILE)?;
        if !metadata.is_file()
            || metadata.mode() & 0o7777 != 0o600
            || metadata.uid() != geteuid().as_raw()
        {
            return Err(INVALID_FILE);
        }

        let mut content = Zeroizing::new(Vec::new());
        file.take(MAX_FILE + 1)
            .read_to_end(&mut content)
            .map_err(|_| INVALID_CONTENT)?;
        if content.len() as u64 > MAX_FILE {
            return Err(INVALID_CONTENT);
        }
        let credentials: Self = serde_json::from_slice(&content).map_err(|_| INVALID_CONTENT)?;
        let printable = |value: &str, max| {
            (1..=max).contains(&value.len())
                && value.bytes().all(|byte| (b' '..=b'~').contains(&byte))
        };
        if !printable(&credentials.username, 512)
            || credentials.username.contains(':')
            || !printable(&credentials.password, 2048)
        {
            return Err(INVALID_CONTENT);
        }
        Ok(credentials)
    }

    fn curl_config(&self) -> Zeroizing<String> {
        let mut config = Zeroizing::new(String::with_capacity(
            2 * (self.username.len() + self.password.len()) + 12,
        ));
        config.push_str("user = \"");
        for character in self
            .username
            .chars()
            .chain([':'])
            .chain(self.password.chars())
        {
            if matches!(character, '\\' | '"') {
                config.push('\\');
            }
            config.push(character);
        }
        config.push_str("\"\n");
        config
    }
}

pub struct EmailChannel {
    relay: String,
    from: String,
    to: String,
    alerts_url: Option<String>,
    credentials: Option<Credentials>,
    transfer_timeout: Duration,
}

fn dns_name(value: &str) -> bool {
    value.len() <= 253
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn mailbox(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    value.len() <= 254
        && local.len() <= 64
        && local.split('.').all(|atom| {
            !atom.is_empty()
                && atom.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~".contains(&byte)
                })
        })
        && dns_name(domain)
}

impl EmailChannel {
    /// Load deployment-owned settings. Absence of all SMTP settings disables the email channel.
    pub fn from_environment(
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, ChannelConfigError> {
        let Some(host) = get("WEB_SMTP_HOST") else {
            if SMTP_ENV.into_iter().any(|key| get(key).is_some()) {
                return Err(ChannelConfigError(
                    "WEB_SMTP_HOST is required when SMTP is configured",
                ));
            }
            return Ok(None);
        };
        let host = match host.parse::<IpAddr>() {
            Ok(IpAddr::V6(address)) => format!("[{address}]"),
            Ok(IpAddr::V4(address)) => address.to_string(),
            Err(_) if dns_name(&host) => host,
            Err(_) => return Err(ChannelConfigError("invalid WEB_SMTP_HOST")),
        };
        let (scheme, default_port) = match get("WEB_SMTP_TLS").as_deref() {
            None | Some("implicit") => ("smtps", 465),
            Some("starttls") => ("smtp", 587),
            _ => {
                return Err(ChannelConfigError(
                    "WEB_SMTP_TLS must be implicit or starttls",
                ));
            }
        };
        let port = get("WEB_SMTP_PORT")
            .map_or(Some(default_port), |value| value.parse::<u16>().ok())
            .filter(|port| *port != 0)
            .ok_or(ChannelConfigError("invalid WEB_SMTP_PORT"))?;
        let address = |key, error| {
            get(key)
                .filter(|value| mailbox(value))
                .ok_or(ChannelConfigError(error))
        };
        let from = address("WEB_SMTP_FROM", "invalid WEB_SMTP_FROM mailbox")?;
        let to = address("WEB_SMTP_TO", "invalid WEB_SMTP_TO mailbox")?;
        let transfer_seconds = get("WEB_SMTP_TIMEOUT_SECONDS")
            .map_or(Some(10), |value| value.parse::<u64>().ok())
            .filter(|value| (1..=60).contains(value))
            .ok_or(ChannelConfigError(
                "WEB_SMTP_TIMEOUT_SECONDS must be between 1 and 60",
            ))?;
        let alerts_url = get("WEB_PUBLIC_ORIGIN")
            .map(|origin| {
                let invalid_origin =
                    ChannelConfigError("WEB_PUBLIC_ORIGIN must be a canonical HTTPS origin");
                let url = Url::parse(&origin).map_err(|_| invalid_origin)?;
                if url.scheme() != "https" || url.origin().ascii_serialization() != origin {
                    return Err(invalid_origin);
                }
                Ok(format!("{origin}/alerts"))
            })
            .transpose()?;
        let credentials = get("WEB_SMTP_CREDENTIAL_FILE")
            .map(|path| Credentials::load(Path::new(&path)))
            .transpose()?;
        let channel = Self {
            relay: format!("{scheme}://{host}:{port}"),
            from,
            to,
            alerts_url,
            credentials,
            transfer_timeout: Duration::from_secs(transfer_seconds),
        };
        channel.validate()?;
        Ok(Some(channel))
    }

    pub fn timeout(&self) -> Duration {
        self.transfer_timeout + TRANSFER_GRACE
    }

    fn message(&self, id: Uuid, kind: MessageKind) -> String {
        let (subject, description) = kind.content();
        let date = Utc::now().to_rfc2822();
        let alerts_url = self.alerts_url.as_deref().unwrap_or("/alerts");
        format!(
            "From: <{}>\r\nTo: <{}>\r\nSubject: {subject}\r\nDate: {date}\r\n\
             Message-ID: <{id}@web-notifier.invalid>\r\nMIME-Version: 1.0\r\n\
             Content-Type: text/plain; charset=us-ascii\r\n\r\n\
             {description}\r\n\r\nOpen {alerts_url} in your private Fertile terminal.\r\n",
            self.from, self.to,
        )
    }

    async fn deliver_with(&self, command: Command, id: Uuid, kind: MessageKind) -> DeliveryOutcome {
        let message = self.message(id, kind);
        let Ok((mut command, _upload)) = self.configure_command(command, &message) else {
            return DeliveryOutcome::ConfigurationError;
        };
        let Ok(child) = command.spawn() else {
            return DeliveryOutcome::ConfigurationError;
        };
        match timeout(self.timeout(), child.wait_with_output()).await {
            Ok(Ok(output)) => classify(&output),
            Ok(Err(_)) => DeliveryOutcome::ConfigurationError,
            Err(_) => DeliveryOutcome::Timeout,
        }
    }

    fn configure_command(
        &self,
        mut command: Command,
        message: &str,
    ) -> io::Result<(Command, File)> {
        let upload = anonymous_file("web-notifier-mail", message.as_bytes())?;
        let upload_fd = upload.as_raw_fd();
        // curl requires this option first to ignore its default config.
        command.arg("--disable");
        let stdin = match &self.credentials {
            Some(credentials) => {
                let config = credentials.curl_config();
                command.args(["--config", "-"]);
                Stdio::from(anonymous_file("web-notifier-smtp-auth", config.as_bytes())?)
            }
            None => Stdio::null(),
        };
        command
            .args([
                "--silent",
                "--globoff",
                "--proto",
                "=smtp,smtps",
                "--ssl-reqd",
                "--tlsv1.2",
            ])
            .arg("--max-time")
            .arg(self.transfer_timeout.as_secs().to_string())
            .args([
                "--url",
                &self.relay,
                "--mail-from",
                &self.from,
                "--mail-rcpt",
                &self.to,
            ])
            .arg("--upload-file")
            .arg(format!("/proc/self/fd/{upload_fd}"))
            .args(["--output", "/dev/null", "--write-out", "%{response_code}"])
            .env_clear()
            .kill_on_drop(true)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        // SAFETY: the child executes only async-signal-safe fcntl on the inherited descriptor.
        // The returned File keeps it open until the command has spawned. In the parent the
        // descriptor remains close-on-exec, so unrelated subprocesses cannot inherit it.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(upload_fd, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok((command, upload))
    }

    async fn deliver(&self, id: Uuid, kind: MessageKind) -> DeliveryOutcome {
        self.deliver_with(Command::new(CURL_BIN), id, kind).await
    }
}

impl NotificationChannel for EmailChannel {
    fn validate(&self) -> Result<(), ChannelConfigError> {
        validate_transport()
    }

    fn snapshot_storage_available(&self) -> bool {
        false
    }

    async fn test(&self, id: Uuid) -> DeliveryOutcome {
        self.deliver(id, MessageKind::Test).await
    }

    async fn send(&self, id: Uuid, mail: MailKind) -> DeliveryOutcome {
        self.deliver(id, MessageKind::Notification(mail)).await
    }
}

fn validate_transport() -> Result<(), ChannelConfigError> {
    let binary = std::fs::metadata(CURL_BIN)
        .map_err(|_| ChannelConfigError("SMTP transport requires /usr/bin/curl"))?;
    if !binary.is_file() || binary.permissions().mode() & 0o111 == 0 {
        return Err(ChannelConfigError(
            "SMTP transport requires executable /usr/bin/curl",
        ));
    }
    Ok(())
}

pub struct EmailChannels {
    primary: Option<Arc<EmailChannel>>,
    secondary: Option<Arc<EmailChannel>>,
    snapshots: Option<Arc<SnapshotStore>>,
    #[cfg(test)]
    test_ca_file: Option<std::path::PathBuf>,
}

impl EmailChannels {
    /// Without a primary slot, only previously provisioned snapshots can serve the worker.
    pub fn for_notifier(get: impl Fn(&str) -> Option<String>) -> Result<Self, ChannelConfigError> {
        Self::load_environment(get, true)
    }

    pub fn from_environment(
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ChannelConfigError> {
        Self::load_environment(get, false)
    }

    fn load_environment(
        get: impl Fn(&str) -> Option<String>,
        notifier: bool,
    ) -> Result<Self, ChannelConfigError> {
        let slot = |result: Result<Option<EmailChannel>, ChannelConfigError>, profile| match result
        {
            Ok(channel) => channel.map(Arc::new),
            Err(_) => {
                tracing::warn!(profile, "SMTP profile is unavailable");
                None
            }
        };
        let primary = slot(EmailChannel::from_environment(&get), "primary");
        let snapshot_dir = get("WEB_SMTP_SNAPSHOT_DIR");
        if notifier && primary.is_none() && snapshot_dir.is_none() {
            return Err(PRIMARY_REQUIRED);
        }
        let secondary = slot(
            EmailChannel::from_environment(|name| match name.strip_prefix("WEB_SMTP_") {
                Some(suffix) => get(&format!("WEB_SMTP_SECONDARY_{suffix}")),
                None => get(name),
            }),
            "secondary",
        );
        if !notifier && primary.is_none() && (secondary.is_none() || snapshot_dir.is_none()) {
            return Err(ChannelConfigError(
                "a usable SMTP primary profile or a secondary profile with snapshot storage is required",
            ));
        }
        let snapshots = snapshot_dir
            .map(|path| {
                let static_root = get("WEB_STATIC_ROOT").ok_or(ChannelConfigError(
                    "WEB_STATIC_ROOT is required with WEB_SMTP_SNAPSHOT_DIR",
                ))?;
                SnapshotStore::open(
                    Path::new(&path),
                    Path::new(&static_root),
                    !notifier || primary.is_some(),
                )
                .map(Arc::new)
            })
            .transpose()?;
        Ok(Self {
            primary,
            secondary,
            snapshots,
            #[cfg(test)]
            test_ca_file: None,
        })
    }

    #[cfg(test)]
    pub(super) fn trust_test_ca(&mut self, certificate: &Path) {
        self.test_ca_file = Some(certificate.to_owned());
    }

    pub fn drain_timeout(&self) -> Duration {
        if self.snapshots.is_some() {
            // A retained snapshot can have a longer timeout than the current slots.
            // Allow three DB deadlines for claim, capture/pin and settlement.
            Duration::from_secs(75) + TRANSFER_GRACE + SNAPSHOT_LOAD_DEADLINE
        } else {
            self.primary
                .as_deref()
                .map_or(TRANSFER_GRACE, EmailChannel::timeout)
                + Duration::from_secs(10)
        }
    }

    fn channel(&self, profile: SmtpProfile) -> Option<&Arc<EmailChannel>> {
        match profile {
            SmtpProfile::Primary => self.primary.as_ref(),
            SmtpProfile::Secondary => self.secondary.as_ref(),
        }
    }

    async fn open_snapshot(&self, id: Uuid) -> Option<EmailChannel> {
        let store = self.snapshots.as_ref()?.clone();
        timeout(
            SNAPSHOT_LOAD_DEADLINE,
            tokio::task::spawn_blocking(move || store.load(id)),
        )
        .await
        .ok()?
        .ok()?
        .ok()
    }
}

impl NotificationChannel for EmailChannels {
    fn validate(&self) -> Result<(), ChannelConfigError> {
        validate_transport()
    }

    fn primary_available(&self) -> bool {
        self.primary.is_some()
    }

    fn snapshot_storage_available(&self) -> bool {
        self.snapshots.is_some()
    }

    async fn prepare_snapshot(&self, snapshot: Uuid) -> Option<PreparedSnapshot> {
        self.open_snapshot(snapshot)
            .await
            .map(PreparedSnapshot::Loaded)
    }

    async fn test(&self, id: Uuid) -> DeliveryOutcome {
        match &self.primary {
            Some(channel) => channel.test(id).await,
            None => DeliveryOutcome::ConfigurationError,
        }
    }

    async fn send(&self, id: Uuid, mail: MailKind) -> DeliveryOutcome {
        match &self.primary {
            Some(channel) => channel.send(id, mail).await,
            None => DeliveryOutcome::ConfigurationError,
        }
    }

    async fn capture_profile(&self, profile: SmtpProfile) -> Option<Uuid> {
        let store = self.snapshots.as_ref()?.clone();
        let channel = self.channel(profile)?.clone();
        tokio::task::spawn_blocking(move || store.save(&channel))
            .await
            .ok()?
            .ok()
    }

    async fn test_snapshot(&self, snapshot: Uuid, id: Uuid) -> DeliveryOutcome {
        let Some(channel) = self.open_snapshot(snapshot).await else {
            return DeliveryOutcome::SnapshotUnavailable;
        };
        #[cfg(test)]
        if let Some(certificate) = &self.test_ca_file {
            let mut curl = Command::new(CURL_BIN);
            curl.args(["--disable", "--cacert"]).arg(certificate);
            return channel.deliver_with(curl, id, MessageKind::Test).await;
        }
        channel.test(id).await
    }

    async fn send_snapshot(&self, snapshot: Uuid, id: Uuid, mail: MailKind) -> DeliveryOutcome {
        match self.open_snapshot(snapshot).await {
            Some(channel) => channel.send(id, mail).await,
            None => DeliveryOutcome::ConfigurationError,
        }
    }
}

#[derive(Clone, Copy)]
enum MessageKind {
    Test,
    Notification(MailKind),
}

impl MessageKind {
    fn content(self) -> (&'static str, String) {
        match self {
            Self::Test => (
                "Fertile notification test",
                "This test message checks your Fertile email channel.".into(),
            ),
            Self::Notification(MailKind::Alert { condition }) => (
                "Fertile alert",
                format!(
                    "An alert needs your attention.\r\n{}",
                    condition_description(condition)
                ),
            ),
            Self::Notification(MailKind::Reminder { condition }) => (
                "Fertile alert reminder",
                format!(
                    "An existing alert remains open.\r\n{}",
                    condition_description(condition)
                ),
            ),
            Self::Notification(MailKind::Recovery { condition }) => (
                "Fertile alert recovered",
                format!(
                    "An existing alert has recovered.\r\n{}",
                    condition_description(condition)
                ),
            ),
            Self::Notification(MailKind::Digest { count }) => (
                "Fertile alerts digest",
                format!("{count} notifications are ready to review."),
            ),
            Self::Notification(MailKind::TransportRecovered { count }) => (
                "Fertile email channel recovered",
                format!("The email channel resumed after {count} failed delivery attempts."),
            ),
        }
    }
}

fn condition_description(condition: Condition) -> &'static str {
    match condition {
        Condition::SourceUnavailable => "Monitored source unavailable",
        Condition::DiskCritical => "Critically low disk space",
        Condition::BackupStale => "Backup recovery point overdue",
        Condition::RecoveryVerificationFailed => "Recovery verification failed",
        Condition::MarketDataDelayed => "Market data delayed",
        Condition::BookOutOfSync => "Order book out of sync",
        Condition::BackfillFailed => "Backfill failed",
        Condition::BacktestFailed => "Backtest failed",
        Condition::BacktestCompleted => "Backtest completed",
        Condition::BackupSucceeded => "Backup completed",
        Condition::MarketAnomaly => "Market anomaly detected",
        Condition::SmtpUnavailable => "Email channel unavailable",
    }
}

fn anonymous_file(name: &str, contents: &[u8]) -> io::Result<File> {
    let mut file = File::from(memfd_create(name, MemfdFlags::CLOEXEC)?);
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(contents)?;
    file.rewind()?;
    Ok(file)
}

fn classify(output: &Output) -> DeliveryOutcome {
    let exit = output.status.code();
    let response = str::from_utf8(&output.stdout)
        .ok()
        .filter(|value| value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<u16>().ok());
    match (exit, response) {
        (Some(28), _) => DeliveryOutcome::Timeout,
        (Some(0), Some(250)) => DeliveryOutcome::Accepted,
        (_, Some(400..=499)) => DeliveryOutcome::Transient,
        (_, Some(500..=599)) => DeliveryOutcome::Rejected,
        (Some(1 | 2 | 3 | 4 | 43 | 48 | 58 | 60 | 64 | 67 | 77), _) => {
            DeliveryOutcome::ConfigurationError
        }
        _ => DeliveryOutcome::Transient,
    }
}

#[cfg(test)]
mod snapshot_tests;
#[cfg(test)]
mod tests;
