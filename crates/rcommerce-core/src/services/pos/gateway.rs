//! The payment "gateway" of a till sale. By the time the sale is placed, the
//! cashier has the money: cash in the drawer or an approved card terminal
//! transaction. Checkout still runs its usual course (prices, tax, stock at
//! the branch, the order), and this gateway confirms the payment at once.
//! The till then records how it was paid (see `till_movements`).

use async_trait::async_trait;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Mutex;
use uuid::Uuid;

use crate::payment::{
    CreatePaymentRequest, Payment, PaymentGateway, PaymentSession, PaymentSessionStatus,
    PaymentStatus, Refund, RefundStatus, WebhookEvent,
};
use crate::{Error, Result};

/// A payment started: amount, currency, order, customer
type Started = (Decimal, String, Uuid, Option<Uuid>);

/// Confirms a till sale's payment at once (one instance per sale)
#[derive(Debug, Default)]
pub struct TillGateway {
    /// Payment id → what was started
    payments: Mutex<HashMap<String, Started>>,
}

#[async_trait]
impl PaymentGateway for TillGateway {
    fn id(&self) -> &'static str {
        "till"
    }

    fn name(&self) -> &'static str {
        "Till"
    }

    async fn create_payment(&self, request: CreatePaymentRequest) -> Result<PaymentSession> {
        let id = format!("till_{}", Uuid::new_v4().simple());
        if let Ok(mut payments) = self.payments.lock() {
            payments.insert(
                id.clone(),
                (
                    request.amount,
                    request.currency.clone(),
                    request.order_id,
                    request.customer_id,
                ),
            );
        }
        Ok(PaymentSession {
            id,
            client_secret: String::new(),
            status: PaymentSessionStatus::Complete,
            amount: request.amount,
            currency: request.currency,
            customer_id: request.customer_id,
        })
    }

    async fn confirm_payment(&self, payment_id: &str) -> Result<Payment> {
        let known = self
            .payments
            .lock()
            .ok()
            .and_then(|payments| payments.get(payment_id).cloned());
        let (amount, currency, order_id, customer_id) =
            known.ok_or_else(|| Error::not_found("Unknown till payment"))?;
        let now = chrono::Utc::now();
        Ok(Payment {
            id: payment_id.to_string(),
            gateway: self.id().to_string(),
            amount,
            currency,
            status: PaymentStatus::Succeeded,
            order_id,
            customer_id,
            payment_method: "till".to_string(),
            transaction_id: payment_id.to_string(),
            client_secret: None,
            card_brand: None,
            card_last_four: None,
            captured_at: Some(now),
            created_at: now,
        })
    }

    async fn capture_payment(&self, payment_id: &str, _amount: Option<Decimal>) -> Result<Payment> {
        self.confirm_payment(payment_id).await
    }

    async fn refund_payment(
        &self,
        payment_id: &str,
        amount: Option<Decimal>,
        reason: &str,
    ) -> Result<Refund> {
        // The till pays refunds back itself (cash or terminal)
        Ok(Refund {
            id: format!("till_refund_{}", Uuid::new_v4().simple()),
            payment_id: payment_id.to_string(),
            amount: amount.unwrap_or_default(),
            currency: "EUR".to_string(),
            status: RefundStatus::Succeeded,
            reason: reason.to_string(),
            created_at: chrono::Utc::now(),
        })
    }

    async fn get_payment(&self, payment_id: &str) -> Result<Payment> {
        self.confirm_payment(payment_id).await
    }

    async fn handle_webhook(&self, _payload: &[u8], _signature: &str) -> Result<WebhookEvent> {
        Err(Error::validation("The till has no webhooks"))
    }
}
