//! Peppol e-invoicing: invoices to companies go out as UBL documents (Peppol
//! BIS Billing 3.0, EN 16931) through an access point.
//!
//! Access points sit behind one trait, [`AccessPoint`], so a store picks its
//! provider in `[peppol]`. Peppyrus is the default. The other providers are
//! written from their published APIs; see `docs/architecture/peppol.md` for
//! which ones were tried against a real account.

pub mod providers;
pub mod ubl;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::invoicing::{Invoice, InvoiceService, Party};
use crate::{Error, Result};

/// The Peppol process of invoices and credit notes
pub const PROCESS_TYPE: &str = "cenbii-procid-ubl::urn:fdc:peppol.eu:2017:poacc:billing:01:1.0";
/// The Peppol document type of an invoice
pub const INVOICE_DOCUMENT_TYPE: &str = "busdox-docid-qns::urn:oasis:names:specification:ubl:schema:xsd:Invoice-2::Invoice##urn:cen.eu:en16931:2017#compliant#urn:fdc:peppol.eu:2017:poacc:billing:3.0::2.1";
/// The Peppol document type of a credit note
pub const CREDIT_NOTE_DOCUMENT_TYPE: &str = "busdox-docid-qns::urn:oasis:names:specification:ubl:schema:xsd:CreditNote-2::CreditNote##urn:cen.eu:en16931:2017#compliant#urn:fdc:peppol.eu:2017:poacc:billing:3.0::2.1";

/// The access point a store sends through
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Peppyrus (Belgium, free): the default
    #[default]
    Peppyrus,
    /// Scrada (Belgium)
    Scrada,
    /// e-invoice.be (Belgium)
    EInvoiceBe,
    /// Falco (Belgium)
    Falco,
    /// A-Cube
    Acube,
    /// B2BRouter
    B2bRouter,
    /// Iopole (France)
    Iopole,
    /// SUPER PDP (France)
    SuperPdp,
    /// No network: every participant receives, every document is delivered
    /// at once (development, tests)
    Simulated,
}

/// `[peppol]`: sending invoices to companies through an access point
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeppolConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub provider: Provider,
    /// The provider's production service (default: its test service)
    #[serde(default)]
    pub production: bool,
    /// Replaces the provider's address (a test double)
    #[serde(default)]
    pub base_url: Option<String>,
    /// API key, or `env:NAME` to read it from an environment variable
    #[serde(default)]
    pub api_key: Option<String>,
    /// Second secret: Scrada password, Falco app secret, OAuth client secret
    /// (Iopole, SUPER PDP); `env:NAME` allowed
    #[serde(default)]
    pub api_secret: Option<String>,
    /// OAuth client id (Iopole, SUPER PDP)
    #[serde(default)]
    pub client_id: Option<String>,
    /// Account at the provider (Scrada company id, B2BRouter account id)
    #[serde(default)]
    pub account_id: Option<String>,
    /// The store's participant id; default `0208:<seller company number>`
    #[serde(default)]
    pub sender_id: Option<String>,
    /// Minutes between two checks of documents on their way
    #[serde(default = "default_poll_minutes")]
    pub poll_minutes: u64,
}

fn default_poll_minutes() -> u64 {
    15
}

impl Default for PeppolConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: Provider::default(),
            production: false,
            base_url: None,
            api_key: None,
            api_secret: None,
            client_id: None,
            account_id: None,
            sender_id: None,
            poll_minutes: default_poll_minutes(),
        }
    }
}

/// A secret from the config: the value itself, or `env:NAME`'s environment variable
pub fn secret(value: &Option<String>) -> Option<String> {
    let value = value.as_deref()?.trim();
    match value.strip_prefix("env:") {
        Some(name) => std::env::var(name).ok().filter(|v| !v.is_empty()),
        None => Some(value.to_string()).filter(|v| !v.is_empty()),
    }
}

/// A UBL document on its way out
#[derive(Debug, Clone)]
pub struct OutgoingDocument {
    /// Our participant id (`0208:0123456749`)
    pub sender: String,
    /// The buyer's participant id
    pub recipient: String,
    /// The buyer's country (some providers route by it)
    pub recipient_country: String,
    pub xml: String,
    pub credit_note: bool,
    /// Our reference for it (the invoice number)
    pub reference: String,
}

impl OutgoingDocument {
    /// The Peppol document type of this document
    pub fn document_type(&self) -> &'static str {
        if self.credit_note {
            CREDIT_NOTE_DOCUMENT_TYPE
        } else {
            INVOICE_DOCUMENT_TYPE
        }
    }
}

/// Where a sent document is
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryStatus {
    /// Accepted by our access point, not yet passed on
    Processing,
    /// Passed on to the buyer's access point
    Sent,
    /// Received by the buyer's access point
    Delivered,
    /// Refused or undeliverable, with the reason
    Failed(String),
}

impl DeliveryStatus {
    /// The value stored on the invoice
    pub fn as_str(&self) -> &'static str {
        match self {
            DeliveryStatus::Processing | DeliveryStatus::Sent => "sent",
            DeliveryStatus::Delivered => "delivered",
            DeliveryStatus::Failed(_) => "failed",
        }
    }

    /// A provider's state name read as a delivery status
    pub fn from_state(state: &str) -> Self {
        let lower = state.to_ascii_lowercase();
        if ["fail", "error", "reject", "refus"]
            .iter()
            .any(|w| lower.contains(w))
        {
            DeliveryStatus::Failed(state.to_string())
        } else if [
            "deliver",
            "accept",
            "processed",
            "success",
            "confirm",
            "received",
        ]
        .iter()
        .any(|w| lower.contains(w))
        {
            DeliveryStatus::Delivered
        } else if ["sent", "transmit"].iter().any(|w| lower.contains(w)) {
            DeliveryStatus::Sent
        } else {
            DeliveryStatus::Processing
        }
    }
}

/// An access point: finds participants, sends documents, reports on them
#[async_trait]
pub trait AccessPoint: Send + Sync {
    /// Whether `participant` receives invoices on Peppol; `None` when the
    /// provider cannot tell (the send then reports it)
    async fn lookup(&self, participant: &str) -> Result<Option<bool>>;
    /// Send a document; the provider's id for it
    async fn send(&self, document: &OutgoingDocument) -> Result<String>;
    /// Where a sent document is now
    async fn status(&self, message_id: &str) -> Result<DeliveryStatus>;
}

/// The participant id of a party: its own, else (Belgium) `0208:` and its
/// company number, taken from the VAT number when missing
pub fn participant_of(party: &Party) -> Option<String> {
    if let Some(id) = party.peppol_id.as_deref().filter(|id| id.contains(':')) {
        return Some(id.to_string());
    }
    let from_vat = party
        .vat_number
        .as_deref()
        .filter(|v| v.starts_with("BE"))
        .map(|v| v[2..].to_string());
    let number = party.company_number.clone().or(from_vat)?;
    let digits: String = number.chars().filter(char::is_ascii_digit).collect();
    let belgian = party.country == "BE"
        || party
            .vat_number
            .as_deref()
            .is_some_and(|v| v.starts_with("BE"));
    (belgian && digits.len() == 10).then(|| format!("0208:{digits}"))
}

/// Peppol delivery of invoices for one store
pub struct PeppolService {
    access_point: Box<dyn AccessPoint>,
    sender: String,
}

impl PeppolService {
    /// The service of `config`, or `None` when Peppol is off; the sender id
    /// defaults to the seller's Belgian company number
    pub fn new(config: &PeppolConfig, seller: &Party) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        let sender = config
            .sender_id
            .clone()
            .or_else(|| participant_of(seller))
            .ok_or_else(|| {
                Error::config("peppol.sender_id is required (or invoicing.seller.company_number)")
            })?;
        let access_point = providers::build(config)?;
        Ok(Some(Self {
            access_point,
            sender,
        }))
    }

    /// A service around any access point (tests)
    pub fn with_access_point(access_point: Box<dyn AccessPoint>, sender: String) -> Self {
        Self {
            access_point,
            sender,
        }
    }

    /// Send an invoice waiting for Peppol: check it, find the buyer on Peppol,
    /// send it, and record the outcome on the invoice. Returns the new status.
    pub async fn deliver(
        &self,
        invoices: &InvoiceService,
        invoice: &Invoice,
        pdf: Option<&[u8]>,
    ) -> Result<String> {
        let Some(recipient) = self.recipient(invoices, invoice).await? else {
            return Ok(invoices
                .get(invoice.id)
                .await?
                .and_then(|i| i.peppol_status)
                .unwrap_or_default());
        };
        if let Some(company) = invoice.company_id {
            invoices.remember_peppol_id(company, &recipient).await?;
        }
        let document = OutgoingDocument {
            sender: self.sender.clone(),
            recipient: recipient.clone(),
            recipient_country: invoice.buyer.country.clone(),
            xml: ubl::invoice_xml(invoice, &self.sender, &recipient, pdf),
            credit_note: invoice.kind == "credit_note",
            reference: invoice.number.clone(),
        };
        match self.access_point.send(&document).await {
            Ok(id) => {
                invoices
                    .set_peppol(invoice.id, "sent", Some(&id), Some(&recipient), None)
                    .await?;
                Ok("sent".into())
            }
            Err(Error::Validation(reason)) => {
                invoices
                    .set_peppol(invoice.id, "failed", None, Some(&recipient), Some(&reason))
                    .await?;
                Ok("failed".into())
            }
            Err(e) => Err(e),
        }
    }

    /// The participant to send `invoice` to, after checking the invoice; `None`
    /// (recorded on the invoice) when it cannot or need not go through Peppol
    async fn recipient(
        &self,
        invoices: &InvoiceService,
        invoice: &Invoice,
    ) -> Result<Option<String>> {
        let problems = ubl::check(invoice);
        if !problems.is_empty() {
            let reason = problems.join("; ");
            invoices
                .set_peppol(invoice.id, "failed", None, None, Some(&reason))
                .await?;
            return Ok(None);
        }
        let Some(recipient) = participant_of(&invoice.buyer) else {
            invoices
                .set_peppol(invoice.id, "not_on_peppol", None, None, None)
                .await?;
            return Ok(None);
        };
        if self.access_point.lookup(&recipient).await? == Some(false) {
            invoices
                .set_peppol(invoice.id, "not_on_peppol", None, Some(&recipient), None)
                .await?;
            return Ok(None);
        }
        Ok(Some(recipient))
    }

    /// Ask the access point where a sent invoice is, and record it
    pub async fn refresh(&self, invoices: &InvoiceService, invoice: &Invoice) -> Result<String> {
        let Some(id) = invoice.peppol_message_id.as_deref() else {
            return Ok(invoice.peppol_status.clone().unwrap_or_default());
        };
        let status = self.access_point.status(id).await?;
        let reason = match &status {
            DeliveryStatus::Failed(reason) => Some(reason.as_str()),
            _ => None,
        };
        invoices
            .set_peppol(invoice.id, status.as_str(), None, None, reason)
            .await?;
        Ok(status.as_str().to_string())
    }

    /// One round of the background job: send what waits, then follow what
    /// is on its way. `pdf` renders an invoice's PDF to embed.
    pub async fn run_once<F>(&self, invoices: &InvoiceService, pdf: F) -> Result<()>
    where
        F: Fn(&Invoice) -> Option<Vec<u8>>,
    {
        for invoice in invoices.awaiting_peppol(&["pending"], 50).await? {
            if let Err(e) = self
                .deliver(invoices, &invoice, pdf(&invoice).as_deref())
                .await
            {
                warn!(
                    "Peppol: sending invoice {} failed, retried later: {}",
                    invoice.number, e
                );
            }
        }
        for invoice in invoices.awaiting_peppol(&["sent"], 100).await? {
            if let Err(e) = self.refresh(invoices, &invoice).await {
                warn!(
                    "Peppol: status of invoice {} unknown for now: {}",
                    invoice.number, e
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn belgian_participants_come_from_the_company_number() {
        let party = Party {
            vat_number: Some("BE0123456749".into()),
            country: "BE".into(),
            ..Default::default()
        };
        assert_eq!(participant_of(&party).as_deref(), Some("0208:0123456749"));
        let own = Party {
            peppol_id: Some("9925:BE0123456749".into()),
            ..Default::default()
        };
        assert_eq!(participant_of(&own).as_deref(), Some("9925:BE0123456749"));
        let french = Party {
            vat_number: Some("FR40303265045".into()),
            country: "FR".into(),
            ..Default::default()
        };
        assert_eq!(participant_of(&french), None);
    }

    #[test]
    fn provider_states_read_as_statuses() {
        assert_eq!(
            DeliveryStatus::from_state("DELIVERED"),
            DeliveryStatus::Delivered
        );
        assert_eq!(
            DeliveryStatus::from_state("Processed"),
            DeliveryStatus::Delivered
        );
        assert_eq!(DeliveryStatus::from_state("SENT"), DeliveryStatus::Sent);
        assert!(matches!(
            DeliveryStatus::from_state("Error"),
            DeliveryStatus::Failed(_)
        ));
        assert_eq!(
            DeliveryStatus::from_state("queued"),
            DeliveryStatus::Processing
        );
    }

    #[test]
    fn secrets_can_come_from_the_environment() {
        assert_eq!(secret(&Some("abc".into())).as_deref(), Some("abc"));
        assert_eq!(secret(&Some("env:RCOMMERCE_TEST_UNSET_VAR".into())), None);
        assert_eq!(secret(&None), None);
    }
}
