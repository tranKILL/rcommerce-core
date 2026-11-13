//! Staff roles: named permission sets that open parts of the admin API to
//! accounts that are not global admins

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;
use validator::Validate;

/// A named set of `<resource>:read` / `<resource>:write` permissions
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct StaffRole {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub permissions: Vec<String>,
    /// Back-office menu entries shown to the role; `None`: every entry its
    /// permissions allow
    pub menu: Option<Vec<String>>,
    /// Dashboard widgets offered to the role, in their default order; `None`:
    /// every widget its permissions read
    pub widgets: Option<Vec<String>>,
    /// The largest manual discount the role may give at the till, in percent
    pub max_discount_percent: Option<Decimal>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Create a staff role
#[derive(Debug, Clone, Deserialize, Validate)]
pub struct CreateStaffRoleRequest {
    #[validate(length(min = 1, max = 100))]
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub menu: Option<Vec<String>>,
    #[serde(default)]
    pub widgets: Option<Vec<String>>,
    /// 0 to 100, as a number or text
    #[serde(default, deserialize_with = "percent")]
    pub max_discount_percent: Option<Decimal>,
}

/// Update a staff role; omitted fields keep their value
#[derive(Debug, Clone, Default, Deserialize, Validate)]
pub struct UpdateStaffRoleRequest {
    #[validate(length(min = 1, max = 100))]
    pub name: Option<String>,
    pub description: Option<String>,
    pub permissions: Option<Vec<String>>,
    /// `null` shows every entry the permissions allow again
    #[serde(default, deserialize_with = "super::product::clearable")]
    pub menu: Option<Option<Vec<String>>>,
    /// `null` offers every widget the permissions read again
    #[serde(default, deserialize_with = "super::product::clearable")]
    pub widgets: Option<Option<Vec<String>>>,
    /// `null` removes the cap (no manual discount)
    #[serde(default, deserialize_with = "clearable_percent")]
    pub max_discount_percent: Option<Option<Decimal>>,
}

/// An account holding at least one staff role
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct StaffMember {
    pub customer_id: Uuid,
    pub email: String,
    pub role_ids: Vec<Uuid>,
}

/// A percentage sent as a JSON number or as text; null: none
fn percent<'de, D>(deserializer: D) -> std::result::Result<Option<Decimal>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Percent(
        #[serde(deserialize_with = "crate::services::price_list_service::number_or_text")] Decimal,
    );
    Option::<Percent>::deserialize(deserializer).map(|p| p.map(|p| p.0))
}

/// A percentage that may be cleared: absent keeps it, null clears it
fn clearable_percent<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Option<Decimal>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    percent(deserializer).map(Some)
}
