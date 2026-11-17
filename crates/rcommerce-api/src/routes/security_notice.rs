//! E-mails telling an account's owner that its sign-in methods changed: a factor
//! added or removed, a recovery code used, new recovery codes, an admin reset. Sent
//! in the background next to the security log entry; a failed e-mail never fails
//! the request.

use chrono::Utc;
use rcommerce_core::models::Customer;
use rcommerce_core::notification::{i18n, EmailNotificationFactory};
use rcommerce_core::services::two_factor::events::{self, Event};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::routes::email_helpers::spawn_send;
use crate::state::AppState;

/// Record `event` in the security log and, when it changes how `customer` signs
/// in (`events::NOTIFIED`), e-mail them about it in their language
pub(crate) async fn record(state: &AppState, customer: &Customer, event: Event<'_>) {
    let kind = event.kind.to_string();
    events::record_quietly(state.db.pool(), event).await;
    if state.notification_service.is_none() || !events::NOTIFIED.contains(&kind.as_str()) {
        return;
    }
    if !first_in_a_while(customer.id, &kind) {
        return;
    }
    let locale = i18n::pick_locale([
        customer.locale.as_deref(),
        Some(state.email_locale.as_str()),
    ]);
    let now = Utc::now();
    let day = i18n::date(&locale, now.date_naive());
    let when = format!("{day} {} UTC", now.format("%H:%M"));
    let name = greeting(&customer.first_name, &customer.last_name, &customer.email);
    let url = format!(
        "{}/account/security",
        state.storefront_url.trim_end_matches('/')
    );
    let built = EmailNotificationFactory::security_notice(
        &customer.email,
        &name,
        &kind,
        &when,
        &url,
        &state.branding,
        &locale,
    );
    match built {
        Ok(notification) => spawn_send(state, notification, "security notice"),
        Err(e) => tracing::warn!("Security notice {} not built: {}", kind, e),
    }
}

/// One notice per account and kind every 10 minutes: a loop of changes neither floods
/// the mailbox nor spends the store's sending quota
const QUIET: Duration = Duration::from_secs(600);

/// (account, kind) -> when its last notice went
static SENT: OnceLock<Mutex<HashMap<(Uuid, String), Instant>>> = OnceLock::new();

/// Whether no notice of `kind` went to `customer_id` in the last [`QUIET`] (and note
/// this one)
fn first_in_a_while(customer_id: Uuid, kind: &str) -> bool {
    let Ok(mut sent) = SENT.get_or_init(Mutex::default).lock() else {
        return true;
    };
    let now = Instant::now();
    if sent.len() > 10_000 {
        sent.retain(|_, at| now.duration_since(*at) < QUIET);
    }
    let key = (customer_id, kind.to_string());
    if sent
        .get(&key)
        .is_some_and(|at| now.duration_since(*at) < QUIET)
    {
        return false;
    }
    sent.insert(key, now);
    true
}

/// The name to greet: letters, spaces, hyphens and apostrophes only, at most 40
/// characters. The account's holder writes the name, and whoever holds the account
/// must not be able to put a message (or a link) into the owner's security notice.
/// The e-mail address when nothing is left.
fn greeting(first_name: &str, last_name: &str, email: &str) -> String {
    let kept: String = format!("{first_name} {last_name}")
        .chars()
        .filter(|c| c.is_alphabetic() || matches!(c, ' ' | '-' | '\''))
        .take(40)
        .collect();
    let name = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        email.to_string()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_keeps_names_and_drops_messages() {
        assert_eq!(
            greeting("Anne-Marie", "D'Haese", "a@x.be"),
            "Anne-Marie D'Haese"
        );
        assert_eq!(greeting("Zoë", "Øster", "a@x.be"), "Zoë Øster");
        let sneaky = greeting("(update, see evil.example/x)", "Lee", "a@x.be");
        assert!(!sneaky.contains('.') && !sneaky.contains('/') && !sneaky.contains('('));
        assert_eq!(greeting("<>", "123", "a@x.be"), "a@x.be");
        assert!(greeting(&"a".repeat(100), "b", "a@x.be").chars().count() <= 40);
    }

    #[test]
    fn one_notice_per_account_and_kind_in_a_while() {
        let (one, other) = (Uuid::new_v4(), Uuid::new_v4());
        assert!(first_in_a_while(one, "passkey_added"));
        assert!(!first_in_a_while(one, "passkey_added"));
        assert!(first_in_a_while(one, "passkey_removed"));
        assert!(first_in_a_while(other, "passkey_added"));
    }
}
