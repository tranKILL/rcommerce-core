//! Which staff permission opens an admin route
//!
//! Global admins reach every admin route. Other accounts reach one only through
//! a staff role granting the route's resource: `read` for GET and HEAD, `write`
//! otherwise. A route outside [`ADMIN_AREAS`] stays admin-only (API keys, staff
//! management), so a new admin route is closed to staff until it is listed here.
//!
//! Customer routes that let admins reach other accounts' data (any order, any
//! customer) ask [`staff_can`] instead.

use axum::http::Method;
use rcommerce_core::{services::ScopeChecker, Action, Resource, StaffService};

use crate::{middleware::JwtAuth, state::AppState};

/// Admin route prefixes (inside `/api/v1`) and the resource that guards them
const ADMIN_AREAS: &[(&str, Resource)] = &[
    ("/admin/statistics", Resource::Reports),
    ("/admin/stats", Resource::Reports),
    ("/admin/analytics", Resource::Reports),
    ("/admin/orders", Resource::Orders),
    ("/admin/products", Resource::Products),
    ("/products", Resource::Products),
    ("/coupons", Resource::Coupons),
    ("/admin/subscriptions", Resource::Payments),
    ("/admin/dunning", Resource::Payments),
    ("/admin/inventory", Resource::Inventory),
    ("/admin/companies", Resource::Customers),
    ("/admin/customers", Resource::Customers),
    ("/admin/customer-groups", Resource::Customers),
    ("/admin/price-lists", Resource::Products),
    ("/admin/invoices", Resource::Orders),
    ("/admin/content", Resource::Settings),
    ("/admin/promotions", Resource::Coupons),
    ("/admin/loyalty/settings", Resource::Settings),
    ("/admin/loyalty", Resource::Customers),
];

/// POST routes that only read, so `read` is enough
const READ_ONLY_POSTS: &[&str] = &["/coupons/validate"];

/// Admin GET routes open to any staff member (one role is enough): the
/// back-office home, which shows each part only to those it concerns, and
/// the caller's own rights
const ANY_STAFF: &[&str] = &["/admin/dashboard", "/admin/staff/me"];

/// Whether any staff member may call this route, whatever their roles
pub fn open_to_any_staff(method: &Method, path: &str) -> bool {
    *method == Method::GET && ANY_STAFF.contains(&path)
}

/// The resource and action a staff role needs for an admin route, or `None`
/// when the route is reserved to global admins
pub fn staff_permission(method: &Method, path: &str) -> Option<(Resource, Action)> {
    let (_, resource) = ADMIN_AREAS
        .iter()
        .find(|(prefix, _)| is_under(path, prefix))?;
    let reads = *method == Method::GET
        || *method == Method::HEAD
        || (*method == Method::POST && READ_ONLY_POSTS.contains(&path));
    let action = if reads { Action::Read } else { Action::Write };
    Some((*resource, action))
}

/// Whether the caller may act on other accounts' data for `resource`: global
/// admins always, anyone else when one of its staff roles grants `action`.
/// Costs one query for non-admins, so ask only on the branch that needs it
/// (after an ownership check fails). A failed lookup is logged and denies.
pub async fn staff_can(
    state: &AppState,
    auth: &JwtAuth,
    resource: Resource,
    action: Action,
) -> bool {
    if auth.is_admin() {
        return true;
    }
    match StaffService::new(state.db.clone())
        .permissions_for(auth.customer_id)
        .await
    {
        Ok(granted) => ScopeChecker::new(&granted).is_ok_and(|c| c.can(resource, action)),
        Err(e) => {
            tracing::error!("Failed to load staff permissions: {}", e);
            false
        }
    }
}

/// Whether `path` is `prefix` or below it (`/products` covers `/products/1`,
/// not `/products-x`)
fn is_under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_follow_the_method() {
        assert_eq!(
            staff_permission(&Method::GET, "/admin/statistics/sales"),
            Some((Resource::Reports, Action::Read))
        );
        assert_eq!(
            staff_permission(
                &Method::PUT,
                "/products/6f1c7a52-0d1e-4f7a-9b8e-1f2a3b4c5d6e"
            ),
            Some((Resource::Products, Action::Write))
        );
        assert_eq!(
            staff_permission(&Method::DELETE, "/admin/products/1/reviews/2"),
            Some((Resource::Products, Action::Write))
        );
        assert_eq!(
            staff_permission(&Method::POST, "/admin/dunning/process"),
            Some((Resource::Payments, Action::Write))
        );
    }

    #[test]
    fn stock_management_needs_the_inventory_permission() {
        assert_eq!(
            staff_permission(&Method::GET, "/admin/inventory/levels"),
            Some((Resource::Inventory, Action::Read))
        );
        assert_eq!(
            staff_permission(&Method::POST, "/admin/inventory/receipts"),
            Some((Resource::Inventory, Action::Write))
        );
    }

    #[test]
    fn coupon_validation_only_reads() {
        assert_eq!(
            staff_permission(&Method::POST, "/coupons/validate"),
            Some((Resource::Coupons, Action::Read))
        );
        assert_eq!(
            staff_permission(&Method::POST, "/coupons"),
            Some((Resource::Coupons, Action::Write))
        );
    }

    #[test]
    fn the_dashboard_and_own_rights_are_open_to_any_staff() {
        assert!(open_to_any_staff(&Method::GET, "/admin/dashboard"));
        assert!(open_to_any_staff(&Method::GET, "/admin/staff/me"));
        assert!(!open_to_any_staff(&Method::POST, "/admin/staff/me"));
        assert!(!open_to_any_staff(&Method::GET, "/admin/staff/roles"));
    }

    #[test]
    fn unlisted_routes_stay_admin_only() {
        for path in ["/admin/api-keys", "/admin/staff/roles", "/admin/new-area"] {
            assert_eq!(staff_permission(&Method::GET, path), None, "{path}");
        }
        assert_eq!(staff_permission(&Method::GET, "/admin/statsx"), None);
        assert_eq!(staff_permission(&Method::GET, "/products-x"), None);
    }
}
