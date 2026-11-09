//! Stock per branch for the storefront ("in stock at <branch>", "low stock")
//!
//! A product's stock at a public location is its saleable quantity there: the
//! free stock of the product and its variants (`available_quantity` of its
//! `inventory_levels` rows, which already excludes stock committed to orders)
//! minus the active, unexpired cart holds (`stock_reservations`). It is
//! reported as a status, not a quantity:
//! - `out` at 0 or below (or no stock row at that location),
//! - `low` at or below the location's reorder point for the product,
//! - `in_stock` above it.
//!
//! Products that do not track inventory (`inventory_management = false`) are
//! always `in_stock`. Every query covers a whole product page or all branches
//! of one product in one round trip.

use serde::Serialize;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::Result;

/// How much of a product a branch can sell
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StockStatus {
    InStock,
    Low,
    Out,
}

/// The status for a saleable quantity and the quantity at which stock is low
pub fn stock_status(saleable: i64, low_at: i64) -> StockStatus {
    if saleable <= 0 {
        StockStatus::Out
    } else if saleable <= low_at {
        StockStatus::Low
    } else {
        StockStatus::InStock
    }
}

/// One product's stock at one public location
#[derive(Debug, Clone, Serialize)]
pub struct BranchStock {
    pub location_id: Uuid,
    pub slug: Option<String>,
    pub name: String,
    pub status: StockStatus,
}

/// One product's stock at a given location
#[derive(Debug, Clone, Serialize)]
pub struct ProductStock {
    pub product_id: Uuid,
    pub status: StockStatus,
}

/// Saleable quantity, low-stock point and tracking flag of one aggregate row
#[derive(FromRow)]
struct StockRow {
    id: Uuid,
    slug: Option<String>,
    name: String,
    saleable: i64,
    low_at: i64,
    tracked: bool,
}

impl StockRow {
    fn status(&self) -> StockStatus {
        if self.tracked {
            stock_status(self.saleable, self.low_at)
        } else {
            StockStatus::InStock
        }
    }
}

/// A product's stock at every public, active location (in the locations'
/// display order); `None` when the product does not exist
pub async fn product_stock_by_branch(
    pool: &PgPool,
    product_id: Uuid,
) -> Result<Option<Vec<BranchStock>>> {
    let tracked: Option<bool> =
        sqlx::query_scalar("SELECT inventory_management FROM products WHERE id = $1")
            .bind(product_id)
            .fetch_optional(pool)
            .await?;
    let Some(tracked) = tracked else {
        return Ok(None);
    };
    let rows = sqlx::query_as::<_, StockRow>(
        "SELECT l.id, l.slug, l.name, \
                (COALESCE(SUM(lv.available_quantity), 0) - \
                 COALESCE((SELECT SUM(r.quantity) FROM stock_reservations r \
                           WHERE r.product_id = $1 AND r.location_id = l.id \
                             AND r.status::text = 'active' AND r.expires_at > NOW()), 0))::BIGINT AS saleable, \
                COALESCE(MAX(lv.reorder_point), 0)::BIGINT AS low_at, $2 AS tracked \
         FROM inventory_locations l \
         LEFT JOIN inventory_levels lv ON lv.location_id = l.id AND lv.product_id = $1 \
         WHERE l.is_public AND l.is_active \
         GROUP BY l.id ORDER BY l.sort_order, l.name",
    )
    .bind(product_id)
    .bind(tracked)
    .fetch_all(pool)
    .await?;
    Ok(Some(
        rows.iter()
            .map(|row| BranchStock {
                location_id: row.id,
                slug: row.slug.clone(),
                name: row.name.clone(),
                status: row.status(),
            })
            .collect(),
    ))
}

/// The stock of several products at one public, active location (by id or
/// slug), in the order asked; unknown product ids are left out. `None` when
/// no such location exists.
pub async fn location_stock_for_products(
    pool: &PgPool,
    location: &str,
    product_ids: &[Uuid],
) -> Result<Option<Vec<ProductStock>>> {
    let location_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM inventory_locations \
         WHERE is_public AND is_active AND (id::text = $1 OR slug = $1)",
    )
    .bind(location)
    .fetch_optional(pool)
    .await?;
    let Some(location_id) = location_id else {
        return Ok(None);
    };
    let rows = sqlx::query_as::<_, StockRow>(
        "SELECT p.id, NULL::TEXT AS slug, p.title AS name, \
                (COALESCE(SUM(lv.available_quantity), 0) - \
                 COALESCE((SELECT SUM(r.quantity) FROM stock_reservations r \
                           WHERE r.product_id = p.id AND r.location_id = $1 \
                             AND r.status::text = 'active' AND r.expires_at > NOW()), 0))::BIGINT AS saleable, \
                COALESCE(MAX(lv.reorder_point), 0)::BIGINT AS low_at, \
                p.inventory_management AS tracked \
         FROM products p \
         LEFT JOIN inventory_levels lv ON lv.product_id = p.id AND lv.location_id = $1 \
         WHERE p.id = ANY($2) \
         GROUP BY p.id, p.title, p.inventory_management, array_position($2, p.id) \
         ORDER BY array_position($2, p.id)",
    )
    .bind(location_id)
    .bind(product_ids)
    .fetch_all(pool)
    .await?;
    Ok(Some(
        rows.iter()
            .map(|row| ProductStock {
                product_id: row.id,
                status: row.status(),
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{stock_status, StockStatus};

    #[test]
    fn status_follows_the_saleable_quantity_and_the_low_point() {
        assert_eq!(stock_status(0, 10), StockStatus::Out);
        assert_eq!(stock_status(-3, 10), StockStatus::Out);
        assert_eq!(stock_status(1, 10), StockStatus::Low);
        assert_eq!(stock_status(10, 10), StockStatus::Low);
        assert_eq!(stock_status(11, 10), StockStatus::InStock);
        assert_eq!(stock_status(1, 0), StockStatus::InStock);
    }
}
