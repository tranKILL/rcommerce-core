//! Returns of goods already handed over, taken back at the till.
//!
//! A return names order lines and quantities: never more than was sold, less
//! what came back before. Its value is what those goods cost the customer:
//! their price excl. VAT, less their share of the order's discount (coupon,
//! points, cart promotions), with the VAT of each rate computed as on the
//! invoice. It is paid back by the original methods in turn: the online
//! payment first (through its gateway), then the card (a refund on the
//! terminal, for exactly the card's part), then cash. What the customer had
//! not paid is only credited. The goods go back in stock at the till's branch
//! unless they are damaged; an invoiced order gets a credit note.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

use crate::inventory::stock_admin::{apply_change, MovementNote, StockKey};
use crate::inventory::tracking::StockMovementType;
use crate::invoicing::service::discount_lines;
use crate::invoicing::{rate_of, vat_breakdown, InvoiceLine};
use crate::{Error, Result};

/// A line to take back
#[derive(Debug, Clone, Deserialize)]
pub struct ReturnLineInput {
    pub order_item_id: Uuid,
    pub quantity: i32,
    /// Back on the shelf (default); `false` for damaged goods
    #[serde(default = "back_on_the_shelf")]
    pub restock: bool,
}

fn back_on_the_shelf() -> bool {
    true
}

#[derive(Debug, sqlx::FromRow)]
struct ItemRow {
    id: Uuid,
    product_id: Option<Uuid>,
    variant_id: Option<Uuid>,
    title: String,
    sku: Option<String>,
    quantity: i32,
    returned: i32,
    price: Decimal,
    tax_amount: Decimal,
}

/// An order line as a return sees it
#[derive(Debug, Clone, Serialize)]
pub struct ReturnableLine {
    pub order_item_id: Uuid,
    pub product_id: Option<Uuid>,
    pub variant_id: Option<Uuid>,
    pub title: String,
    pub sku: Option<String>,
    /// Units sold
    pub quantity: i32,
    /// Units already brought back
    pub returned: i32,
    /// Excl. VAT, per unit
    pub unit_price: Decimal,
    /// VAT percentage
    pub vat_rate: Decimal,
}

impl ReturnableLine {
    /// Units that can still come back
    pub fn left(&self) -> i32 {
        (self.quantity - self.returned).max(0)
    }

    /// `quantity` units excl. VAT, before any discount
    fn net(&self, quantity: i32) -> Decimal {
        (self.unit_price * Decimal::from(quantity)).round_dp(2)
    }
}

impl From<ItemRow> for ReturnableLine {
    fn from(row: ItemRow) -> Self {
        let net = (row.price * Decimal::from(row.quantity)).round_dp(2);
        Self {
            order_item_id: row.id,
            product_id: row.product_id,
            variant_id: row.variant_id,
            title: row.title,
            sku: row.sku,
            quantity: row.quantity,
            returned: row.returned,
            unit_price: row.price,
            vat_rate: rate_of(net, row.tax_amount),
        }
    }
}

/// Money by payment method
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct ByMethod {
    /// Paid online (card or wallet through the web shop's gateway)
    pub online: Decimal,
    /// Paid by card on the till's terminal
    pub card: Decimal,
    pub cash: Decimal,
}

/// What a return of an order can take back
#[derive(Debug, Clone, Serialize)]
pub struct Returnable {
    pub order_id: Uuid,
    pub order_number: String,
    pub status: String,
    pub customer_id: Option<Uuid>,
    pub customer_name: Option<String>,
    pub currency: String,
    /// The order's invoice, when it has one: a return gets a credit note
    pub invoice_number: Option<String>,
    pub lines: Vec<ReturnableLine>,
    /// Still refundable per method: paid, less refunded
    pub refundable: ByMethod,
    /// The order's discount (coupon, points, cart promotions)
    pub discount_total: Decimal,
    /// The part of it already credited by earlier returns
    pub discount_credited: Decimal,
    /// Delivery charged (never refunded by a return)
    pub shipping_total: Decimal,
}

impl Returnable {
    /// Every line sold, excl. VAT, before the order's discount
    fn goods(&self) -> Decimal {
        self.lines.iter().map(|l| l.net(l.quantity)).sum()
    }

    /// The part of the order's discount that went to the goods (the rest
    /// went to the delivery, in proportion, as on the invoice)
    fn goods_discount(&self) -> Decimal {
        let goods = self.goods();
        let base = goods + self.shipping_total;
        if self.discount_total <= Decimal::ZERO || base <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        (self.discount_total * goods / base)
            .round_dp(2)
            .min(self.discount_total)
    }
}

#[derive(Debug, sqlx::FromRow)]
struct OrderRow {
    order_number: String,
    status: String,
    customer_id: Option<Uuid>,
    customer_name: Option<String>,
    currency: String,
    discount_total: Decimal,
    shipping_total: Decimal,
}

/// What `order` can still give back (`not found` when it does not exist)
pub async fn returnable(conn: &mut PgConnection, order: Uuid) -> Result<Returnable> {
    let row: OrderRow = sqlx::query_as(
        "SELECT o.order_number, o.status::text AS status, o.customer_id, \
         NULLIF(TRIM(COALESCE(c.first_name, '') || ' ' || COALESCE(c.last_name, '')), '') AS customer_name, \
         o.currency::text AS currency, o.discount_total, o.shipping_total \
         FROM orders o LEFT JOIN customers c ON c.id = o.customer_id WHERE o.id = $1",
    )
    .bind(order)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| Error::not_found("Order not found"))?;
    let items: Vec<ItemRow> = sqlx::query_as(
        "SELECT oi.id, oi.product_id, oi.variant_id, oi.title, oi.sku, oi.quantity, oi.price, oi.tax_amount, \
         COALESCE((SELECT SUM(rl.quantity) FROM order_return_lines rl WHERE rl.order_item_id = oi.id), 0)::int AS returned \
         FROM order_items oi WHERE oi.order_id = $1 AND COALESCE(oi.is_bundle_component, false) = false \
         ORDER BY oi.created_at, oi.id",
    )
    .bind(order)
    .fetch_all(&mut *conn)
    .await?;
    let (invoice_number, discount_credited): (Option<String>, Decimal) = sqlx::query_as(
        "SELECT (SELECT number FROM invoices WHERE order_id = $1 AND kind = 'invoice'), \
         COALESCE((SELECT SUM(discount) FROM order_returns WHERE order_id = $1), 0)",
    )
    .bind(order)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Returnable {
        order_id: order,
        order_number: row.order_number,
        status: row.status,
        customer_id: row.customer_id,
        customer_name: row.customer_name,
        currency: row.currency,
        invoice_number,
        lines: items.into_iter().map(ReturnableLine::from).collect(),
        refundable: refundable(conn, order).await?,
        discount_total: row.discount_total,
        discount_credited,
        shipping_total: row.shipping_total,
    })
}

/// What each method paid for `order`, less what was refunded through it
async fn refundable(conn: &mut PgConnection, order: Uuid) -> Result<ByMethod> {
    let rows: Vec<(String, Decimal)> = sqlx::query_as(
        "SELECT CASE WHEN p.gateway = 'till' AND p.payment_method::text = 'cash' THEN 'cash' \
                     WHEN p.gateway = 'till' THEN 'card' ELSE 'online' END AS method, \
                SUM(p.amount - COALESCE((SELECT SUM(r.amount) FROM refunds r \
                    WHERE r.payment_id = p.id AND r.status = 'refunded'), 0)) AS left \
         FROM payments p WHERE p.order_id = $1 AND p.status IN ('paid', 'refunded') GROUP BY 1",
    )
    .bind(order)
    .fetch_all(conn)
    .await?;
    let mut by = ByMethod::default();
    for (method, left) in rows {
        let left = left.max(Decimal::ZERO);
        match method.as_str() {
            "cash" => by.cash += left,
            "card" => by.card += left,
            _ => by.online += left,
        }
    }
    Ok(by)
}

/// A return priced: what is credited, excl. VAT (the discount share as
/// negative lines, one per VAT rate), VAT per rate and totals
#[derive(Debug, Clone, Serialize)]
pub struct PricedReturn {
    pub lines: Vec<InvoiceLine>,
    /// The returned goods excl. VAT, before their share of the discount
    pub goods: Decimal,
    /// Their share of the order's discount
    pub discount: Decimal,
    pub subtotal: Decimal,
    pub tax_total: Decimal,
    /// To pay back (or credit), VAT included
    pub total: Decimal,
    /// Everything sold has now come back
    pub last: bool,
    /// The returned goods' part of all the goods sold (for loyalty points)
    #[serde(skip)]
    pub ratio: Decimal,
}

/// Check the lines asked for against what can come back
fn check_lines(order: &Returnable, picked: &[ReturnLineInput]) -> Result<()> {
    if picked.is_empty() {
        return Err(Error::validation("A return needs at least one line"));
    }
    for (i, wanted) in picked.iter().enumerate() {
        if picked[..i]
            .iter()
            .any(|p| p.order_item_id == wanted.order_item_id)
        {
            return Err(Error::validation("A line is named twice"));
        }
        let line = order
            .lines
            .iter()
            .find(|l| l.order_item_id == wanted.order_item_id)
            .ok_or_else(|| Error::validation("A line is not part of this order"))?;
        if wanted.quantity < 1 || wanted.quantity > line.left() {
            return Err(Error::validation(format!(
                "{}: {} unit(s) can come back",
                line.title,
                line.left()
            )));
        }
    }
    Ok(())
}

/// Price a return of `picked` (checked against what can come back);
/// `discount_label` names the discount lines
pub fn price_return(
    order: &Returnable,
    picked: &[ReturnLineInput],
    discount_label: &str,
) -> Result<PricedReturn> {
    check_lines(order, picked)?;
    let mut lines = Vec::with_capacity(picked.len() + 2);
    for wanted in picked {
        let line = order
            .lines
            .iter()
            .find(|l| l.order_item_id == wanted.order_item_id)
            .ok_or_else(|| Error::validation("A line is not part of this order"))?;
        lines.push(InvoiceLine {
            description: line.title.clone(),
            sku: line.sku.clone(),
            quantity: wanted.quantity,
            unit_price: line.unit_price,
            vat_rate: line.vat_rate,
            net: line.net(wanted.quantity),
        });
    }
    let goods: Decimal = lines.iter().map(|l| l.net).sum();
    let all_goods = order.goods();
    let last = order.lines.iter().all(|line| {
        let now = picked
            .iter()
            .find(|p| p.order_item_id == line.order_item_id)
            .map_or(0, |p| p.quantity);
        line.returned + now >= line.quantity
    });
    let ratio = if all_goods > Decimal::ZERO {
        goods / all_goods
    } else {
        Decimal::ZERO
    };
    let discount = discount_share(order, ratio, last).min(goods);
    lines.extend(discount_lines(&lines, discount, discount_label));
    let vat = vat_breakdown(&lines);
    let subtotal: Decimal = vat.iter().map(|v| v.taxable).sum();
    let tax_total: Decimal = vat.iter().map(|v| v.tax).sum();
    Ok(PricedReturn {
        lines,
        goods,
        discount,
        subtotal,
        tax_total,
        total: subtotal + tax_total,
        last,
        ratio,
    })
}

/// The returned goods' share of the discount: in proportion to their value,
/// and all that is left of it when everything has come back
fn discount_share(order: &Returnable, ratio: Decimal, last: bool) -> Decimal {
    let goods_discount = order.goods_discount();
    let left = (goods_discount - order.discount_credited).max(Decimal::ZERO);
    if last {
        left
    } else {
        (goods_discount * ratio).round_dp(2).min(left)
    }
}

/// How a return is paid back
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct RefundPlan {
    pub online: Decimal,
    pub card: Decimal,
    pub cash: Decimal,
    /// Credited only: the customer had not paid it
    pub not_refunded: Decimal,
}

impl RefundPlan {
    /// Money given back, every method
    pub fn refunded(&self) -> Decimal {
        self.online + self.card + self.cash
    }
}

/// Pay `total` back by the original methods in turn: online, then card, then
/// cash. `card` is what the terminal refunded, which must be the card's part
/// exactly (`None`: a preview, the card's part is only suggested).
pub fn plan_refund(
    total: Decimal,
    refundable: &ByMethod,
    card: Option<Decimal>,
) -> Result<RefundPlan> {
    let online = total.min(refundable.online).max(Decimal::ZERO);
    let mut rest = total - online;
    let card_part = rest.min(refundable.card).max(Decimal::ZERO);
    if let Some(given) = card {
        if given != card_part {
            return Err(Error::validation(format!(
                "The card's part is {card_part}: refund exactly that on the terminal (refunded: {given})"
            )));
        }
    }
    rest -= card_part;
    let cash = rest.min(refundable.cash).max(Decimal::ZERO);
    Ok(RefundPlan {
        online,
        card: card_part,
        cash,
        not_refunded: rest - cash,
    })
}

/// The approved card refunds of a session not used yet, with their amounts
pub async fn card_refunds(
    conn: &mut PgConnection,
    session: Uuid,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, Decimal)>> {
    let rows: Vec<(Uuid, Decimal)> = sqlx::query_as(
        "SELECT id, amount FROM terminal_transactions WHERE id = ANY($1) AND session_id = $2 \
         AND kind = 'refund' AND status = 'approved' AND order_id IS NULL AND return_id IS NULL",
    )
    .bind(ids)
    .bind(session)
    .fetch_all(conn)
    .await?;
    let mut unique = ids.to_vec();
    unique.sort();
    unique.dedup();
    if rows.len() != unique.len() || unique.len() != ids.len() {
        return Err(Error::validation(
            "A card refund is not approved, already used, or from another session",
        ));
    }
    Ok(rows)
}

/// A return to record
pub struct ReturnRecord<'a> {
    pub order: &'a Returnable,
    pub session: Uuid,
    pub location: Uuid,
    pub staff: Uuid,
    pub reason: &'a str,
    pub lines: &'a [ReturnLineInput],
    pub priced: &'a PricedReturn,
    pub plan: &'a RefundPlan,
}

/// Record the return and its lines; its id
pub async fn record_return(conn: &mut PgConnection, record: &ReturnRecord<'_>) -> Result<Uuid> {
    let priced = record.priced;
    let plan = record.plan;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO order_returns (order_id, session_id, location_id, staff_id, reason, subtotal, \
         tax_total, total, discount, refund_online, refund_card, refund_cash, not_refunded) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING id",
    )
    .bind(record.order.order_id)
    .bind(record.session)
    .bind(record.location)
    .bind(record.staff)
    .bind(record.reason)
    .bind(priced.subtotal)
    .bind(priced.tax_total)
    .bind(priced.total)
    .bind(priced.discount)
    .bind(plan.online)
    .bind(plan.card)
    .bind(plan.cash)
    .bind(plan.not_refunded)
    .fetch_one(&mut *conn)
    .await?;
    for wanted in record.lines {
        let net = record
            .order
            .lines
            .iter()
            .find(|l| l.order_item_id == wanted.order_item_id)
            .map_or(Decimal::ZERO, |l| l.net(wanted.quantity));
        sqlx::query(
            "INSERT INTO order_return_lines (return_id, order_item_id, quantity, restocked, net) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(wanted.order_item_id)
        .bind(wanted.quantity)
        .bind(wanted.restock)
        .bind(net)
        .execute(&mut *conn)
        .await?;
    }
    Ok(id)
}

/// Put the returned goods back in stock at the till's branch (a bundle: its
/// parts), except those marked damaged
pub async fn restock(tx: &mut Transaction<'_, Postgres>, record: &ReturnRecord<'_>) -> Result<()> {
    let note = MovementNote {
        reason: Some("return".into()),
        reference: Some(record.order.order_number.clone()),
        notes: Some(record.reason.to_string()),
        cost_per_unit: None,
        actor: Some(record.staff),
    };
    for wanted in record.lines.iter().filter(|l| l.restock) {
        let Some(line) = record
            .order
            .lines
            .iter()
            .find(|l| l.order_item_id == wanted.order_item_id)
        else {
            continue;
        };
        let parts: Vec<(Option<Uuid>, Option<Uuid>, i32)> = sqlx::query_as(
            "SELECT product_id, variant_id, quantity FROM order_items WHERE bundle_parent_id = $1",
        )
        .bind(line.order_item_id)
        .fetch_all(&mut **tx)
        .await?;
        let goods = if parts.is_empty() {
            vec![(line.product_id, line.variant_id, wanted.quantity)]
        } else {
            parts
                .into_iter()
                .map(|(p, v, q)| (p, v, q * wanted.quantity / line.quantity.max(1)))
                .collect()
        };
        for (product, variant, quantity) in goods {
            // A product deleted since the sale has nowhere to go back to
            let Some(product_id) = product.filter(|_| quantity > 0) else {
                continue;
            };
            let key = StockKey {
                product_id,
                variant_id: variant,
                location_id: record.location,
            };
            apply_change(tx, key, quantity, StockMovementType::Return, &note).await?;
        }
    }
    Ok(())
}

/// Record the till's refunds against the sale's payment rows (cash, card),
/// marking a row refunded once all of it went back
pub async fn refund_till_payments(
    conn: &mut PgConnection,
    order: Uuid,
    return_id: Uuid,
    plan: &RefundPlan,
    reason: &str,
) -> Result<()> {
    for (method, amount) in [("cash", plan.cash), ("debit_card", plan.card)] {
        if amount <= Decimal::ZERO {
            continue;
        }
        let payment: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM payments WHERE order_id = $1 AND gateway = 'till' \
             AND payment_method::text = $2 AND status = 'paid' ORDER BY created_at LIMIT 1",
        )
        .bind(order)
        .bind(method)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(payment) = payment else {
            return Err(Error::validation(format!("No {method} payment to refund")));
        };
        sqlx::query(
            "INSERT INTO refunds (payment_id, order_id, amount, currency, reason, status, gateway_refund_id, settled_at) \
             SELECT id, order_id, $2, currency, $3, 'refunded', $4, NOW() FROM payments WHERE id = $1",
        )
        .bind(payment)
        .bind(amount)
        .bind(reason)
        .bind(format!("till-return-{}-{method}", return_id.simple()))
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "UPDATE payments p SET status = 'refunded', updated_at = NOW() WHERE p.id = $1 \
             AND (SELECT COALESCE(SUM(amount), 0) FROM refunds WHERE payment_id = p.id AND status = 'refunded') >= p.amount",
        )
        .bind(payment)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Once everything sold has come back the order is refunded; its payment
/// too when all that was paid went back
pub async fn close_when_all_back(
    conn: &mut PgConnection,
    order: &Returnable,
    priced: &PricedReturn,
    plan: &RefundPlan,
) -> Result<()> {
    if !priced.last {
        return Ok(());
    }
    let left =
        order.refundable.online + order.refundable.card + order.refundable.cash - plan.refunded();
    sqlx::query(
        "UPDATE orders SET status = 'refunded', \
         payment_status = CASE WHEN $2 THEN 'refunded'::payment_status ELSE payment_status END, \
         updated_at = NOW() WHERE id = $1",
    )
    .bind(order.order_id)
    .bind(left <= Decimal::ZERO)
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn line(id: u128, quantity: i32, price: Decimal, rate: Decimal) -> ReturnableLine {
        ReturnableLine {
            order_item_id: Uuid::from_u128(id),
            product_id: None,
            variant_id: None,
            title: format!("item {id}"),
            sku: None,
            quantity,
            returned: 0,
            unit_price: price,
            vat_rate: rate,
        }
    }

    fn order(lines: Vec<ReturnableLine>, discount: Decimal) -> Returnable {
        Returnable {
            order_id: Uuid::nil(),
            order_number: "ORD-1".into(),
            status: "completed".into(),
            customer_id: None,
            customer_name: None,
            currency: "EUR".into(),
            invoice_number: None,
            lines,
            refundable: ByMethod::default(),
            discount_total: discount,
            discount_credited: Decimal::ZERO,
            shipping_total: Decimal::ZERO,
        }
    }

    fn back(id: u128, quantity: i32) -> ReturnLineInput {
        ReturnLineInput {
            order_item_id: Uuid::from_u128(id),
            quantity,
            restock: true,
        }
    }

    #[test]
    fn a_partial_return_carries_its_share_of_the_discount() {
        // 10 × 6.00 at 21 % and 4 × 10.00 at 6 %: 100.00 of goods, 10.00 off
        let order = order(
            vec![
                line(1, 10, dec!(6.00), dec!(21)),
                line(2, 4, dec!(10.00), dec!(6)),
            ],
            dec!(10.00),
        );
        let priced = price_return(&order, &[back(1, 5)], "Discount").unwrap();
        assert_eq!(priced.goods, dec!(30.00));
        assert_eq!(priced.discount, dec!(3.00));
        assert_eq!(priced.subtotal, dec!(27.00));
        assert_eq!(priced.tax_total, dec!(5.67));
        assert_eq!(priced.total, dec!(32.67));
        assert!(!priced.last);
        assert_eq!(priced.lines.len(), 2, "the goods and one discount line");
    }

    #[test]
    fn the_last_return_takes_what_is_left_of_the_discount() {
        let mut lines = vec![line(1, 3, dec!(3.33), dec!(21))];
        lines[0].returned = 2;
        let mut order = order(lines, dec!(1.00));
        // 2 of 3 units already back, with 0.67 of the discount
        order.discount_credited = dec!(0.67);
        let priced = price_return(&order, &[back(1, 1)], "Discount").unwrap();
        assert!(priced.last);
        assert_eq!(priced.discount, dec!(0.33));
    }

    #[test]
    fn lines_are_checked_against_what_can_come_back() {
        let mut lines = vec![line(1, 3, dec!(1), dec!(21))];
        lines[0].returned = 2;
        let order = order(lines, Decimal::ZERO);
        assert!(
            price_return(&order, &[back(1, 2)], "-").is_err(),
            "only 1 left"
        );
        assert!(price_return(&order, &[back(1, 0)], "-").is_err());
        assert!(
            price_return(&order, &[back(9, 1)], "-").is_err(),
            "not in the order"
        );
        assert!(price_return(&order, &[back(1, 1), back(1, 1)], "-").is_err());
        assert!(price_return(&order, &[], "-").is_err());
    }

    #[test]
    fn refunds_follow_the_original_methods_in_turn() {
        let paid = ByMethod {
            online: Decimal::ZERO,
            card: dec!(20.00),
            cash: dec!(50.00),
        };
        let suggested = plan_refund(dec!(30.00), &paid, None).unwrap();
        assert_eq!((suggested.card, suggested.cash), (dec!(20.00), dec!(10.00)));
        assert!(plan_refund(dec!(30.00), &paid, Some(dec!(10.00))).is_err());
        assert!(plan_refund(dec!(30.00), &paid, Some(Decimal::ZERO)).is_err());
        let done = plan_refund(dec!(30.00), &paid, Some(dec!(20.00))).unwrap();
        assert_eq!(done.refunded(), dec!(30.00));
        // Online first; what was never paid is only credited
        let online = ByMethod {
            online: dec!(12.00),
            ..Default::default()
        };
        let plan = plan_refund(dec!(15.00), &online, Some(Decimal::ZERO)).unwrap();
        assert_eq!((plan.online, plan.not_refunded), (dec!(12.00), dec!(3.00)));
    }
}
