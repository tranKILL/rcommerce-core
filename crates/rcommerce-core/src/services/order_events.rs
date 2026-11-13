//! An order's history, from start to end: placed, paid, preparing, ready at
//! the counter, collected (by whom), out for delivery (driver), delivered
//! (received by), cancelled (why), refunded (how much), invoiced. Each event
//! keeps who did it (the customer, a staff member, or the system) and when.
//!
//! Staff see every name. Customers see the steps, dates and the people named
//! for collection or receipt, not the names of staff members.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::Result;

/// Who did something
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    Customer(Option<Uuid>),
    Staff(Uuid),
    System,
}

impl Actor {
    fn parts(self) -> (&'static str, Option<Uuid>) {
        match self {
            Actor::Customer(id) => ("customer", id),
            Actor::Staff(id) => ("staff", Some(id)),
            Actor::System => ("system", None),
        }
    }
}

/// An event of the history
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OrderEvent {
    pub kind: String,
    /// `customer`, `staff` or `system`
    pub actor_role: String,
    /// The person's name (staff names only for staff readers)
    pub actor_name: Option<String>,
    /// What the step says: `collected_by`, `driver`, `received_by`,
    /// `location`, `reason`, `amount`, `invoice_number`, ...
    pub detail: Json<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// Record an event; the actor's name is taken from their account (the
/// order's own contact for a guest)
pub async fn record(
    conn: &mut PgConnection,
    order: Uuid,
    kind: &str,
    actor: Actor,
    detail: serde_json::Value,
) -> Result<()> {
    let (role, id) = actor.parts();
    sqlx::query(
        "INSERT INTO order_events (order_id, kind, actor_role, actor_id, actor_name, detail) \
         SELECT $1, $2, $3, $4, \
           COALESCE((SELECT NULLIF(btrim(c.first_name || ' ' || c.last_name), '') FROM customers c WHERE c.id = $4), \
                    CASE WHEN $3 = 'customer' THEN (SELECT NULLIF(btrim(COALESCE(o.shipping_address->>'first_name', '') \
                      || ' ' || COALESCE(o.shipping_address->>'last_name', '')), '') FROM orders o WHERE o.id = $1) END), \
           $5",
    )
    .bind(order)
    .bind(kind)
    .bind(role)
    .bind(id)
    .bind(Json(detail))
    .execute(conn)
    .await?;
    Ok(())
}

/// The same, on a pool; a failure is logged, never fatal to the step itself
pub async fn record_quietly(
    pool: &PgPool,
    order: Uuid,
    kind: &str,
    actor: Actor,
    detail: serde_json::Value,
) {
    let result = match pool.acquire().await {
        Ok(mut conn) => record(&mut conn, order, kind, actor, detail).await,
        Err(e) => Err(e.into()),
    };
    if let Err(e) = result {
        tracing::warn!("Failed to record the {kind} event of order {order}: {e}");
    }
}

/// An order's history, oldest first; `for_staff` keeps staff names
pub async fn history(pool: &PgPool, order: Uuid, for_staff: bool) -> Result<Vec<OrderEvent>> {
    Ok(sqlx::query_as::<_, OrderEvent>(
        "SELECT kind, actor_role, \
           CASE WHEN actor_role = 'staff' AND NOT $2 THEN NULL ELSE actor_name END AS actor_name, \
           detail, created_at FROM order_events WHERE order_id = $1 ORDER BY created_at, id",
    )
    .bind(order)
    .bind(for_staff)
    .fetch_all(pool)
    .await?)
}
