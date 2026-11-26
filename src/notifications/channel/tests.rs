use super::*;
use std::{fs, os::unix::fs::symlink};

fn configured(variable: &str) -> Option<String> {
    Some(
        match variable {
            "WEB_SMTP_HOST" => "relay.example.invalid",
            "WEB_SMTP_FROM" => "sender@example.invalid",
            "WEB_SMTP_TO" => "test-owner+alerts@example.invalid",
            "WEB_PUBLIC_ORIGIN" => "https://terminal.example.invalid",
            _ => return None,
        }
        .to_owned(),
    )
}

fn example() -> EmailChannel {
    EmailChannel::from_environment(configured).unwrap().unwrap()
}

fn substitute(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", script, "smtp-process-substitute"]);
    command
}

#[test]
fn smtp_environment_rejects_partial_configuration_and_injection() {
    assert!(EmailChannel::from_environment(|_| None).unwrap().is_none());
    assert!(
        EmailChannel::from_environment(|key| match key {
            "WEB_SMTP_HOST" => None,
            _ => configured(key),
        })
        .is_err()
    );
    for (key, value) in [
        ("WEB_SMTP_HOST", "relay.example.invalid/alternate"),
        ("WEB_SMTP_HOST", "user:password@relay.example.invalid"),
        (
            "WEB_SMTP_FROM",
            "owner@example.invalid\r\nBcc: x@example.invalid",
        ),
        (
            "WEB_SMTP_TO",
            "first@example.invalid,second@example.invalid",
        ),
        ("WEB_SMTP_PORT", "0"),
        ("WEB_SMTP_TLS", "none"),
        ("WEB_SMTP_TIMEOUT_SECONDS", "61"),
        (
            "WEB_PUBLIC_ORIGIN",
            "https://terminal.example.invalid/untrusted",
        ),
    ] {
        assert!(
            EmailChannel::from_environment(|name| {
                if name == key {
                    Some(value.into())
                } else {
                    configured(name)
                }
            })
            .is_err(),
            "accepted {key}"
        );
    }
    assert_eq!(example().relay, "smtps://relay.example.invalid:465");
    let starttls = EmailChannel::from_environment(|name| match name {
        "WEB_SMTP_HOST" => Some("::1".into()),
        "WEB_SMTP_TLS" => Some("starttls".into()),
        _ => configured(name),
    })
    .unwrap()
    .unwrap();
    assert_eq!(starttls.relay, "smtp://[::1]:587");
}

#[test]
fn invalid_secondary_slot_leaves_the_primary_channel_usable() {
    let channels = EmailChannels::from_environment(|name| match name {
        "WEB_SMTP_SECONDARY_HOST" => Some("invalid/path".into()),
        _ => configured(name),
    })
    .unwrap();
    assert_eq!(
        channels.channel(SmtpProfile::Primary).unwrap().relay,
        "smtps://relay.example.invalid:465"
    );
    assert!(channels.channel(SmtpProfile::Secondary).is_none());
}

#[test]
fn smtp_secret_requires_owned_regular_0600_file_and_strict_format() {
    let directory = tempfile::TempDir::new_in("/tmp").unwrap();
    let path = directory.path().join("synthetic-credentials");
    let fixture = br#"{"username":"synthetic-user","password":"synthetic-password"}"#;
    fs::write(&path, fixture).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    assert!(Credentials::load(&path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(Credentials::load(&path).is_ok());
    let link = directory.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(Credentials::load(&link).is_err());
    assert!(Credentials::load(directory.path()).is_err());
    assert!(Credentials::load(Path::new("relative.json")).is_err());
    for invalid in [
        r#"{"username":"synthetic-user","password":"synthetic\npassword"}"#,
        r#"{"username":"synthetic:user","password":"synthetic-password"}"#,
        r#"{"username":"synthetic-user","password":"synthetic-password","secret":"synthetic-extra"}"#,
    ] {
        fs::write(&path, invalid).unwrap();
        let error = Credentials::load(&path).err().unwrap();
        assert!(!error.to_string().contains("synthetic"));
    }
}

#[test]
fn mail_templates_use_stable_message_ids_and_allowlisted_content() {
    let channel = example();
    // Construct the fixture from a fixed counter and literal message metadata.
    let id = Uuid::from_u128(42);
    let kinds = [
        (
            MessageKind::Test,
            "This test message checks your Fertile email channel.",
        ),
        (
            MessageKind::Notification(MailKind::Alert {
                condition: Condition::MarketDataDelayed,
            }),
            "An alert needs your attention.\r\nMarket data delayed",
        ),
        (
            MessageKind::Notification(MailKind::Reminder {
                condition: Condition::BookOutOfSync,
            }),
            "An existing alert remains open.\r\nOrder book out of sync",
        ),
        (
            MessageKind::Notification(MailKind::Recovery {
                condition: Condition::SourceUnavailable,
            }),
            "An existing alert has recovered.\r\nMonitored source unavailable",
        ),
        (
            MessageKind::Notification(MailKind::Digest { count: 3 }),
            "3 notifications are ready to review.",
        ),
        (
            MessageKind::Notification(MailKind::TransportRecovered { count: 7 }),
            "The email channel resumed after 7 failed delivery attempts.",
        ),
    ];
    for (kind, description) in kinds {
        let message = channel.message(id, kind);
        let (headers, body) = message.split_once("\r\n\r\n").unwrap();
        assert!(headers.contains(&format!("Message-ID: <{id}@web-notifier.invalid>\r\n")));
        assert_eq!(
            body,
            format!(
                "{description}\r\n\r\nOpen https://terminal.example.invalid/alerts in your private Fertile terminal.\r\n"
            )
        );
        assert!(body.is_ascii());
    }
}

#[tokio::test]
async fn smtp_child_receives_test_message_and_secrets_only_through_descriptors() {
    let mut channel = example();
    let fixture = tempfile::NamedTempFile::new_in("/tmp").unwrap();
    fs::write(
        fixture.path(),
        br#"{"username":"synthetic-user","password":"synthetic\"test\\password"}"#,
    )
    .unwrap();
    channel.credentials = Some(Credentials::load(fixture.path()).unwrap());

    let mut command = substitute(
        r#"
        set -eu
        test -z "${SUBSTITUTE_PROXY_CANARY:-}"
        test "$(/usr/bin/cat)" = 'user = "synthetic-user:synthetic\"test\\password"'
        test "$1" = '--disable'
        case " $* " in
        *' --config - '*' --proto =smtp,smtps --ssl-reqd --tlsv1.2 '*' --url smtps://relay.example.invalid:465 '*) ;;
        *) exit 2 ;;
        esac
        upload_file=''
        while [ "$#" -gt 0 ]; do
            case "$1" in
            *synthetic-user*|*synthetic\"test*) exit 2 ;;
            --upload-file) upload_file="$2" ;;
            esac
            shift
        done
        test -n "$upload_file"
        case "$(/usr/bin/cat "$upload_file")" in
        *'Fertile notification test'*'00000000-0000-0000-0000-00000000002a@web-notifier.invalid'*) ;;
        *) exit 2 ;;
        esac
        printf '250'
    "#,
    );
    command.env(
        "SUBSTITUTE_PROXY_CANARY",
        "synthetic-config-must-stay-private",
    );
    assert_eq!(
        channel
            .deliver_with(command, Uuid::from_u128(42), MessageKind::Test)
            .await,
        DeliveryOutcome::Accepted,
    );
}

#[tokio::test]
async fn smtp_child_normalizes_accepted_timeout_transient_and_terminal_responses() {
    let channel = example();
    let id = Uuid::from_u128(42);
    let message = MessageKind::Notification(MailKind::Alert {
        condition: Condition::MarketAnomaly,
    });
    for (script, expected) in [
        ("printf '250'", DeliveryOutcome::Accepted),
        ("printf '250'; exit 28", DeliveryOutcome::Timeout),
        ("printf '450'; exit 55", DeliveryOutcome::Transient),
        ("printf '250'; exit 56", DeliveryOutcome::Transient),
        ("printf '550'; exit 55", DeliveryOutcome::Rejected),
        ("printf '000'; exit 60", DeliveryOutcome::ConfigurationError),
        (
            "printf 'invalid'; printf 'synthetic relay error' >&2; exit 55",
            DeliveryOutcome::Transient,
        ),
    ] {
        assert_eq!(
            channel.deliver_with(substitute(script), id, message).await,
            expected
        );
    }
}

#[tokio::test]
async fn smtp_process_watchdog_classifies_a_stalled_child_as_timeout() {
    let mut channel = example();
    channel.transfer_timeout = Duration::from_millis(20);
    let command = substitute("exec /bin/sleep 30");
    let result = timeout(
        Duration::from_secs(5),
        channel.deliver_with(command, Uuid::from_u128(42), MessageKind::Test),
    )
    .await
    .unwrap();
    assert_eq!(result, DeliveryOutcome::Timeout);
}
