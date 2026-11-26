use super::*;
use crate::notifications::model::Condition;

fn now() -> DateTime<Utc> {
    "2026-01-01T00:00:00Z".parse().unwrap()
}

fn urgent_rule() -> RuleConfig {
    RuleConfig {
        condition: Condition::SourceUnavailable,
        severity: Severity::Urgent,
        enabled: true,
        email_enabled: true,
        cooldown_seconds: None,
        reminder_seconds: Some(3600),
        min_occurrences: 2,
    }
}

#[test]
fn threshold_reminder_and_recovery_require_an_ordered_open_incident() {
    let rule = urgent_rule();
    let mut incident = Incident::default();
    let mut observe = |phase, seconds| {
        incident
            .apply(&rule, phase, now() + TimeDelta::seconds(seconds))
            .unwrap()
    };
    assert_eq!(observe(Phase::Triggered, 0), (Disposition::Observed, None));
    assert_eq!(observe(Phase::Recovered, 1), (Disposition::Observed, None));
    assert_eq!(observe(Phase::Triggered, 2), (Disposition::Observed, None));
    assert_eq!(
        observe(Phase::Triggered, 3),
        (Disposition::Pending, Some(DeliveryKind::Alert))
    );
    assert_eq!(observe(Phase::Recovered, 2), (Disposition::Ignored, None));
    assert_eq!(
        observe(Phase::Triggered, 3602),
        (Disposition::Observed, None)
    );
    assert_eq!(
        observe(Phase::Triggered, 3603),
        (Disposition::Pending, Some(DeliveryKind::Reminder))
    );
    assert_eq!(
        observe(Phase::Recovered, 3604),
        (Disposition::Pending, Some(DeliveryKind::Recovery))
    );
    assert_eq!(
        observe(Phase::Recovered, 3605),
        (Disposition::Observed, None)
    );
}

#[test]
fn page_only_rule_records_alert_and_recovery_without_mail() {
    let rule = RuleConfig {
        email_enabled: false,
        min_occurrences: 1,
        ..urgent_rule()
    };
    let mut incident = Incident::default();
    assert_eq!(
        incident.apply(&rule, Phase::Triggered, now()).unwrap(),
        (Disposition::InApp, None)
    );
    assert_eq!(
        incident
            .apply(&rule, Phase::Recovered, now() + TimeDelta::seconds(1))
            .unwrap(),
        (Disposition::InApp, None)
    );
}
