//! Card terminals at the till. A [`PaymentTerminal`] starts a payment or a
//! refund for an amount and reports how it ends; the till records every
//! transaction in `terminal_transactions` and pays a sale only with an
//! approved one.
//!
//! [`SimulatedTerminal`] answers at once, for development and tests:
//! amounts ending in ,13 are declined, amounts ending in ,14 stay pending
//! until cancelled, every other amount is approved. A Worldline adapter comes
//! with the merchant's terminal documentation; until then
//! [`UnavailableTerminal`] refuses card payments with a clear message.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::{Error, Result};

/// How a terminal transaction stands
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalStatus {
    Pending,
    Approved,
    Declined,
    Cancelled,
}

impl TerminalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TerminalStatus::Pending => "pending",
            TerminalStatus::Approved => "approved",
            TerminalStatus::Declined => "declined",
            TerminalStatus::Cancelled => "cancelled",
        }
    }

    fn parse(text: &str) -> TerminalStatus {
        match text {
            "approved" => TerminalStatus::Approved,
            "declined" => TerminalStatus::Declined,
            "cancelled" => TerminalStatus::Cancelled,
            _ => TerminalStatus::Pending,
        }
    }
}

/// What the terminal says about a transaction
#[derive(Debug, Clone)]
pub struct TerminalState {
    /// The provider's reference
    pub reference: String,
    pub status: TerminalStatus,
    /// The provider's answer, kept as is
    pub raw: serde_json::Value,
}

/// A card terminal
#[async_trait]
pub trait PaymentTerminal: Send + Sync {
    /// The provider's name, stored with each transaction
    fn provider(&self) -> &'static str;
    /// Ask the terminal to take (`refund`: give back) `amount`
    async fn start(&self, refund: bool, amount: Decimal) -> Result<TerminalState>;
    /// How a pending transaction stands now
    async fn status(&self, reference: &str) -> Result<TerminalState>;
    /// Stop a pending transaction
    async fn cancel(&self, reference: &str) -> Result<TerminalState>;
}

/// Answers at once (see the module docs)
pub struct SimulatedTerminal;

#[async_trait]
impl PaymentTerminal for SimulatedTerminal {
    fn provider(&self) -> &'static str {
        "simulated"
    }

    async fn start(&self, refund: bool, amount: Decimal) -> Result<TerminalState> {
        let cents = (amount * Decimal::ONE_HUNDRED).round() % Decimal::ONE_HUNDRED;
        let status = if cents == Decimal::from(13) {
            TerminalStatus::Declined
        } else if cents == Decimal::from(14) {
            TerminalStatus::Pending
        } else {
            TerminalStatus::Approved
        };
        let reference = format!("SIM-{}", Uuid::new_v4().simple());
        Ok(TerminalState {
            raw: serde_json::json!({ "simulated": true, "refund": refund, "status": status }),
            reference,
            status,
        })
    }

    async fn status(&self, reference: &str) -> Result<TerminalState> {
        Ok(TerminalState {
            reference: reference.to_string(),
            status: TerminalStatus::Pending,
            raw: serde_json::json!({ "simulated": true }),
        })
    }

    async fn cancel(&self, reference: &str) -> Result<TerminalState> {
        Ok(TerminalState {
            reference: reference.to_string(),
            status: TerminalStatus::Cancelled,
            raw: serde_json::json!({ "simulated": true, "cancelled": true }),
        })
    }
}

/// No terminal configured yet: card payments at the till are refused
pub struct UnavailableTerminal(pub &'static str);

#[async_trait]
impl PaymentTerminal for UnavailableTerminal {
    fn provider(&self) -> &'static str {
        self.0
    }

    async fn start(&self, _refund: bool, _amount: Decimal) -> Result<TerminalState> {
        Err(Error::validation(format!(
            "The {} card terminal is not set up yet: take the payment in cash",
            self.0
        )))
    }

    async fn status(&self, reference: &str) -> Result<TerminalState> {
        Err(Error::not_found(format!(
            "Unknown terminal transaction {reference}"
        )))
    }

    async fn cancel(&self, reference: &str) -> Result<TerminalState> {
        self.status(reference).await
    }
}

/// The terminal named in the configuration (`simulated` by default)
pub fn from_config(name: &str) -> Arc<dyn PaymentTerminal> {
    match name {
        "worldline" => Arc::new(UnavailableTerminal("worldline")),
        _ => Arc::new(SimulatedTerminal),
    }
}

/// A transaction as the till records it
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TerminalTransaction {
    pub id: Uuid,
    pub session_id: Uuid,
    pub provider: String,
    pub reference: String,
    pub kind: String,
    pub amount: Decimal,
    pub status: String,
    pub order_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const TRANSACTION_COLUMNS: &str = "id, session_id, provider, reference, kind, amount, status, \
    order_id, created_at, updated_at";

/// Terminal transactions of the till's sessions
#[derive(Clone)]
pub struct Terminals {
    pool: PgPool,
    terminal: Arc<dyn PaymentTerminal>,
}

impl Terminals {
    pub fn new(pool: PgPool, terminal: Arc<dyn PaymentTerminal>) -> Self {
        Self { pool, terminal }
    }

    /// Start a payment (or refund) on the terminal for an open session
    pub async fn start(
        &self,
        session: Uuid,
        refund: bool,
        amount: Decimal,
    ) -> Result<TerminalTransaction> {
        if amount <= Decimal::ZERO {
            return Err(Error::validation("The amount must be positive"));
        }
        let amount = amount.round_dp(2);
        let state = self.terminal.start(refund, amount).await?;
        let transaction = sqlx::query_as(&format!(
            "INSERT INTO terminal_transactions (session_id, provider, reference, kind, amount, status, raw) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING {TRANSACTION_COLUMNS}"
        ))
        .bind(session)
        .bind(self.terminal.provider())
        .bind(&state.reference)
        .bind(if refund { "refund" } else { "payment" })
        .bind(amount)
        .bind(state.status.as_str())
        .bind(&state.raw)
        .fetch_one(&self.pool)
        .await?;
        Ok(transaction)
    }

    /// A transaction, asking the terminal again while it is pending
    pub async fn get(&self, id: Uuid) -> Result<TerminalTransaction> {
        let transaction = self.stored(id).await?;
        if TerminalStatus::parse(&transaction.status) != TerminalStatus::Pending {
            return Ok(transaction);
        }
        let state = self.terminal.status(&transaction.reference).await?;
        self.update(id, &state).await
    }

    /// Stop a pending transaction
    pub async fn cancel(&self, id: Uuid) -> Result<TerminalTransaction> {
        let transaction = self.stored(id).await?;
        if TerminalStatus::parse(&transaction.status) != TerminalStatus::Pending {
            return Err(Error::validation(
                "Only a pending card payment can be cancelled",
            ));
        }
        let state = self.terminal.cancel(&transaction.reference).await?;
        self.update(id, &state).await
    }

    async fn stored(&self, id: Uuid) -> Result<TerminalTransaction> {
        sqlx::query_as(&format!(
            "SELECT {TRANSACTION_COLUMNS} FROM terminal_transactions WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| Error::not_found("Card payment not found"))
    }

    async fn update(&self, id: Uuid, state: &TerminalState) -> Result<TerminalTransaction> {
        let transaction = sqlx::query_as(&format!(
            "UPDATE terminal_transactions SET status = $2, raw = $3, updated_at = NOW() \
             WHERE id = $1 RETURNING {TRANSACTION_COLUMNS}"
        ))
        .bind(id)
        .bind(state.status.as_str())
        .bind(&state.raw)
        .fetch_one(&self.pool)
        .await?;
        Ok(transaction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_simulated_terminal_answers_by_the_cents() {
        let terminal = SimulatedTerminal;
        let status = |cents: i64| async move {
            terminal_status(&SimulatedTerminal, Decimal::new(cents, 2)).await
        };
        assert_eq!(status(4_250).await, TerminalStatus::Approved);
        assert_eq!(status(1_013).await, TerminalStatus::Declined);
        assert_eq!(status(1_014).await, TerminalStatus::Pending);
        let cancelled = terminal.cancel("SIM-x").await.unwrap();
        assert_eq!(cancelled.status, TerminalStatus::Cancelled);
    }

    async fn terminal_status(terminal: &dyn PaymentTerminal, amount: Decimal) -> TerminalStatus {
        terminal.start(false, amount).await.unwrap().status
    }

    #[tokio::test]
    async fn an_unconfigured_terminal_refuses_with_a_reason() {
        let error = from_config("worldline")
            .start(false, Decimal::ONE)
            .await
            .unwrap_err();
        assert_eq!(error.status_code(), 400);
        assert_eq!(from_config("anything").provider(), "simulated");
    }
}
