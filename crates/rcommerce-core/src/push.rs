//! Push notifications to the store's mobile apps (Android, iOS).
//!
//! The apps register their device token (`push_devices`, migration 052); messages
//! go through Firebase Cloud Messaging (HTTP v1), which also reaches iOS apps set
//! up with an APNs key in the Firebase project. `[push] provider = "log"` only logs
//! (development); no provider: nothing is sent.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{Error, Result};

/// Devices a customer may keep; registering one more forgets the oldest
pub const MAX_DEVICES: i64 = 10;

/// `[push]`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PushConfig {
    /// `fcm` (Firebase Cloud Messaging), `log` (development); unset: no push
    #[serde(default)]
    pub provider: Option<String>,
    /// The Firebase project (`fcm`)
    #[serde(default)]
    pub fcm_project_id: Option<String>,
    /// The Firebase service account key, a JSON file (`fcm`)
    #[serde(default)]
    pub fcm_credentials_file: Option<String>,
}

/// What a notification says; `url` opens in the app when it is tapped
#[derive(Debug, Clone, PartialEq)]
pub struct PushMessage {
    pub title: String,
    pub body: String,
    pub url: String,
}

/// What became of one message
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Sent,
    /// The app was removed or the token replaced: the device is forgotten
    Unregistered,
    Failed,
}

/// The sender of the configured provider
pub enum PushSender {
    Log,
    Fcm(Box<Fcm>),
}

impl PushSender {
    /// The sender `[push]` asks for; none without a provider. A missing or bad
    /// key file is a configuration error.
    pub fn from_config(config: &PushConfig) -> Result<Option<Arc<Self>>> {
        match config.provider.as_deref() {
            None | Some("") => Ok(None),
            Some("log") => Ok(Some(Arc::new(Self::Log))),
            Some("fcm") => {
                let project = config
                    .fcm_project_id
                    .clone()
                    .ok_or_else(|| Error::config("push.fcm_project_id is required for fcm"))?;
                let path = config.fcm_credentials_file.as_deref().ok_or_else(|| {
                    Error::config("push.fcm_credentials_file is required for fcm")
                })?;
                let key = std::fs::read_to_string(path)
                    .map_err(|e| Error::config(format!("push.fcm_credentials_file: {e}")))?;
                Ok(Some(Arc::new(Self::Fcm(Box::new(Fcm::new(
                    project, &key,
                )?)))))
            }
            Some(other) => Err(Error::config(format!("push.provider: unknown `{other}`"))),
        }
    }

    /// Send `message` to one device
    pub async fn send(&self, token: &str, message: &PushMessage) -> Delivery {
        match self {
            Self::Log => {
                tracing::info!(title = %message.title, url = %message.url, "push (log only)");
                Delivery::Sent
            }
            Self::Fcm(fcm) => fcm.send(token, message).await,
        }
    }
}

/// The parts of a Firebase service account key used here
#[derive(Deserialize)]
struct ServiceAccount {
    client_email: String,
    private_key: String,
    #[serde(default = "google_token_uri")]
    token_uri: String,
}

fn google_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_string()
}

/// Firebase Cloud Messaging, HTTP v1
pub struct Fcm {
    project: String,
    account: ServiceAccount,
    key: jsonwebtoken::EncodingKey,
    http: reqwest::Client,
    /// The OAuth access token and when it stops being used
    access: Mutex<Option<(String, Instant)>>,
}

impl Fcm {
    /// From the project id and the service account key (JSON)
    pub fn new(project: String, key_json: &str) -> Result<Self> {
        let account: ServiceAccount = serde_json::from_str(key_json)
            .map_err(|e| Error::config(format!("push: bad service account key: {e}")))?;
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(account.private_key.as_bytes())
            .map_err(|e| Error::config(format!("push: bad service account key: {e}")))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| Error::config(format!("push: {e}")))?;
        Ok(Self {
            project,
            account,
            key,
            http,
            access: Mutex::new(None),
        })
    }

    /// An access token for the messaging scope, kept until 5 minutes before it ends
    async fn access_token(&self) -> Result<String> {
        let mut access = self.access.lock().await;
        if let Some((token, until)) = access.as_ref() {
            if Instant::now() < *until {
                return Ok(token.clone());
            }
        }
        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({
            "iss": self.account.client_email,
            "scope": "https://www.googleapis.com/auth/firebase.messaging",
            "aud": self.account.token_uri,
            "iat": now,
            "exp": now + 3600,
        });
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        let assertion = jsonwebtoken::encode(&header, &claims, &self.key)
            .map_err(|e| Error::notification_error(format!("push: {e}")))?;
        #[derive(Deserialize)]
        struct Granted {
            access_token: String,
            expires_in: u64,
        }
        let granted: Granted = self
            .http
            .post(&self.account.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
            ])
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| Error::notification_error(format!("push: token: {e}")))?
            .json()
            .await
            .map_err(|e| Error::notification_error(format!("push: token: {e}")))?;
        let keep = Duration::from_secs(granted.expires_in.saturating_sub(300));
        *access = Some((granted.access_token.clone(), Instant::now() + keep));
        Ok(granted.access_token)
    }

    async fn send(&self, token: &str, message: &PushMessage) -> Delivery {
        let access = match self.access_token().await {
            Ok(access) => access,
            Err(e) => {
                tracing::warn!("{e}");
                return Delivery::Failed;
            }
        };
        let url = format!(
            "https://fcm.googleapis.com/v1/projects/{}/messages:send",
            self.project
        );
        let body = fcm_body(token, message);
        match self
            .http
            .post(&url)
            .bearer_auth(access)
            .json(&body)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => Delivery::Sent,
            // UNREGISTERED (404) or a token FCM does not know (400 INVALID_ARGUMENT
            // naming the token)
            Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => Delivery::Unregistered,
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                if status == reqwest::StatusCode::BAD_REQUEST && text.contains("registration token")
                {
                    return Delivery::Unregistered;
                }
                tracing::warn!("push: FCM answered {status}");
                Delivery::Failed
            }
            Err(e) => {
                tracing::warn!("push: {e}");
                Delivery::Failed
            }
        }
    }
}

/// The FCM v1 request: a notification, the link as data, high priority off
fn fcm_body(token: &str, message: &PushMessage) -> serde_json::Value {
    serde_json::json!({
        "message": {
            "token": token,
            "notification": { "title": message.title, "body": message.body },
            "data": { "url": message.url },
            "android": { "notification": { "click_action": "OPEN_URL" } },
            "apns": { "payload": { "aps": { "sound": "default" } } },
        }
    })
}

/// A registered device
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct PushDevice {
    pub id: Uuid,
    pub platform: String,
    #[serde(skip_serializing)]
    pub token: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
}

/// What an app sends to register
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewDevice {
    /// `android` or `ios`
    pub platform: String,
    /// The FCM registration token
    pub token: String,
}

/// Register (or refresh) a device for `customer_id`. A token already registered
/// moves to this customer (one phone, the account signed in last). Beyond
/// [`MAX_DEVICES`] the oldest is forgotten.
pub async fn register(pool: &PgPool, customer_id: Uuid, device: &NewDevice) -> Result<PushDevice> {
    if !matches!(device.platform.as_str(), "android" | "ios") {
        return Err(Error::validation("platform: android or ios"));
    }
    let token = device.token.trim();
    if token.is_empty() || token.len() > 4096 || token.chars().any(char::is_control) {
        return Err(Error::validation("token: 1 to 4096 printable characters"));
    }
    let mut tx = pool.begin().await?;
    let saved = sqlx::query_as::<_, PushDevice>(
        "INSERT INTO push_devices (customer_id, platform, token) VALUES ($1, $2, $3) \
         ON CONFLICT (token) DO UPDATE SET customer_id = EXCLUDED.customer_id, \
           platform = EXCLUDED.platform, last_seen_at = NOW() \
         RETURNING id, platform, token, created_at, last_seen_at",
    )
    .bind(customer_id)
    .bind(&device.platform)
    .bind(token)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM push_devices WHERE customer_id = $1 AND id NOT IN \
         (SELECT id FROM push_devices WHERE customer_id = $1 \
          ORDER BY last_seen_at DESC LIMIT $2)",
    )
    .bind(customer_id)
    .bind(MAX_DEVICES)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(saved)
}

/// Forget a device of `customer_id` (signing out of the app); false when unknown
pub async fn unregister(pool: &PgPool, customer_id: Uuid, token: &str) -> Result<bool> {
    let done = sqlx::query("DELETE FROM push_devices WHERE customer_id = $1 AND token = $2")
        .bind(customer_id)
        .bind(token.trim())
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// The devices of a customer, latest first
pub async fn devices(pool: &PgPool, customer_id: Uuid) -> Result<Vec<PushDevice>> {
    Ok(sqlx::query_as::<_, PushDevice>(
        "SELECT id, platform, token, created_at, last_seen_at FROM push_devices \
         WHERE customer_id = $1 ORDER BY last_seen_at DESC",
    )
    .bind(customer_id)
    .fetch_all(pool)
    .await?)
}

/// Send `message` to every device of a customer; devices FCM no longer knows are
/// forgotten. Returns how many received it.
pub async fn notify_customer(
    pool: &PgPool,
    sender: &PushSender,
    customer_id: Uuid,
    message: &PushMessage,
) -> Result<usize> {
    let mut sent = 0;
    for device in devices(pool, customer_id).await? {
        match sender.send(&device.token, message).await {
            Delivery::Sent => sent += 1,
            Delivery::Unregistered => {
                sqlx::query("DELETE FROM push_devices WHERE id = $1")
                    .bind(device.id)
                    .execute(pool)
                    .await?;
            }
            Delivery::Failed => {}
        }
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_provider_sends_nothing_and_unknown_ones_are_refused() {
        assert!(PushSender::from_config(&PushConfig::default())
            .unwrap()
            .is_none());
        let log = PushConfig {
            provider: Some("log".into()),
            ..Default::default()
        };
        assert!(PushSender::from_config(&log).unwrap().is_some());
        let unknown = PushConfig {
            provider: Some("pigeon".into()),
            ..Default::default()
        };
        assert!(PushSender::from_config(&unknown).is_err());
        let fcm = PushConfig {
            provider: Some("fcm".into()),
            ..Default::default()
        };
        assert!(
            PushSender::from_config(&fcm).is_err(),
            "project and key needed"
        );
    }

    #[test]
    fn the_fcm_request_carries_the_texts_and_the_link() {
        let message = PushMessage {
            title: "Votre panier".into(),
            body: "3 articles".into(),
            url: "https://shop.example/cart?lang=fr".into(),
        };
        let body = fcm_body("tok", &message);
        assert_eq!(body["message"]["token"], "tok");
        assert_eq!(body["message"]["notification"]["title"], "Votre panier");
        assert_eq!(body["message"]["data"]["url"], message.url);
    }
}
