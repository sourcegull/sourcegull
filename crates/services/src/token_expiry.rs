//! Code host token expiry: a worker singleton that reads when each connection's token expires
//! and emails before it does (`codeHosts.tokenExpiryAlerts`).
//!
//! Every [`INTERVAL`] it checks the tokens that are due ([`RECHECK`] after the last check, or
//! at once after the connection's configuration changed) with one request each
//! ([`RepoSource::token_expiry`](sourcegull_codehost::RepoSource::token_expiry)), then sends
//! the alerts that are due. Each alert window (`daysBefore`, and 0 for the day the token
//! expires) is emailed once per expiry date; a token checked for the first time inside a
//! window gets only the alert of that window.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sourcegull_codehost::TokenExpiry;
use sourcegull_config::{SiteConfig, SiteConfigHandle};
use sourcegull_crypto::Keyring;
use sourcegull_db::{Db, StoredExpiry, TokenCheck};
use sourcegull_repos::Registry;
use sourcegull_types::{ExternalService, ExternalServiceKind};
use tokio_util::sync::CancellationToken;

use crate::email::{EmailMessage, EmailSender};
use crate::relay::{EXTERNAL_SERVICE_KIND, marshal_id};

/// Lease name of the singleton.
pub const TOKEN_EXPIRY_CHECKER: &str = "code-host-token-expiry";
/// How often the singleton looks for due checks and alerts.
pub const INTERVAL: Duration = Duration::from_secs(15 * 60);
/// How long a check stays current.
pub const RECHECK: chrono::Duration = chrono::Duration::days(1);
/// How long one check may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// What the checker needs.
#[derive(Debug, Clone)]
pub struct TokenExpiryChecker {
    pub db: Db,
    pub config: SiteConfigHandle,
    pub keyring: Option<Arc<Keyring>>,
    pub registry: Registry,
    pub email: Arc<dyn EmailSender>,
}

impl TokenExpiryChecker {
    /// Checks and alerts every [`INTERVAL`] (first run right away) until `cancel` fires.
    pub async fn run(self, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            let run = async {
                self.check_due(Utc::now()).await?;
                self.send_alerts(Utc::now()).await
            };
            if let Err(e) = sourcegull_db::record_run(TOKEN_EXPIRY_CHECKER, run).await {
                tracing::warn!(error = %e, "code host token expiry check failed");
            }
        }
    }

    /// Checks the tokens of the connections that are due.
    pub async fn check_due(&self, now: DateTime<Utc>) -> sourcegull_db::Result<()> {
        if self.keyring.is_none() {
            // Connection configurations cannot be read.
            return Ok(());
        }
        let store = self.db.external_services(self.keyring.clone());
        let checks = self.db.token_checks();
        for id in checks.due(now - RECHECK).await? {
            let Some(svc) = store.get(id).await? else {
                continue;
            };
            let text = store.config(id).await?;
            match self.expiry(&svc, &text).await {
                Ok(expiry) => {
                    checks.record(id, svc.updated_at, expiry).await?;
                }
                Err(e) => {
                    tracing::info!(connection = id.get(), error = %e, "token expiry check failed");
                    checks.record_error(id, svc.updated_at, &e).await?;
                }
            }
        }
        Ok(())
    }

    async fn expiry(&self, svc: &ExternalService, text: &str) -> Result<StoredExpiry, String> {
        let source = self
            .registry
            .source(svc.kind, text)
            .map_err(|e| e.to_string())?;
        match tokio::time::timeout(CHECK_TIMEOUT, source.token_expiry()).await {
            Ok(Ok(expiry)) => Ok(stored(expiry)),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("timed out".to_owned()),
        }
    }

    /// Emails the alerts that are due, when they are enabled and email is set up.
    pub async fn send_alerts(&self, now: DateTime<Utc>) -> sourcegull_db::Result<()> {
        let config = self.config.load();
        let site = &config.value;
        let settings = &site.code_hosts_token_expiry_alerts;
        if !settings.enabled || site.email_smtp.is_none() {
            return Ok(());
        }
        let checks = self.db.token_checks();
        let due: Vec<(TokenCheck, DateTime<Utc>, i32)> = checks
            .expiring()
            .await?
            .into_iter()
            .filter_map(|c| {
                let StoredExpiry::At(at) = c.expiry else {
                    return None;
                };
                let window = alert_window(at, now, &settings.days_before)?;
                c.alerted_days
                    .is_none_or(|sent| window < sent)
                    .then_some((c, at, window))
            })
            .collect();
        if due.is_empty() {
            return Ok(());
        }
        let recipients = if settings.recipients.is_empty() {
            self.db.user_emails().site_admin_addresses().await?
        } else {
            settings.recipients.clone()
        };
        if recipients.is_empty() {
            tracing::warn!("token expiry alerts are due, but no site admin has a verified email");
            return Ok(());
        }
        let store = self.db.external_services(None);
        for (check, at, window) in due {
            let Some(svc) = store.get(check.external_service_id).await? else {
                continue;
            };
            let mut sent = false;
            for to in &recipients {
                match self.email.send(site, alert(site, &svc, at, now, to)).await {
                    Ok(()) => sent = true,
                    Err(e) => {
                        tracing::warn!(connection = svc.id.get(), to, error = %e, "token expiry alert not sent");
                    }
                }
            }
            if sent {
                checks.set_alerted(svc.id, window).await?;
            }
        }
        Ok(())
    }
}

fn stored(expiry: TokenExpiry) -> StoredExpiry {
    match expiry {
        TokenExpiry::Unknown => StoredExpiry::Unknown,
        TokenExpiry::Never => StoredExpiry::Never,
        TokenExpiry::At(at) => StoredExpiry::At(at),
    }
}

/// Whole days left until `at`, rounded up; 0 once it has passed.
pub fn days_left(at: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    let secs = (at - now).num_seconds();
    if secs <= 0 {
        0
    } else {
        (secs + 86_399) / 86_400
    }
}

/// The alert window a token expiring at `at` is in: the smallest of `days_before` and 0 that
/// is at least the days left. `None` before the first window.
pub fn alert_window(at: DateTime<Utc>, now: DateTime<Utc>, days_before: &[u32]) -> Option<i32> {
    let left = days_left(at, now);
    days_before
        .iter()
        .map(|&d| i64::from(d))
        .chain([0])
        .filter(|&d| left <= d)
        .min()
        .and_then(|d| i32::try_from(d).ok())
}

fn kind_name(kind: ExternalServiceKind) -> &'static str {
    match kind {
        ExternalServiceKind::GitLab => "GitLab",
        ExternalServiceKind::GitHub => "GitHub",
        ExternalServiceKind::Gitea => "Gitea",
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The alert email for `svc`'s token, which expires at `at`.
fn alert(
    site: &SiteConfig,
    svc: &ExternalService,
    at: DateTime<Utc>,
    now: DateTime<Utc>,
    to: &str,
) -> EmailMessage {
    let kind = kind_name(svc.kind);
    let name = &svc.display_name;
    let date = at.format("%Y-%m-%d %H:%M UTC");
    let left = days_left(at, now);
    let (subject, status) = match left {
        0 => (
            format!("The {kind} token of {name} has expired"),
            format!(
                "The access token of the {kind} connection \"{name}\" expired on {date}. Syncing and fetching stop until it is replaced."
            ),
        ),
        1 => (
            format!("The {kind} token of {name} expires tomorrow"),
            format!(
                "The access token of the {kind} connection \"{name}\" expires on {date}. Syncing and fetching stop then."
            ),
        ),
        n => (
            format!("The {kind} token of {name} expires in {n} days"),
            format!(
                "The access token of the {kind} connection \"{name}\" expires on {date}. Syncing and fetching stop then."
            ),
        ),
    };
    let action =
        format!("Create a new token in {kind} and set it on the connection (Edit, then Token).");
    let origin = site
        .external_url
        .as_deref()
        .unwrap_or("")
        .trim_end_matches('/');
    let url = format!(
        "{origin}/site-admin/external-services/{}",
        marshal_id(EXTERNAL_SERVICE_KIND, svc.id.get())
    );
    let footer =
        "Turn these emails off with codeHosts.tokenExpiryAlerts in the site configuration.";
    EmailMessage {
        to: to.to_owned(),
        subject,
        text: format!("{status}\n\n{action}\n\n{url}\n\n{footer}\n"),
        html: format!(
            "<p>{}</p><p>{}</p><p><a href=\"{}\">Open the connection</a></p><p style=\"color:#666\">{}</p>",
            html_escape(&status),
            html_escape(&action),
            html_escape(&url),
            html_escape(footer)
        ),
        headers: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    #[test]
    fn days_left_rounds_up() {
        let at = t("2026-11-22T00:00:00Z");
        assert_eq!(days_left(at, t("2026-11-21T23:00:00Z")), 1);
        assert_eq!(days_left(at, t("2026-11-21T00:00:00Z")), 1);
        assert_eq!(days_left(at, t("2026-11-20T23:59:59Z")), 2);
        assert_eq!(days_left(at, at), 0);
        assert_eq!(days_left(at, t("2026-11-25T00:00:00Z")), 0);
    }

    #[test]
    fn alert_windows() {
        let at = t("2026-11-22T00:00:00Z");
        let days = [14, 7, 1];
        let window = |now: &str| alert_window(at, t(now), &days);
        assert_eq!(window("2026-11-01T00:00:00Z"), None);
        assert_eq!(window("2026-11-08T00:00:00Z"), Some(14));
        assert_eq!(window("2026-11-12T00:00:00Z"), Some(14));
        assert_eq!(window("2026-11-15T00:00:00Z"), Some(7));
        assert_eq!(window("2026-11-19T00:00:00Z"), Some(7));
        assert_eq!(window("2026-11-21T00:00:00Z"), Some(1));
        assert_eq!(window("2026-11-22T00:00:00Z"), Some(0));
        assert_eq!(alert_window(at, t("2026-11-01T00:00:00Z"), &[]), None);
        assert_eq!(alert_window(at, t("2026-11-23T00:00:00Z"), &[]), Some(0));
    }

    #[test]
    fn alert_email() {
        let site = SiteConfig {
            external_url: Some("https://sg.example.com/".into()),
            ..SiteConfig::default()
        };
        let svc = ExternalService {
            id: sourcegull_types::ExternalServiceId(7),
            kind: ExternalServiceKind::GitLab,
            display_name: "GitLab <main>".into(),
            webhook_uuid: String::new(),
            last_sync_at: None,
            next_sync_at: None,
            last_full_sync_at: None,
            sync_watermark: None,
            last_sync_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deleted_at: None,
        };
        let at = t("2026-11-22T00:00:00Z");
        let mail = alert(
            &site,
            &svc,
            at,
            t("2026-11-15T00:00:00Z"),
            "admin@example.com",
        );
        assert_eq!(
            mail.subject,
            "The GitLab token of GitLab <main> expires in 7 days"
        );
        assert!(mail.text.contains("expires on 2026-11-22 00:00 UTC"));
        assert!(mail.text.contains(&format!(
            "https://sg.example.com/site-admin/external-services/{}",
            marshal_id(EXTERNAL_SERVICE_KIND, 7)
        )));
        assert!(mail.html.contains("GitLab &lt;main&gt;"));
        let expired = alert(&site, &svc, at, at, "admin@example.com");
        assert_eq!(
            expired.subject,
            "The GitLab token of GitLab <main> has expired"
        );
    }
}
