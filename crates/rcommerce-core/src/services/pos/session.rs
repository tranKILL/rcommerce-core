//! Till sessions: a cashier opens one at a branch with a cash float, takes
//! sales and refunds, puts cash in or takes it out, then counts the drawer at
//! closing. The Z report sums the session's movements per method and compares
//! the cash expected with the cash counted.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{Error, Result};

/// A till session
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TillSession {
    pub id: Uuid,
    pub location_id: Uuid,
    pub location_name: String,
    pub cashier_id: Uuid,
    pub cashier_name: String,
    pub opening_float: Decimal,
    pub opened_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub counted_cash: Option<Decimal>,
    pub expected_cash: Option<Decimal>,
    pub difference: Option<Decimal>,
    pub z_number: Option<i32>,
    pub notes: Option<String>,
}

/// Money in (positive) or out (negative) of a session
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TillMovement {
    pub id: Uuid,
    pub kind: String,
    pub method: String,
    pub amount: Decimal,
    pub order_id: Option<Uuid>,
    pub order_number: Option<String>,
    pub reason: Option<String>,
    pub staff_name: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Count and amount of one kind of movement by one method
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MethodTotal {
    pub method: String,
    pub count: i64,
    pub amount: Decimal,
}

/// A session's totals: what the Z report prints
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TillTotals {
    /// Sales per method (amounts received)
    pub sales: Vec<MethodTotal>,
    /// Refunds per method (amounts paid back, as positive numbers)
    pub refunds: Vec<MethodTotal>,
    pub cash_in: Decimal,
    pub cash_out: Decimal,
    /// Sales minus refunds, every method
    pub net: Decimal,
    /// Opening float plus every cash movement
    pub expected_cash: Decimal,
}

/// A session with its totals and movements
#[derive(Debug, Clone, Serialize)]
pub struct ZReport {
    pub session: TillSession,
    pub totals: TillTotals,
    pub movements: Vec<TillMovement>,
}

/// `cash_in` or `cash_out`, with its reason
#[derive(Debug, Clone, Deserialize)]
pub struct CashMovement {
    pub kind: String,
    pub amount: Decimal,
    #[serde(default)]
    pub reason: Option<String>,
}

const SESSION_COLUMNS: &str = "s.id, s.location_id, l.name AS location_name, s.cashier_id, \
    TRIM(COALESCE(c.first_name, '') || ' ' || COALESCE(c.last_name, '')) AS cashier_name, \
    s.opening_float, s.opened_at, s.closed_at, s.counted_cash, s.expected_cash, s.difference, \
    s.z_number, s.notes";

const SESSION_FROM: &str = "FROM till_sessions s \
    JOIN inventory_locations l ON l.id = s.location_id \
    JOIN customers c ON c.id = s.cashier_id";

/// Add a movement to a session's per-method list
fn add_to(list: &mut Vec<MethodTotal>, method: &str, amount: Decimal) {
    match list.iter_mut().find(|m| m.method == method) {
        Some(total) => {
            total.count += 1;
            total.amount += amount;
        }
        None => list.push(MethodTotal {
            method: method.to_string(),
            count: 1,
            amount,
        }),
    }
}

/// The totals of a session opened with `float` and these movements
pub fn totals(float: Decimal, movements: &[TillMovement]) -> TillTotals {
    let mut totals = TillTotals {
        expected_cash: float,
        ..Default::default()
    };
    for m in movements {
        match m.kind.as_str() {
            "sale" => add_to(&mut totals.sales, &m.method, m.amount),
            "refund" => add_to(&mut totals.refunds, &m.method, -m.amount),
            "cash_in" => totals.cash_in += m.amount,
            "cash_out" => totals.cash_out -= m.amount,
            _ => {}
        }
        if m.kind == "sale" || m.kind == "refund" {
            totals.net += m.amount;
        }
        if m.method == "cash" {
            totals.expected_cash += m.amount;
        }
    }
    totals.sales.sort_by(|a, b| a.method.cmp(&b.method));
    totals.refunds.sort_by(|a, b| a.method.cmp(&b.method));
    totals
}

/// A unique violation on the one-open-session index is a client error
fn already_open(error: sqlx::Error) -> Error {
    match &error {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            Error::validation("You already have an open till session: close it first")
        }
        _ => error.into(),
    }
}

/// Till sessions and their cash
#[derive(Clone)]
pub struct TillSessions {
    pool: PgPool,
}

impl TillSessions {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Open a session for `cashier` at an active branch
    pub async fn open(&self, cashier: Uuid, location: Uuid, float: Decimal) -> Result<TillSession> {
        if float < Decimal::ZERO {
            return Err(Error::validation("The opening float cannot be negative"));
        }
        let active: Option<bool> =
            sqlx::query_scalar("SELECT is_active FROM inventory_locations WHERE id = $1")
                .bind(location)
                .fetch_optional(&self.pool)
                .await?;
        if active != Some(true) {
            return Err(Error::validation("Unknown or closed branch"));
        }
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO till_sessions (location_id, cashier_id, opening_float) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(location)
        .bind(cashier)
        .bind(float.round_dp(2))
        .fetch_one(&self.pool)
        .await
        .map_err(already_open)?;
        self.get(id).await
    }

    /// A session
    pub async fn get(&self, id: Uuid) -> Result<TillSession> {
        sqlx::query_as(&format!(
            "SELECT {SESSION_COLUMNS} {SESSION_FROM} WHERE s.id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| Error::not_found("Till session not found"))
    }

    /// The cashier's open session, if any
    pub async fn current(&self, cashier: Uuid) -> Result<Option<TillSession>> {
        let session = sqlx::query_as(&format!(
            "SELECT {SESSION_COLUMNS} {SESSION_FROM} \
             WHERE s.cashier_id = $1 AND s.closed_at IS NULL"
        ))
        .bind(cashier)
        .fetch_optional(&self.pool)
        .await?;
        Ok(session)
    }

    /// Sessions, newest first, of one branch or all
    pub async fn list(&self, location: Option<Uuid>, limit: i64) -> Result<Vec<TillSession>> {
        let sessions = sqlx::query_as(&format!(
            "SELECT {SESSION_COLUMNS} {SESSION_FROM} \
             WHERE ($1::uuid IS NULL OR s.location_id = $1) ORDER BY s.opened_at DESC LIMIT $2"
        ))
        .bind(location)
        .bind(limit.clamp(1, 200))
        .fetch_all(&self.pool)
        .await?;
        Ok(sessions)
    }

    /// The session, still open, of this cashier (another cashier's or a
    /// closed session is refused)
    pub async fn open_of(&self, id: Uuid, cashier: Uuid) -> Result<TillSession> {
        let session = self.get(id).await?;
        if session.cashier_id != cashier {
            return Err(Error::forbidden(
                "This till session belongs to another cashier",
            ));
        }
        if session.closed_at.is_some() {
            return Err(Error::validation("This till session is closed"));
        }
        Ok(session)
    }

    /// The session's movements, oldest first
    pub async fn movements(&self, id: Uuid) -> Result<Vec<TillMovement>> {
        let movements = sqlx::query_as(
            "SELECT m.id, m.kind, m.method, m.amount, m.order_id, o.order_number, m.reason, \
             TRIM(COALESCE(c.first_name, '') || ' ' || COALESCE(c.last_name, '')) AS staff_name, \
             m.created_at \
             FROM till_movements m LEFT JOIN orders o ON o.id = m.order_id \
             LEFT JOIN customers c ON c.id = m.staff_id \
             WHERE m.session_id = $1 ORDER BY m.created_at, m.id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(movements)
    }

    /// Put cash in the drawer or take some out (a reason is required to take
    /// cash out)
    pub async fn cash(
        &self,
        session: Uuid,
        staff: Uuid,
        movement: CashMovement,
    ) -> Result<ZReport> {
        let sign = match movement.kind.as_str() {
            "cash_in" => Decimal::ONE,
            "cash_out" => Decimal::NEGATIVE_ONE,
            _ => return Err(Error::validation("kind must be cash_in or cash_out")),
        };
        if movement.amount <= Decimal::ZERO {
            return Err(Error::validation("The amount must be positive"));
        }
        let reason = movement
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty());
        if sign.is_sign_negative() && reason.is_none() {
            return Err(Error::validation("Say why cash is taken out"));
        }
        let mut conn = self.pool.acquire().await?;
        record(
            &mut conn,
            Entry {
                session,
                kind: &movement.kind,
                method: "cash",
                amount: sign * movement.amount.round_dp(2),
                order: None,
                reason,
                staff,
            },
        )
        .await?;
        self.report(session).await
    }

    /// The session with its totals and movements
    pub async fn report(&self, id: Uuid) -> Result<ZReport> {
        let session = self.get(id).await?;
        let movements = self.movements(id).await?;
        Ok(ZReport {
            totals: totals(session.opening_float, &movements),
            session,
            movements,
        })
    }

    /// Close the session with the cash counted: the branch's next Z number,
    /// the cash expected and the difference
    pub async fn close(
        &self,
        id: Uuid,
        cashier: Uuid,
        counted: Decimal,
        notes: Option<String>,
    ) -> Result<ZReport> {
        if counted < Decimal::ZERO {
            return Err(Error::validation("The cash counted cannot be negative"));
        }
        let session = self.open_of(id, cashier).await?;
        let expected = totals(session.opening_float, &self.movements(id).await?).expected_cash;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('till_z_' || $1::text))")
            .bind(session.location_id)
            .execute(&mut *tx)
            .await?;
        let closed = sqlx::query(
            "UPDATE till_sessions SET closed_at = NOW(), counted_cash = $2, expected_cash = $3, \
             difference = $2 - $3, notes = $4, z_number = (SELECT COALESCE(MAX(z_number), 0) + 1 \
             FROM till_sessions WHERE location_id = $5) WHERE id = $1 AND closed_at IS NULL",
        )
        .bind(id)
        .bind(counted.round_dp(2))
        .bind(expected)
        .bind(
            notes
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty()),
        )
        .bind(session.location_id)
        .execute(&mut *tx)
        .await?;
        if closed.rows_affected() == 0 {
            return Err(Error::validation("This till session is closed"));
        }
        tx.commit().await?;
        self.report(id).await
    }
}

/// A movement to record
pub struct Entry<'a> {
    pub session: Uuid,
    pub kind: &'a str,
    pub method: &'a str,
    /// Positive in, negative out
    pub amount: Decimal,
    pub order: Option<Uuid>,
    pub reason: Option<&'a str>,
    pub staff: Uuid,
}

/// Record a movement (inside the caller's transaction for sales and refunds)
pub async fn record(conn: &mut PgConnection, entry: Entry<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO till_movements (session_id, kind, method, amount, order_id, reason, staff_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(entry.session)
    .bind(entry.kind)
    .bind(entry.method)
    .bind(entry.amount)
    .bind(entry.order)
    .bind(entry.reason)
    .bind(entry.staff)
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn movement(kind: &str, method: &str, cents: i64) -> TillMovement {
        TillMovement {
            id: Uuid::nil(),
            kind: kind.into(),
            method: method.into(),
            amount: Decimal::new(cents, 2),
            order_id: None,
            order_number: None,
            reason: None,
            staff_name: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn z_totals_add_up_per_method_and_cash_follows_the_drawer() {
        let movements = vec![
            movement("sale", "cash", 4_250),
            movement("sale", "card", 12_000),
            movement("sale", "cash", 1_000),
            movement("refund", "cash", -500),
            movement("refund", "card", -2_000),
            movement("cash_in", "cash", 5_000),
            movement("cash_out", "cash", -3_000),
        ];
        let t = totals(Decimal::new(10_000, 2), &movements);
        assert_eq!(
            t.sales,
            vec![
                MethodTotal {
                    method: "card".into(),
                    count: 1,
                    amount: Decimal::new(12_000, 2)
                },
                MethodTotal {
                    method: "cash".into(),
                    count: 2,
                    amount: Decimal::new(5_250, 2)
                },
            ]
        );
        assert_eq!(t.refunds[0].amount, Decimal::new(2_000, 2));
        assert_eq!(t.refunds[1].amount, Decimal::new(500, 2));
        assert_eq!(
            (t.cash_in, t.cash_out),
            (Decimal::new(5_000, 2), Decimal::new(3_000, 2))
        );
        assert_eq!(t.net, Decimal::new(14_750, 2));
        // 100 float + 42.50 + 10 − 5 + 50 − 30
        assert_eq!(t.expected_cash, Decimal::new(16_750, 2));
    }

    #[test]
    fn an_empty_session_expects_its_float() {
        let t = totals(Decimal::new(15_000, 2), &[]);
        assert_eq!(t.expected_cash, Decimal::new(15_000, 2));
        assert!(t.sales.is_empty() && t.net.is_zero());
    }
}
