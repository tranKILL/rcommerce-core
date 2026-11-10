//! Access point adapters. Each follows its provider's published API: the
//! address of its test and production services, its authentication, and its
//! calls to look up a participant, send a UBL document and read its status.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use reqwest::{Client, RequestBuilder, Response};
use serde_json::Value;

use super::{
    secret, AccessPoint, DeliveryStatus, OutgoingDocument, PeppolConfig, Provider, PROCESS_TYPE,
};
use crate::{Error, Result};

/// The test and production addresses of each provider
fn addresses(provider: Provider) -> (&'static str, &'static str) {
    match provider {
        Provider::Peppyrus => (
            "https://api.test.peppyrus.be/v1",
            "https://api.peppyrus.be/v1",
        ),
        Provider::Scrada => ("https://apitest.scrada.be/v1", "https://api.scrada.be/v1"),
        Provider::EInvoiceBe => ("https://api-dev.e-invoice.be", "https://api.e-invoice.be"),
        Provider::Falco => (
            "https://api.sandbox.falco-app.be/v1",
            "https://api.falco-app.be/v1",
        ),
        Provider::Acube => (
            "https://peppol-sandbox.api.acubeapi.com",
            "https://peppol.api.acubeapi.com",
        ),
        Provider::B2bRouter => (
            "https://api-staging.b2brouter.net",
            "https://api.b2brouter.net",
        ),
        Provider::Iopole => ("https://api.ppd.iopole.fr/v1", "https://api.iopole.fr/v1"),
        Provider::SuperPdp => (
            "https://api.superpdp.tech/v1.beta",
            "https://api.superpdp.tech/v1.beta",
        ),
        Provider::Simulated => ("", ""),
    }
}

/// The settings an adapter is built from
struct Settings<'a> {
    config: &'a PeppolConfig,
    http: Client,
    base: String,
}

impl Settings<'_> {
    fn required(value: Option<String>, name: &str) -> Result<String> {
        value.ok_or_else(|| Error::config(format!("peppol.{name} is required")))
    }

    fn key(&self) -> Result<String> {
        Self::required(secret(&self.config.api_key), "api_key")
    }

    fn secret(&self) -> Result<String> {
        Self::required(secret(&self.config.api_secret), "api_secret")
    }

    fn account(&self) -> Result<String> {
        Self::required(self.config.account_id.clone(), "account_id")
    }

    fn oauth(&self, token_url: &str) -> Result<OAuth> {
        let client_id = Self::required(self.config.client_id.clone(), "client_id")?;
        Ok(OAuth::new(
            self.http.clone(),
            token_url,
            client_id,
            self.secret()?,
        ))
    }
}

/// An adapter needing only an API key
fn keyed(s: &Settings<'_>) -> Result<Box<dyn AccessPoint>> {
    let (http, base, key) = (s.http.clone(), s.base.clone(), s.key()?);
    Ok(match s.config.provider {
        Provider::EInvoiceBe => Box::new(EInvoiceBe { http, base, key }),
        Provider::Acube => Box::new(Acube { http, base, key }),
        _ => Box::new(Peppyrus { http, base, key }),
    })
}

/// An adapter needing a key and a second secret or an account
fn keyed_with_more(s: &Settings<'_>) -> Result<Box<dyn AccessPoint>> {
    let (http, base, key) = (s.http.clone(), s.base.clone(), s.key()?);
    Ok(match s.config.provider {
        Provider::Scrada => Box::new(Scrada {
            http,
            base,
            key,
            password: s.secret()?,
            company: s.account()?,
        }),
        Provider::Falco => Box::new(Falco {
            http,
            base,
            key,
            secret: s.secret()?,
        }),
        _ => Box::new(B2bRouter {
            http,
            base,
            key,
            account: s.account()?,
        }),
    })
}

/// An adapter authenticated by OAuth client credentials
fn oauth(s: &Settings<'_>) -> Result<Box<dyn AccessPoint>> {
    let (http, base) = (s.http.clone(), s.base.clone());
    Ok(match s.config.provider {
        Provider::Iopole => {
            let realm = if s.config.production {
                "auth.iopole.fr"
            } else {
                "auth.ppd.iopole.fr"
            };
            let token = format!("https://{realm}/realms/iopole/protocol/openid-connect/token");
            Box::new(Iopole {
                http,
                base,
                oauth: s.oauth(&token)?,
            })
        }
        _ => Box::new(SuperPdp {
            http,
            base,
            oauth: s.oauth("https://api.superpdp.tech/oauth2/token")?,
        }),
    })
}

/// The access point `config` names
pub fn build(config: &PeppolConfig) -> Result<Box<dyn AccessPoint>> {
    let http = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| Error::config(format!("peppol http client: {e}")))?;
    let (test, production) = addresses(config.provider);
    let url = config
        .base_url
        .clone()
        .unwrap_or_else(|| if config.production { production } else { test }.to_string());
    let settings = Settings {
        config,
        http,
        base: format!("{}/", url.trim_end_matches('/')),
    };
    match config.provider {
        Provider::Peppyrus | Provider::EInvoiceBe | Provider::Acube => keyed(&settings),
        Provider::Scrada | Provider::Falco | Provider::B2bRouter => keyed_with_more(&settings),
        Provider::Iopole | Provider::SuperPdp => oauth(&settings),
        Provider::Simulated => Ok(Box::new(Simulated)),
    }
}

/// The response's JSON, or the error it carries: 400/422 are the document's
/// fault (validation), anything else the service's (network)
async fn answer(response: std::result::Result<Response, reqwest::Error>) -> Result<(u16, String)> {
    let response =
        response.map_err(|e| Error::network(format!("access point unreachable: {e}")))?;
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    match status {
        200..=299 | 404 => Ok((status, body)),
        400 | 422 => Err(Error::validation(format!(
            "access point refused the document: {}",
            truncate(&body)
        ))),
        _ => Err(Error::network(format!(
            "access point answered {status}: {}",
            truncate(&body)
        ))),
    }
}

fn truncate(text: &str) -> String {
    text.chars().take(500).collect()
}

fn json(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

/// The first string found at one of `paths` (`"invoice.id"`)
fn find(value: &Value, paths: &[&str]) -> Option<String> {
    paths.iter().find_map(|path| {
        let node = path.split('.').try_fold(value, |node, key| node.get(key))?;
        match node {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    })
}

fn sent_id(body: &str, paths: &[&str]) -> Result<String> {
    find(&json(body), paths)
        .or_else(|| {
            Some(body.trim().trim_matches('"').to_string())
                .filter(|s| !s.is_empty() && !s.starts_with('{'))
        })
        .ok_or_else(|| Error::network("access point accepted the document without an id"))
}

/// A `file` part with the UBL document, for multipart uploads
fn ubl_form(document: &OutgoingDocument) -> reqwest::multipart::Form {
    let part = reqwest::multipart::Part::bytes(document.xml.clone().into_bytes())
        .file_name(format!("{}.xml", document.reference))
        .mime_str("application/xml")
        .unwrap_or_else(|_| reqwest::multipart::Part::bytes(Vec::new()));
    reqwest::multipart::Form::new().part("file", part)
}

/// Peppyrus: `X-Api-Key`; JSON message with the base64 document
struct Peppyrus {
    http: Client,
    base: String,
    key: String,
}

impl Peppyrus {
    fn get(&self, path: &str) -> RequestBuilder {
        self.http
            .get(format!("{}{path}", self.base))
            .header("X-Api-Key", &self.key)
    }
}

#[async_trait]
impl AccessPoint for Peppyrus {
    async fn lookup(&self, participant: &str) -> Result<Option<bool>> {
        let (status, _) = answer(
            self.get("peppol/lookup")
                .query(&[("participantId", participant)])
                .send()
                .await,
        )
        .await?;
        Ok(Some(status != 404))
    }

    async fn send(&self, document: &OutgoingDocument) -> Result<String> {
        let body = serde_json::json!({
            "sender": document.sender,
            "recipient": document.recipient,
            "processType": PROCESS_TYPE,
            "documentType": document.document_type(),
            "fileContent": base64::engine::general_purpose::STANDARD.encode(&document.xml),
        });
        let request = self
            .http
            .post(format!("{}message", self.base))
            .header("X-Api-Key", &self.key)
            .json(&body);
        let (_, body) = answer(request.send().await).await?;
        sent_id(&body, &["id"])
    }

    async fn status(&self, message_id: &str) -> Result<DeliveryStatus> {
        let (status, body) =
            answer(self.get(&format!("message/{message_id}")).send().await).await?;
        if status == 404 {
            return Ok(DeliveryStatus::Failed("unknown message".into()));
        }
        let message = json(&body);
        let folder = find(&message, &["folder"]).unwrap_or_default();
        if message.get("confirmed").and_then(Value::as_bool) == Some(true) {
            return Ok(DeliveryStatus::Delivered);
        }
        Ok(match folder.as_str() {
            "failed" => {
                let (_, report) = answer(
                    self.get(&format!("message/{message_id}/report"))
                        .send()
                        .await,
                )
                .await?;
                DeliveryStatus::Failed(truncate(&report))
            }
            "outbox" => DeliveryStatus::Processing,
            _ => DeliveryStatus::Sent,
        })
    }
}

/// Scrada: `X-API-KEY` and `X-PASSWORD`; the raw document with Peppol headers
struct Scrada {
    http: Client,
    base: String,
    key: String,
    password: String,
    company: String,
}

impl Scrada {
    fn request(&self, method: reqwest::Method, path: &str) -> RequestBuilder {
        self.http
            .request(
                method,
                format!("{}company/{}/{path}", self.base, self.company),
            )
            .header("X-API-KEY", &self.key)
            .header("X-PASSWORD", &self.password)
    }
}

#[async_trait]
impl AccessPoint for Scrada {
    async fn lookup(&self, participant: &str) -> Result<Option<bool>> {
        let body = serde_json::json!({ "peppolID": participant });
        let (status, _) = answer(
            self.request(reqwest::Method::POST, "peppol/lookup")
                .json(&body)
                .send()
                .await,
        )
        .await?;
        Ok(Some(status != 404))
    }

    async fn send(&self, document: &OutgoingDocument) -> Result<String> {
        let (doc_scheme, doc_value) = document
            .document_type()
            .split_once("::")
            .unwrap_or(("busdox-docid-qns", ""));
        let request = self
            .request(reqwest::Method::POST, "peppol/outbound/document")
            .header("x-scrada-peppol-sender-scheme", "iso6523-actorid-upis")
            .header("x-scrada-peppol-sender-id", &document.sender)
            .header("x-scrada-peppol-receiver-scheme", "iso6523-actorid-upis")
            .header("x-scrada-peppol-receiver-id", &document.recipient)
            .header(
                "x-scrada-peppol-c1-country-code",
                &document.recipient_country,
            )
            .header("x-scrada-peppol-document-type-scheme", doc_scheme)
            .header("x-scrada-peppol-document-type-value", doc_value)
            .header("x-scrada-peppol-process-scheme", "cenbii-procid-ubl")
            .header(
                "x-scrada-peppol-process-value",
                "urn:fdc:peppol.eu:2017:poacc:billing:01:1.0",
            )
            .header("x-scrada-external-reference", &document.reference)
            .header("Content-Type", "text/xml")
            .body(document.xml.clone());
        let (_, body) = answer(request.send().await).await?;
        sent_id(&body, &["id"])
    }

    async fn status(&self, message_id: &str) -> Result<DeliveryStatus> {
        let path = format!("peppol/outbound/document/{message_id}/info");
        let (_, body) = answer(self.request(reqwest::Method::GET, &path).send().await).await?;
        let info = json(&body);
        let state = find(&info, &["status"]).unwrap_or_default();
        Ok(match DeliveryStatus::from_state(&state) {
            DeliveryStatus::Failed(_) => {
                DeliveryStatus::Failed(find(&info, &["errorMessage"]).unwrap_or(state))
            }
            other => other,
        })
    }
}

/// e-invoice.be: bearer key; upload the document, then send it
struct EInvoiceBe {
    http: Client,
    base: String,
    key: String,
}

#[async_trait]
impl AccessPoint for EInvoiceBe {
    async fn lookup(&self, participant: &str) -> Result<Option<bool>> {
        let request = self
            .http
            .get(format!("{}api/validate/peppol-id", self.base))
            .bearer_auth(&self.key)
            .query(&[("peppol_id", participant)]);
        let (status, body) = answer(request.send().await).await?;
        Ok(Some(
            status != 404
                && json(&body)
                    .get("is_valid")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
        ))
    }

    async fn send(&self, document: &OutgoingDocument) -> Result<String> {
        let upload = self
            .http
            .post(format!("{}api/documents/ubl", self.base))
            .bearer_auth(&self.key)
            .multipart(ubl_form(document));
        let (_, body) = answer(upload.send().await).await?;
        let id = sent_id(&body, &["id"])?;
        let send = self
            .http
            .post(format!("{}api/documents/{id}/send", self.base))
            .bearer_auth(&self.key)
            .query(&[
                ("sender_peppol_scheme", "iso6523-actorid-upis"),
                ("sender_peppol_id", document.sender.as_str()),
                ("receiver_peppol_scheme", "iso6523-actorid-upis"),
                ("receiver_peppol_id", document.recipient.as_str()),
            ]);
        answer(send.send().await).await?;
        Ok(id)
    }

    async fn status(&self, message_id: &str) -> Result<DeliveryStatus> {
        let request = self
            .http
            .get(format!("{}api/documents/{message_id}", self.base))
            .bearer_auth(&self.key);
        let (_, body) = answer(request.send().await).await?;
        Ok(DeliveryStatus::from_state(
            &find(&json(&body), &["state"]).unwrap_or_default(),
        ))
    }
}

/// Falco: API key and app secret headers; JSON import with `send`
struct Falco {
    http: Client,
    base: String,
    key: String,
    secret: String,
}

impl Falco {
    fn request(&self, method: reqwest::Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .header("X-Falco-Api-Key", &self.key)
            .header("X-Falco-App-Secret", &self.secret)
    }
}

#[async_trait]
impl AccessPoint for Falco {
    async fn lookup(&self, participant: &str) -> Result<Option<bool>> {
        let request = self
            .request(reqwest::Method::GET, "peppol/lookup")
            .query(&[("participant_id", participant)]);
        let (status, _) = answer(request.send().await).await?;
        Ok(Some(status != 404))
    }

    async fn send(&self, document: &OutgoingDocument) -> Result<String> {
        let body = serde_json::json!({
            "ubl_content": base64::engine::general_purpose::STANDARD.encode(&document.xml),
            "filename": format!("{}.xml", document.reference),
            "send": true,
        });
        let (_, body) = answer(
            self.request(reqwest::Method::POST, "invoices/imports/ubl")
                .json(&body)
                .send()
                .await,
        )
        .await?;
        sent_id(&body, &["id", "invoice_id"])
    }

    async fn status(&self, message_id: &str) -> Result<DeliveryStatus> {
        let (_, body) = answer(
            self.request(reqwest::Method::GET, &format!("invoices/{message_id}"))
                .send()
                .await,
        )
        .await?;
        Ok(DeliveryStatus::from_state(
            &find(&json(&body), &["status", "peppol_status", "state"])
                .unwrap_or_else(|| "sent".into()),
        ))
    }
}

/// A-Cube: bearer key; the raw document per document kind
struct Acube {
    http: Client,
    base: String,
    key: String,
}

#[async_trait]
impl AccessPoint for Acube {
    async fn lookup(&self, _participant: &str) -> Result<Option<bool>> {
        Ok(None)
    }

    async fn send(&self, document: &OutgoingDocument) -> Result<String> {
        let kind = if document.credit_note {
            "credit-notes"
        } else {
            "invoices"
        };
        let request = self
            .http
            .post(format!("{}{kind}/outgoing/ubl", self.base))
            .bearer_auth(&self.key)
            .header("Content-Type", "application/xml")
            .header("Accept", "application/json")
            .body(document.xml.clone());
        let (_, body) = answer(request.send().await).await?;
        sent_id(&body, &["uuid"])
    }

    async fn status(&self, message_id: &str) -> Result<DeliveryStatus> {
        let request = self
            .http
            .get(format!("{}invoices/{message_id}", self.base))
            .bearer_auth(&self.key);
        let (_, body) = answer(request.send().await).await?;
        Ok(DeliveryStatus::from_state(
            &find(&json(&body), &["status", "state"]).unwrap_or_else(|| "sent".into()),
        ))
    }
}

/// B2BRouter: API key and version headers; import with sending
struct B2bRouter {
    http: Client,
    base: String,
    key: String,
    account: String,
}

impl B2bRouter {
    fn request(&self, method: reqwest::Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .header("X-B2B-API-Key", &self.key)
            .header("X-B2B-API-Version", "2025-10-13")
            .header("Accept", "application/json")
    }
}

#[async_trait]
impl AccessPoint for B2bRouter {
    async fn lookup(&self, participant: &str) -> Result<Option<bool>> {
        let (scheme, id) = participant.split_once(':').unwrap_or(("0208", participant));
        let (status, _) = answer(
            self.request(reqwest::Method::GET, &format!("directory/{scheme}/{id}"))
                .send()
                .await,
        )
        .await?;
        Ok(Some(status != 404))
    }

    async fn send(&self, document: &OutgoingDocument) -> Result<String> {
        let path = format!(
            "accounts/{}/invoices/import?send_after_import=true",
            self.account
        );
        let request = self
            .request(reqwest::Method::POST, &path)
            .header("Content-Type", "application/octet-stream")
            .body(document.xml.clone());
        let (_, body) = answer(request.send().await).await?;
        sent_id(&body, &["invoice.id", "id"])
    }

    async fn status(&self, message_id: &str) -> Result<DeliveryStatus> {
        let (_, body) = answer(
            self.request(reqwest::Method::GET, &format!("invoices/{message_id}"))
                .send()
                .await,
        )
        .await?;
        Ok(DeliveryStatus::from_state(
            &find(&json(&body), &["invoice.state", "state"]).unwrap_or_default(),
        ))
    }
}

/// OAuth client credentials, the token kept until shortly before it expires
struct OAuth {
    http: Client,
    token_url: String,
    client_id: String,
    client_secret: String,
    token: Mutex<Option<(String, Instant)>>,
}

impl OAuth {
    fn new(http: Client, token_url: &str, client_id: String, client_secret: String) -> Self {
        Self {
            http,
            token_url: token_url.to_string(),
            client_id,
            client_secret,
            token: Mutex::new(None),
        }
    }

    async fn token(&self) -> Result<String> {
        if let Some((token, until)) = self.token.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            if Instant::now() < until {
                return Ok(token);
            }
        }
        let form = [
            ("grant_type", "client_credentials"),
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
        ];
        let (_, body) = answer(self.http.post(&self.token_url).form(&form).send().await).await?;
        let reply = json(&body);
        let token = find(&reply, &["access_token"])
            .ok_or_else(|| Error::network("access point gave no token"))?;
        let lifetime = reply
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(300)
            .saturating_sub(30);
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) = Some((
            token.clone(),
            Instant::now() + Duration::from_secs(lifetime),
        ));
        Ok(token)
    }
}

/// Iopole: OAuth; multipart upload of the document
struct Iopole {
    http: Client,
    base: String,
    oauth: OAuth,
}

#[async_trait]
impl AccessPoint for Iopole {
    async fn lookup(&self, _participant: &str) -> Result<Option<bool>> {
        Ok(None)
    }

    async fn send(&self, document: &OutgoingDocument) -> Result<String> {
        let request = self
            .http
            .post(format!("{}invoice", self.base))
            .bearer_auth(self.oauth.token().await?)
            .multipart(ubl_form(document));
        let (_, body) = answer(request.send().await).await?;
        sent_id(&body, &["id"])
    }

    async fn status(&self, message_id: &str) -> Result<DeliveryStatus> {
        let request = self
            .http
            .get(format!("{}invoice/{message_id}", self.base))
            .bearer_auth(self.oauth.token().await?);
        let (_, body) = answer(request.send().await).await?;
        Ok(DeliveryStatus::from_state(
            &find(&json(&body), &["invoiceState", "state"]).unwrap_or_default(),
        ))
    }
}

/// SUPER PDP: OAuth; multipart upload; status from the last event
struct SuperPdp {
    http: Client,
    base: String,
    oauth: OAuth,
}

#[async_trait]
impl AccessPoint for SuperPdp {
    async fn lookup(&self, _participant: &str) -> Result<Option<bool>> {
        Ok(None)
    }

    async fn send(&self, document: &OutgoingDocument) -> Result<String> {
        let request = self
            .http
            .post(format!("{}invoices", self.base))
            .bearer_auth(self.oauth.token().await?)
            .multipart(ubl_form(document));
        let (_, body) = answer(request.send().await).await?;
        sent_id(&body, &["id"])
    }

    async fn status(&self, message_id: &str) -> Result<DeliveryStatus> {
        let request = self
            .http
            .get(format!("{}invoices/{message_id}", self.base))
            .query(&[("expand[]", "events")])
            .bearer_auth(self.oauth.token().await?);
        let (_, body) = answer(request.send().await).await?;
        let reply = json(&body);
        let last = reply
            .get("events")
            .and_then(Value::as_array)
            .and_then(|events| events.last())
            .cloned()
            .unwrap_or(Value::Null);
        Ok(DeliveryStatus::from_state(
            &find(&last, &["status_code"]).unwrap_or_default(),
        ))
    }
}

/// No network: every participant receives, every document is delivered
struct Simulated;

#[async_trait]
impl AccessPoint for Simulated {
    async fn lookup(&self, _participant: &str) -> Result<Option<bool>> {
        Ok(Some(true))
    }

    async fn send(&self, document: &OutgoingDocument) -> Result<String> {
        if !document.xml.contains("<cbc:CustomizationID>") {
            return Err(Error::validation("not a Peppol BIS document"));
        }
        Ok(format!("simulated-{}", document.reference))
    }

    async fn status(&self, _message_id: &str) -> Result<DeliveryStatus> {
        Ok(DeliveryStatus::Delivered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_found_in_json_or_text() {
        assert_eq!(sent_id(r#"{"id":"m-1"}"#, &["id"]).unwrap(), "m-1");
        assert_eq!(
            sent_id(r#"{"invoice":{"id":42}}"#, &["invoice.id"]).unwrap(),
            "42"
        );
        assert_eq!(sent_id("\"3f2a\"", &["id"]).unwrap(), "3f2a");
        assert!(sent_id("{}", &["id"]).is_err());
    }

    #[test]
    fn every_provider_needs_its_credentials() {
        let mut config = PeppolConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(build(&config).is_err(), "Peppyrus without a key");
        config.api_key = Some("k".into());
        assert!(build(&config).is_ok());
        config.provider = Provider::Scrada;
        assert!(
            build(&config).is_err(),
            "Scrada needs a password and a company"
        );
        config.provider = Provider::Simulated;
        config.api_key = None;
        assert!(build(&config).is_ok());
    }
}
