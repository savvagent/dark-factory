//! Mail delivery — the seam `df-auth` deliberately stops short of.
//!
//! `df_auth::magic::issue` mints a token and returns it. It does no I/O at all,
//! which is what makes it testable; deciding where the message goes, what it
//! says, and who carries it is this module's job.
//!
//! **Delivery is a trait, and the trait is the deliverable.** A provider needs
//! an account, a verified sending domain, SPF/DKIM records, and a bounce policy
//! — decisions that belong to a deployment, not to a crate. What the server owes
//! is a seam that a provider drops into without touching a handler, plus a
//! development implementation that never silently swallows a link.
//!
//! **A failed send is a failed request.** The tempting alternative — mint the
//! token, return 200, log the failure — produces a user staring at "check your
//! email" for a message that will never arrive, and a support ticket nobody can
//! diagnose. If the mail did not go, the caller is told, and the one-live-link
//! rule means retrying is safe.

use std::sync::Arc;

use crate::error::ApiError;

/// One message.
///
/// `text` is not optional and `html` is: every client renders text, some
/// gateways strip HTML, and a link that only exists inside a `<a href>` is a
/// link some recipients cannot use.
#[derive(Debug, Clone)]
pub struct Mail {
    pub to: String,
    pub subject: String,
    pub text: String,
    pub html: Option<String>,
}

/// How a message leaves the building.
///
/// `Send + Sync + 'static` so an implementation can be shared across every
/// request; `async_trait` because trait methods returning futures are still
/// awkward in object-safe form.
#[async_trait::async_trait]
pub trait Mailer: Send + Sync + 'static {
    async fn send(&self, mail: Mail) -> Result<(), MailError>;

    /// A short name for logs and for the health endpoint, so an operator can
    /// tell at a glance whether a deployment is actually mailing anything.
    fn describe(&self) -> &'static str;
}

#[derive(Debug, thiserror::Error)]
#[error("could not send mail: {0}")]
pub struct MailError(pub String);

impl From<MailError> for ApiError {
    fn from(e: MailError) -> Self {
        // The provider's message is for the operator, not the user: it names
        // hosts, credentials, and quota states.
        tracing::error!(error = %e, "mail delivery failed");
        ApiError::new(
            http::StatusCode::BAD_GATEWAY,
            "mail_undeliverable",
            "we could not send that email just now. Try again in a moment.",
        )
    }
}

/// Development delivery: write the message to the log.
///
/// **Loud on purpose.** Every send emits a `warn` naming the recipient and the
/// link, because the failure mode this guards against is a staging deployment
/// that silently discards every invitation for a week. A quiet no-op mailer
/// looks identical to a working one until somebody asks why nobody has joined.
pub struct LogMailer;

#[async_trait::async_trait]
impl Mailer for LogMailer {
    async fn send(&self, mail: Mail) -> Result<(), MailError> {
        tracing::warn!(
            to = %mail.to,
            subject = %mail.subject,
            "MAIL NOT SENT — no mail provider is configured. Message body follows:\n{}",
            mail.text
        );
        Ok(())
    }

    fn describe(&self) -> &'static str {
        "log (development only — no mail is actually sent)"
    }
}

/// Collects messages in memory. Tests assert on what was sent, and read the
/// links out of it rather than reaching into the database for a token.
#[derive(Default)]
pub struct CapturingMailer {
    sent: std::sync::Mutex<Vec<Mail>>,
}

impl CapturingMailer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn sent(&self) -> Vec<Mail> {
        self.sent.lock().expect("mail capture poisoned").clone()
    }

    pub fn last(&self) -> Option<Mail> {
        self.sent
            .lock()
            .expect("mail capture poisoned")
            .last()
            .cloned()
    }

    pub fn clear(&self) {
        self.sent.lock().expect("mail capture poisoned").clear();
    }
}

#[async_trait::async_trait]
impl Mailer for CapturingMailer {
    async fn send(&self, mail: Mail) -> Result<(), MailError> {
        self.sent.lock().expect("mail capture poisoned").push(mail);
        Ok(())
    }

    fn describe(&self) -> &'static str {
        "capturing (tests)"
    }
}

// ---------------------------------------------------------------------------
// The messages themselves
// ---------------------------------------------------------------------------

/// A magic link mail.
///
/// The link points at a **page**, not at an endpoint. The page renders a button
/// that POSTs the token back. That indirection is the whole reason this
/// function exists rather than the caller formatting a URL: corporate mail
/// scanners and link-preview fetchers follow every URL in every message, and a
/// link that consumes its token on `GET` is spent before the human ever clicks
/// it. The failure looks exactly like an attack and is not.
pub fn verify_email(to: &str, link: &str) -> Mail {
    Mail {
        to: to.to_string(),
        subject: "Confirm your email for dark-factory".into(),
        text: format!(
            "Confirm this address to finish setting up your dark-factory account:\n\n\
             {link}\n\n\
             The link works once and expires in 10 minutes.\n\n\
             If you did not sign up, you can ignore this — no account has been created \
             for this address without it."
        ),
        html: None,
    }
}

pub fn recover_account(to: &str, link: &str) -> Mail {
    Mail {
        to: to.to_string(),
        subject: "Recover your dark-factory account".into(),
        text: format!(
            "Use this link to get back into your dark-factory account:\n\n\
             {link}\n\n\
             The link works once and expires in 10 minutes. Following it will remove the \
             authenticator app currently on your account, and you will be asked to set up \
             a new one.\n\n\
             If you did not ask for this, ignore this message and nothing will change."
        ),
        html: None,
    }
}

/// An invitation.
///
/// Names the org and the inviter, because "you have been invited" from an
/// unfamiliar product is indistinguishable from spam, and the recipient's own
/// judgement is the only filter that matters at this point.
pub fn invitation(to: &str, org_name: &str, inviter: &str, role: &str, link: &str) -> Mail {
    Mail {
        to: to.to_string(),
        subject: format!("{inviter} invited you to {org_name} on dark-factory"),
        text: format!(
            "{inviter} has invited you to join {org_name} on dark-factory as a {role}.\n\n\
             {link}\n\n\
             The invitation expires in 14 days, and can only be accepted while signed in \
             as {to}.\n\n\
             dark-factory coordinates agentic coding work across a team's repositories."
        ),
        html: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_capturing_mailer_records_what_was_sent() {
        let mailer = CapturingMailer::new();
        mailer
            .send(verify_email(
                "bob@acme.test",
                "https://console.test/verify?token=x",
            ))
            .await
            .unwrap();

        let sent = mailer.last().expect("nothing captured");
        assert_eq!(sent.to, "bob@acme.test");
        assert!(sent.text.contains("https://console.test/verify?token=x"));
    }

    /// Every message that carries a credential says how long it lasts and what
    /// to do if it was unexpected. A link with no context is a link people
    /// report as phishing.
    #[test]
    fn credential_mails_explain_themselves() {
        let mails = [
            verify_email("bob@acme.test", "https://console.test/verify?token=x"),
            recover_account("bob@acme.test", "https://console.test/recover?token=x"),
            invitation(
                "bob@acme.test",
                "Acme",
                "rob@acme.test",
                "member",
                "https://console.test/invite/acme?token=x",
            ),
        ];

        for mail in mails {
            assert!(
                mail.text.contains("token=x"),
                "no link in {:?}",
                mail.subject
            );
            assert!(
                mail.text.contains("expires"),
                "{:?} does not say when it expires",
                mail.subject
            );
            assert!(!mail.subject.is_empty());
        }
    }
}
