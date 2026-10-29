//! Outbound mail. The transport is replaceable so tests can inspect links.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lettre::message::header::{HeaderName, HeaderValue};
use lettre::message::{Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use sourcegull_config::{SiteConfig, SmtpAuthentication, SmtpEncryption};
use sourcegull_crypto::session::{generate_session_id, hash_session_id};
use sourcegull_types::UserId;

use crate::state::AppState;

/// The error shown when an email is needed but `email.smtp` is not set.
pub const NOT_CONFIGURED: &str = "Email is not set up on this instance. A site admin can configure \
     SMTP under Site admin > Site configuration > Email & webhooks.";

/// A text and HTML email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailMessage {
    pub to: String,
    pub subject: String,
    pub text: String,
    pub html: String,
    /// Extra header fields as `(name, value)`, e.g. a mailing list's `Approved` header. The
    /// caller validates them (a name without `:` or whitespace, a value without line breaks).
    pub headers: Vec<(String, String)>,
}

#[async_trait]
pub trait EmailSender: Send + Sync + std::fmt::Debug {
    async fn send(&self, config: &SiteConfig, message: EmailMessage) -> Result<(), String>;
}

/// SMTP delivery using the current site config at send time.
#[derive(Debug, Default)]
pub struct SmtpEmailSender;

#[async_trait]
impl EmailSender for SmtpEmailSender {
    async fn send(&self, config: &SiteConfig, mail: EmailMessage) -> Result<(), String> {
        let smtp = config.email_smtp.as_ref().ok_or(NOT_CONFIGURED)?;
        let message = build_message(config, mail)?;
        let mut builder = match smtp.encryption {
            SmtpEncryption::Starttls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host)
                    .map_err(|e| e.to_string())?
            }
            SmtpEncryption::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&smtp.host)
                .map_err(|e| e.to_string())?,
            SmtpEncryption::None => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&smtp.host)
            }
        }
        .port(smtp.port);
        if smtp.authentication != SmtpAuthentication::None {
            let username = smtp.username.clone().ok_or("Missing SMTP username")?;
            let password = smtp.password.clone().ok_or("Missing SMTP password")?;
            let mechanism = match smtp.authentication {
                SmtpAuthentication::Plain => Mechanism::Plain,
                SmtpAuthentication::Login => Mechanism::Login,
                SmtpAuthentication::None => unreachable!(),
            };
            builder = builder
                .credentials(Credentials::new(username, password))
                .authentication(vec![mechanism]);
        }
        if smtp.no_verify_tls && smtp.encryption != SmtpEncryption::None {
            let parameters = TlsParameters::builder(smtp.host.clone())
                .dangerous_accept_invalid_certs(true)
                .dangerous_accept_invalid_hostnames(true)
                .build()
                .map_err(|e| e.to_string())?;
            builder = builder.tls(match smtp.encryption {
                SmtpEncryption::Starttls => Tls::Required(parameters),
                SmtpEncryption::Tls => Tls::Wrapper(parameters),
                SmtpEncryption::None => unreachable!(),
            });
        }
        builder
            .build()
            .send(message)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// The MIME message of `mail`, from the site's sender address.
fn build_message(config: &SiteConfig, mail: EmailMessage) -> Result<Message, String> {
    let from: Mailbox = config
        .email_address
        .as_deref()
        .ok_or("Missing email.address")?
        .parse()
        .map_err(|e| format!("Invalid sender address: {e}"))?;
    let from = Mailbox::new(config.email_sender_name.clone(), from.email);
    let mut builder = Message::builder();
    for (name, value) in mail.headers {
        if value.contains(['\r', '\n']) {
            return Err(format!("Invalid value of header {name:?}: line break"));
        }
        let name = HeaderName::new_from_ascii(name.clone())
            .map_err(|_| format!("Invalid header name {name:?}"))?;
        builder = builder.raw_header(HeaderValue::new(name, value));
    }
    builder
        .from(from)
        .to(mail
            .to
            .parse()
            .map_err(|e| format!("Invalid recipient: {e}"))?)
        .subject(mail.subject)
        .multipart(
            MultiPart::alternative()
                .singlepart(SinglePart::plain(mail.text))
                .singlepart(SinglePart::html(mail.html)),
        )
        .map_err(|e| e.to_string())
}

/// In-process sender retaining every message for assertions.
#[derive(Debug, Default, Clone)]
pub struct MemoryEmailSender {
    messages: Arc<Mutex<Vec<EmailMessage>>>,
}

impl MemoryEmailSender {
    pub fn messages(&self) -> Vec<EmailMessage> {
        self.messages.lock().expect("email lock poisoned").clone()
    }
}

#[async_trait]
impl EmailSender for MemoryEmailSender {
    async fn send(&self, _: &SiteConfig, message: EmailMessage) -> Result<(), String> {
        self.messages
            .lock()
            .expect("email lock poisoned")
            .push(message);
        Ok(())
    }
}

/// Simple escaped HTML link paired with a plain-text body.
pub fn link_message(to: &str, subject: &str, intro: &str, url: &str) -> EmailMessage {
    let escaped = url
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;");
    EmailMessage {
        to: to.into(),
        subject: subject.into(),
        text: format!("{intro}\n\n{url}\n"),
        html: format!("<p>{intro}</p><p><a href=\"{escaped}\">Open link</a></p>"),
        headers: Vec::new(),
    }
}

/// Text representation of a SHA-256 code hash for `user_emails`.
pub fn code_hash(code: &str) -> String {
    hash_session_id(code)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Issues an admin-visible reset link and emails it when delivery is configured.
pub async fn issue_password_reset(
    state: &AppState,
    user: UserId,
    email: Option<&str>,
) -> Result<String, sourcegull_db::DbError> {
    let code = generate_session_id();
    let config = state.config.load();
    state
        .db
        .password_resets()
        .create(
            user,
            &hash_session_id(&code),
            config.value.auth_password_reset_link_expiry,
        )
        .await?;
    let origin = config
        .value
        .external_url
        .as_deref()
        .unwrap_or("")
        .trim_end_matches('/');
    let id = crate::relay::marshal_id("User", user.get());
    let url = format!("{origin}/reset-password?userID={id}&code={code}");
    if let Some(email) = email {
        state
            .send_email(link_message(
                email,
                "Reset your password",
                "Use this link to reset your password:",
                &url,
            ))
            .await;
    }
    Ok(url)
}

/// Issues a verification link for an unverified address.
pub async fn send_verification(
    state: &AppState,
    user: UserId,
    email: &str,
) -> Result<bool, sourcegull_db::DbError> {
    if state.config.load().value.email_smtp.is_none() {
        return Ok(false);
    }
    let code = generate_session_id();
    if !state
        .db
        .user_emails()
        .set_verification_code(user, email, &code_hash(&code))
        .await?
    {
        return Ok(false);
    }
    let config = state.config.load();
    let origin = config
        .value
        .external_url
        .as_deref()
        .unwrap_or("")
        .trim_end_matches('/');
    let encoded = percent_encoding::utf8_percent_encode(email, percent_encoding::NON_ALPHANUMERIC);
    let url = format!("{origin}/-/verify-email?code={code}&email={encoded}");
    state
        .send_email(link_message(
            email,
            "Verify your email",
            "Use this link to verify your email address:",
            &url,
        ))
        .await;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SiteConfig {
        SiteConfig {
            email_address: Some("sourcegull@example.com".into()),
            ..SiteConfig::default()
        }
    }

    fn mail(headers: Vec<(String, String)>) -> EmailMessage {
        EmailMessage {
            to: "alice@example.com".into(),
            subject: "Hello".into(),
            text: "text".into(),
            html: "<p>html</p>".into(),
            headers,
        }
    }

    #[test]
    fn extra_headers_are_added() {
        let m = build_message(
            &config(),
            mail(vec![("Approved".into(), "list-password".into())]),
        )
        .expect("message");
        let raw = String::from_utf8(m.formatted()).expect("utf-8");
        let lines: Vec<&str> = raw.split("\r\n").collect();
        assert!(lines.contains(&"Approved: list-password"), "{raw}");
        assert!(lines.contains(&"Subject: Hello"), "{raw}");
    }

    #[test]
    fn invalid_headers_are_refused() {
        for (name, value) in [
            ("Approved", "x\r\nBcc: eve@example.com"),
            ("Bad Name", "x"),
            ("Bad:Name", "x"),
        ] {
            assert!(
                build_message(&config(), mail(vec![(name.into(), value.into())])).is_err(),
                "{name}: {value}"
            );
        }
    }
}
