//! Staff roles and their assignment to accounts
//!
//! A staff role lists `<resource>:read` and `<resource>:write` permissions
//! (see [`crate::services::api_key_scopes`]). An account holding roles gets
//! the union of their permissions on the admin API; global admins do not need
//! roles. Roles never grant `admin` or wildcard scopes, so they cannot reach
//! admin-only routes or resources added later.

use rust_decimal::Decimal;
use uuid::Uuid;
use validator::Validate;

use crate::{
    models::{CreateStaffRoleRequest, StaffMember, StaffRole, UpdateStaffRoleRequest},
    repository::Database,
    services::api_key_scopes::{Action, Scope},
    services::dashboard_widgets::{self, LayoutSlot, RoleWidgets},
    Error, Result,
};

/// What a staff member is shown, from their roles: menu entries, dashboard
/// widgets and the till discount cap. It narrows the display only; their
/// permissions decide what they may do.
#[derive(Debug, Clone)]
pub struct StaffView {
    /// `None`: every entry their permissions allow
    pub menu: Option<Vec<String>>,
    /// The widgets offered to them, as their default layout
    pub widgets: Vec<LayoutSlot>,
    /// The largest manual discount they may give, in percent
    pub max_discount_percent: Decimal,
}

/// Staff role management and permission lookup
pub struct StaffService {
    db: Database,
}

impl StaffService {
    /// Create a new staff service
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// All staff roles, by name
    pub async fn list_roles(&self) -> Result<Vec<StaffRole>> {
        let roles = sqlx::query_as::<_, StaffRole>("SELECT * FROM staff_roles ORDER BY name")
            .fetch_all(self.db.pool())
            .await?;
        Ok(roles)
    }

    /// Create a role; its name must be unique
    pub async fn create_role(&self, request: CreateStaffRoleRequest) -> Result<StaffRole> {
        request
            .validate()
            .map_err(|e| Error::validation(format!("Invalid staff role: {}", e)))?;
        let permissions = validate_permissions(&request.permissions)?;
        let menu = request
            .menu
            .as_deref()
            .map(dashboard_widgets::validate_menu)
            .transpose()?;
        let widgets = request
            .widgets
            .as_deref()
            .map(dashboard_widgets::validate_role_widgets)
            .transpose()?;
        let cap = checked_cap(request.max_discount_percent)?;

        sqlx::query_as::<_, StaffRole>(
            "INSERT INTO staff_roles (name, description, permissions, menu, widgets, \
             max_discount_percent) VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
        )
        .bind(&request.name)
        .bind(&request.description)
        .bind(&permissions)
        .bind(&menu)
        .bind(&widgets)
        .bind(cap)
        .fetch_one(self.db.pool())
        .await
        .map_err(duplicate_name)
    }

    /// Update a role; omitted fields keep their value
    pub async fn update_role(
        &self,
        id: Uuid,
        request: UpdateStaffRoleRequest,
    ) -> Result<StaffRole> {
        request
            .validate()
            .map_err(|e| Error::validation(format!("Invalid staff role: {}", e)))?;
        let permissions = match &request.permissions {
            Some(permissions) => Some(validate_permissions(permissions)?),
            None => None,
        };
        let (menu, widgets, cap) = checked_display(&request)?;

        sqlx::query_as::<_, StaffRole>(
            "UPDATE staff_roles SET name = COALESCE($2, name), \
             description = COALESCE($3, description), \
             permissions = COALESCE($4, permissions), \
             menu = CASE WHEN $5 THEN $6 ELSE menu END, \
             widgets = CASE WHEN $7 THEN $8 ELSE widgets END, \
             max_discount_percent = CASE WHEN $9 THEN $10 ELSE max_discount_percent END, \
             updated_at = NOW() WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(&request.name)
        .bind(&request.description)
        .bind(&permissions)
        .bind(menu.is_some())
        .bind(menu.flatten())
        .bind(widgets.is_some())
        .bind(widgets.flatten())
        .bind(cap.is_some())
        .bind(cap.flatten())
        .fetch_optional(self.db.pool())
        .await
        .map_err(duplicate_name)?
        .ok_or_else(|| Error::not_found("Staff role not found"))
    }

    /// Delete a role and its assignments
    pub async fn delete_role(&self, id: Uuid) -> Result<()> {
        let result = sqlx::query("DELETE FROM staff_roles WHERE id = $1")
            .bind(id)
            .execute(self.db.pool())
            .await?;
        if result.rows_affected() == 0 {
            return Err(Error::not_found("Staff role not found"));
        }
        Ok(())
    }

    /// Accounts holding at least one role, by e-mail
    pub async fn list_members(&self) -> Result<Vec<StaffMember>> {
        let members = sqlx::query_as::<_, StaffMember>(
            "SELECT c.id AS customer_id, c.email, \
             array_agg(a.role_id ORDER BY a.role_id) AS role_ids \
             FROM staff_role_assignments a JOIN customers c ON c.id = a.customer_id \
             GROUP BY c.id, c.email ORDER BY c.email",
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(members)
    }

    /// Replace an account's roles (an empty list removes its staff access)
    pub async fn set_member_roles(
        &self,
        customer_id: Uuid,
        role_ids: &[Uuid],
    ) -> Result<Vec<Uuid>> {
        let mut role_ids = role_ids.to_vec();
        role_ids.sort();
        role_ids.dedup();

        let mut tx = self.db.pool().begin().await?;
        let customer: Option<Uuid> = sqlx::query_scalar("SELECT id FROM customers WHERE id = $1")
            .bind(customer_id)
            .fetch_optional(&mut *tx)
            .await?;
        customer.ok_or_else(|| Error::not_found("Customer not found"))?;

        let known: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM staff_roles WHERE id = ANY($1)")
            .bind(&role_ids)
            .fetch_one(&mut *tx)
            .await?;
        if known != role_ids.len() as i64 {
            return Err(Error::not_found("Staff role not found"));
        }

        sqlx::query("DELETE FROM staff_role_assignments WHERE customer_id = $1")
            .bind(customer_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO staff_role_assignments (customer_id, role_id) \
             SELECT $1, unnest($2::uuid[])",
        )
        .bind(customer_id)
        .bind(&role_ids)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(role_ids)
    }

    /// The union of the permissions of an account's roles (empty without roles)
    pub async fn permissions_for(&self, customer_id: Uuid) -> Result<Vec<String>> {
        let permissions = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT p FROM staff_role_assignments a \
             JOIN staff_roles r ON r.id = a.role_id, unnest(r.permissions) AS p \
             WHERE a.customer_id = $1 ORDER BY p",
        )
        .bind(customer_id)
        .fetch_all(self.db.pool())
        .await?;
        Ok(permissions)
    }

    /// The roles an account holds, by name
    pub async fn roles_of(&self, customer_id: Uuid) -> Result<Vec<StaffRole>> {
        let roles = sqlx::query_as::<_, StaffRole>(
            "SELECT r.* FROM staff_roles r JOIN staff_role_assignments a ON a.role_id = r.id \
             WHERE a.customer_id = $1 ORDER BY r.name",
        )
        .bind(customer_id)
        .fetch_all(self.db.pool())
        .await?;
        Ok(roles)
    }

    /// What a staff member is shown (global admins: everything, any discount)
    pub async fn view(&self, customer_id: Uuid, is_admin: bool) -> Result<StaffView> {
        if is_admin {
            return Ok(StaffView {
                menu: None,
                widgets: dashboard_widgets::offered(true, &[]),
                max_discount_percent: Decimal::ONE_HUNDRED,
            });
        }
        let roles = self.roles_of(customer_id).await?;
        Ok(view_of(&roles))
    }

    /// The dashboard layout a staff member saved, if any
    pub async fn saved_layout(&self, customer_id: Uuid) -> Result<Option<Vec<LayoutSlot>>> {
        let layout: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT layout FROM staff_dashboard_layouts WHERE customer_id = $1")
                .bind(customer_id)
                .fetch_optional(self.db.pool())
                .await?;
        // A layout that no longer parses counts as none
        Ok(layout.and_then(|value| serde_json::from_value(value).ok()))
    }

    /// Save a staff member's layout, checked against the widgets offered to
    /// them; returns the layout as it will show
    pub async fn save_layout(
        &self,
        customer_id: Uuid,
        layout: &[LayoutSlot],
        offered: &[LayoutSlot],
    ) -> Result<Vec<LayoutSlot>> {
        let layout = dashboard_widgets::validate(layout, offered)?;
        sqlx::query(
            "INSERT INTO staff_dashboard_layouts (customer_id, layout) VALUES ($1, $2) \
             ON CONFLICT (customer_id) DO UPDATE SET layout = EXCLUDED.layout, \
             updated_at = NOW()",
        )
        .bind(customer_id)
        .bind(sqlx::types::Json(&layout))
        .execute(self.db.pool())
        .await?;
        Ok(dashboard_widgets::resolve(Some(&layout), offered))
    }

    /// Forget a staff member's layout: their roles' default applies again
    pub async fn reset_layout(&self, customer_id: Uuid) -> Result<()> {
        sqlx::query("DELETE FROM staff_dashboard_layouts WHERE customer_id = $1")
            .bind(customer_id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }
}

/// What the holder of these roles is shown: menus and widgets add up, the
/// highest cap counts (a role without one gives no manual discount)
fn view_of(roles: &[StaffRole]) -> StaffView {
    let widgets: Vec<RoleWidgets> = roles
        .iter()
        .map(|r| RoleWidgets {
            permissions: &r.permissions,
            widgets: r.widgets.as_deref(),
        })
        .collect();
    let menus: Vec<Option<&[String]>> = roles.iter().map(|r| r.menu.as_deref()).collect();
    StaffView {
        menu: dashboard_widgets::menu(false, &menus),
        widgets: dashboard_widgets::offered(false, &widgets),
        max_discount_percent: roles
            .iter()
            .filter_map(|r| r.max_discount_percent)
            .max()
            .unwrap_or_default(),
    }
}

/// A role update's menu, widgets and cap, checked: `None` keeps the value,
/// `Some(None)` clears it
type DisplayChange = (
    Option<Option<Vec<String>>>,
    Option<Option<Vec<String>>>,
    Option<Option<Decimal>>,
);

fn checked_display(request: &UpdateStaffRoleRequest) -> Result<DisplayChange> {
    let menu = match &request.menu {
        Some(Some(menu)) => Some(Some(dashboard_widgets::validate_menu(menu)?)),
        other => other.clone(),
    };
    let widgets = match &request.widgets {
        Some(Some(ids)) => Some(Some(dashboard_widgets::validate_role_widgets(ids)?)),
        other => other.clone(),
    };
    let cap = match request.max_discount_percent {
        Some(cap) => Some(checked_cap(cap)?),
        None => None,
    };
    Ok((menu, widgets, cap))
}

/// A discount cap must be a percentage
fn checked_cap(cap: Option<Decimal>) -> Result<Option<Decimal>> {
    match cap {
        Some(cap) if cap < Decimal::ZERO || cap > Decimal::ONE_HUNDRED => Err(Error::validation(
            "The discount cap is a percentage from 0 to 100",
        )),
        _ => Ok(cap),
    }
}

/// Check and normalise a role's permissions: each must be `<resource>:read` or
/// `<resource>:write`. Wildcards (`read`, `write`, `admin`) and `<resource>:admin`
/// are refused, so a role can never grant admin rights.
pub fn validate_permissions(permissions: &[String]) -> Result<Vec<String>> {
    let mut normalized: Vec<String> = Vec::new();
    for permission in permissions {
        let scope = Scope::parse(permission).map_err(|e| {
            Error::validation(format!("Invalid permission '{}': {}", permission, e))
        })?;
        if scope.resource.is_none() || scope.action == Action::Admin {
            return Err(Error::validation(format!(
                "Invalid permission '{}': use <resource>:read or <resource>:write",
                permission
            )));
        }
        let scope = scope.as_scope_string();
        if !normalized.contains(&scope) {
            normalized.push(scope);
        }
    }
    Ok(normalized)
}

/// A unique violation on the role name is a client error
fn duplicate_name(error: sqlx::Error) -> Error {
    match &error {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            Error::validation("A staff role with this name already exists")
        }
        _ => error.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn resource_read_and_write_are_accepted_once() {
        let permissions = strings(&["products:write", "reports:read", "products:write"]);
        assert_eq!(
            validate_permissions(&permissions).unwrap(),
            strings(&["products:write", "reports:read"])
        );
        assert!(validate_permissions(&[]).unwrap().is_empty());
    }

    fn role(permissions: &[&str], menu: Option<&[&str]>, cap: Option<i64>) -> StaffRole {
        StaffRole {
            id: Uuid::nil(),
            name: "r".into(),
            description: None,
            permissions: strings(permissions),
            menu: menu.map(strings),
            widgets: None,
            max_discount_percent: cap.map(Decimal::from),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn roles_add_up_menus_widgets_and_keep_the_highest_cap() {
        let view = view_of(&[
            role(&["orders:write"], Some(&["orders", "counter"]), Some(5)),
            role(&["inventory:read"], Some(&["stock"]), None),
        ]);
        assert_eq!(view.menu, Some(strings(&["orders", "counter", "stock"])));
        assert_eq!(view.max_discount_percent, Decimal::from(5));
        let offered: Vec<&str> = view.widgets.iter().map(|w| w.widget.as_str()).collect();
        assert_eq!(offered, ["orders", "invoices", "stock"]);
        let none = view_of(&[role(&["orders:read"], None, None)]);
        assert_eq!(none.menu, None);
        assert_eq!(none.max_discount_percent, Decimal::ZERO);
    }

    #[test]
    fn discount_caps_are_percentages() {
        assert!(checked_cap(Some(Decimal::from(101))).is_err());
        assert!(checked_cap(Some(Decimal::from(-1))).is_err());
        assert_eq!(checked_cap(None).unwrap(), None);
        assert_eq!(
            checked_cap(Some(Decimal::new(125, 1))).unwrap(),
            Some(Decimal::new(125, 1))
        );
    }

    #[test]
    fn a_role_update_keeps_or_clears_its_display() {
        let keep = UpdateStaffRoleRequest::default();
        assert_eq!(checked_display(&keep).unwrap(), (None, None, None));
        let request: UpdateStaffRoleRequest = serde_json::from_str(
            r#"{"menu":null,"widgets":["sales","sales"],"max_discount_percent":10}"#,
        )
        .unwrap();
        let (menu, widgets, cap) = checked_display(&request).unwrap();
        assert_eq!(menu, Some(None));
        assert_eq!(widgets, Some(Some(strings(&["sales"]))));
        assert_eq!(cap, Some(Some(Decimal::from(10))));
        let bad: UpdateStaffRoleRequest = serde_json::from_str(r#"{"widgets":["nope"]}"#).unwrap();
        assert!(checked_display(&bad).is_err());
    }

    #[test]
    fn admin_wildcard_and_unknown_permissions_are_refused() {
        for bad in [
            "admin",
            "read",
            "write",
            "products:admin",
            "nope:read",
            "products:delete",
            "",
        ] {
            let error = validate_permissions(&strings(&[bad])).unwrap_err();
            assert_eq!(error.status_code(), 400, "{bad}");
        }
    }
}
