//! Back-office dashboard widgets, what each staff member is offered, and
//! their personal layouts
//!
//! Each widget reads one resource and comes in some sizes. A staff role may
//! narrow the widgets it offers (its list also sets their default order); a
//! staff member then arranges their own dashboard: order, size, hidden or
//! shown. None of this grants access: a widget whose resource the role cannot
//! read is never offered, and the dashboard computes only offered widgets.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{
    services::api_key_scopes::{Action, Resource, ScopeChecker},
    Error, Result,
};

/// How much room a widget takes on the dashboard (a quarter, half or the
/// whole width on wide screens)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WidgetSize {
    Small,
    Medium,
    Large,
}

use WidgetSize::{Large, Medium, Small};

/// A dashboard widget
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Widget {
    pub id: &'static str,
    /// The resource a role must read to be offered it
    pub resource: Resource,
    /// Sizes it can take
    pub sizes: &'static [WidgetSize],
    /// Its size until the staff member picks another
    pub size: WidgetSize,
    /// Shown by default when a role offers every widget
    pub shown: bool,
}

const fn widget(
    id: &'static str,
    resource: Resource,
    sizes: &'static [WidgetSize],
    size: WidgetSize,
    shown: bool,
) -> Widget {
    Widget {
        id,
        resource,
        sizes,
        size,
        shown,
    }
}

/// Every widget, in the default order. The analytics ones are hidden by
/// default: they cost more to compute and a staff member shows those they use.
pub const WIDGETS: &[Widget] = &[
    widget(
        "orders",
        Resource::Orders,
        &[Small, Medium, Large],
        Large,
        true,
    ),
    widget(
        "stock",
        Resource::Inventory,
        &[Small, Medium, Large],
        Medium,
        true,
    ),
    widget(
        "invoices",
        Resource::Orders,
        &[Small, Medium, Large],
        Medium,
        true,
    ),
    widget("sales", Resource::Reports, &[Medium, Large], Large, true),
    widget(
        "customers",
        Resource::Customers,
        &[Small, Medium, Large],
        Medium,
        true,
    ),
    widget(
        "catalogue",
        Resource::Products,
        &[Small, Medium, Large],
        Medium,
        true,
    ),
    widget(
        "finance",
        Resource::Reports,
        &[Small, Medium, Large],
        Medium,
        false,
    ),
    widget(
        "projection",
        Resource::Reports,
        &[Medium, Large],
        Large,
        false,
    ),
    widget(
        "top_customers",
        Resource::Reports,
        &[Medium, Large],
        Medium,
        false,
    ),
    widget("seasonality", Resource::Reports, &[Large], Large, false),
    widget("restock", Resource::Reports, &[Medium, Large], Large, false),
    widget(
        "promotion_results",
        Resource::Reports,
        &[Medium, Large],
        Large,
        false,
    ),
    widget("loyalty", Resource::Reports, &[Small, Medium], Small, false),
];

/// The widget with this id
pub fn find(id: &str) -> Option<&'static Widget> {
    WIDGETS.iter().find(|w| w.id == id)
}

/// A widget as the API describes it
#[derive(Debug, Clone, Serialize)]
pub struct WidgetInfo {
    pub id: &'static str,
    pub resource: &'static str,
    pub sizes: &'static [WidgetSize],
    pub size: WidgetSize,
    pub shown: bool,
}

/// Every widget, for the role editor
pub fn catalogue() -> Vec<WidgetInfo> {
    WIDGETS
        .iter()
        .map(|w| WidgetInfo {
            id: w.id,
            resource: w.resource.as_str(),
            sizes: w.sizes,
            size: w.size,
            shown: w.shown,
        })
        .collect()
}

/// One widget on a staff member's dashboard
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutSlot {
    pub widget: String,
    pub size: WidgetSize,
    #[serde(default)]
    pub hidden: bool,
}

/// A slot with the sizes its widget can take, for the apps' size picker
#[derive(Debug, Clone, Serialize)]
pub struct LayoutEntry {
    #[serde(flatten)]
    pub slot: LayoutSlot,
    pub sizes: &'static [WidgetSize],
}

/// Add the sizes each widget can take
pub fn with_sizes(layout: Vec<LayoutSlot>) -> Vec<LayoutEntry> {
    layout
        .into_iter()
        .map(|slot| LayoutEntry {
            sizes: find(&slot.widget).map(|w| w.sizes).unwrap_or(&[]),
            slot,
        })
        .collect()
}

/// What one of a staff member's roles contributes
#[derive(Debug, Clone, Copy)]
pub struct RoleWidgets<'a> {
    pub permissions: &'a [String],
    /// The widgets it offers, in order; `None`: every widget it can read
    pub widgets: Option<&'a [String]>,
}

/// The widgets offered to a staff member, as their default layout. Global
/// admins get every widget. Otherwise each role offers its own list (or every
/// widget), keeping those its permissions read; roles add up. A widget a role
/// lists explicitly is shown; the others keep their default visibility.
pub fn offered(is_admin: bool, roles: &[RoleWidgets]) -> Vec<LayoutSlot> {
    if is_admin {
        return WIDGETS.iter().map(default_slot).collect();
    }
    let mut slots: Vec<LayoutSlot> = Vec::new();
    for role in roles {
        let Ok(checker) = ScopeChecker::new(role.permissions) else {
            continue;
        };
        let listed: Vec<(&Widget, bool)> = match role.widgets {
            Some(ids) => ids
                .iter()
                .filter_map(|id| find(id))
                .map(|w| (w, true))
                .collect(),
            None => WIDGETS.iter().map(|w| (w, w.shown)).collect(),
        };
        for (widget, shown) in listed {
            if !checker.can(widget.resource, Action::Read) {
                continue;
            }
            match slots.iter_mut().find(|s| s.widget == widget.id) {
                Some(slot) => slot.hidden &= !shown,
                None => slots.push(LayoutSlot {
                    widget: widget.id.to_string(),
                    size: widget.size,
                    hidden: !shown,
                }),
            }
        }
    }
    slots
}

fn default_slot(widget: &Widget) -> LayoutSlot {
    LayoutSlot {
        widget: widget.id.to_string(),
        size: widget.size,
        hidden: !widget.shown,
    }
}

/// The dashboard to show: the saved layout, keeping the widgets still offered
/// (at a size they can still take), then the offered widgets it does not list
/// yet (a widget added later, a new role), as the default puts them
pub fn resolve(saved: Option<&[LayoutSlot]>, offered: &[LayoutSlot]) -> Vec<LayoutSlot> {
    let Some(saved) = saved else {
        return offered.to_vec();
    };
    let mut layout: Vec<LayoutSlot> = Vec::new();
    for slot in saved {
        let Some(default) = offered.iter().find(|o| o.widget == slot.widget) else {
            continue;
        };
        if layout.iter().any(|s| s.widget == slot.widget) {
            continue;
        }
        let fits = find(&slot.widget).is_some_and(|w| w.sizes.contains(&slot.size));
        layout.push(LayoutSlot {
            size: if fits { slot.size } else { default.size },
            ..slot.clone()
        });
    }
    for default in offered {
        if !layout.iter().any(|s| s.widget == default.widget) {
            layout.push(default.clone());
        }
    }
    layout
}

/// Check a layout a staff member saves: known widgets offered to them, once
/// each, at a size they can take
pub fn validate(layout: &[LayoutSlot], offered: &[LayoutSlot]) -> Result<Vec<LayoutSlot>> {
    let mut seen = HashSet::new();
    for slot in layout {
        let widget = find(&slot.widget)
            .ok_or_else(|| Error::validation(format!("Unknown widget '{}'", slot.widget)))?;
        if !offered.iter().any(|o| o.widget == slot.widget) {
            return Err(Error::validation(format!(
                "Widget '{}' is not available to you",
                slot.widget
            )));
        }
        if !seen.insert(widget.id) {
            return Err(Error::validation(format!(
                "Widget '{}' is listed twice",
                slot.widget
            )));
        }
        if !widget.sizes.contains(&slot.size) {
            return Err(Error::validation(format!(
                "Widget '{}' cannot take this size",
                slot.widget
            )));
        }
    }
    Ok(layout.to_vec())
}

/// Check and normalise a role's widget list: known ids, each once, in order
pub fn validate_role_widgets(ids: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        if find(id).is_none() {
            return Err(Error::validation(format!("Unknown widget '{}'", id)));
        }
        if !out.contains(id) {
            out.push(id.clone());
        }
    }
    Ok(out)
}

/// Check and normalise a role's menu entries. The ids belong to the apps, so
/// only their form is checked: 1 to 40 of `a-z`, `0-9`, `_` and `-`, at most
/// 100 entries, each once.
pub fn validate_menu(ids: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        let well_formed = (1..=40).contains(&id.len())
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if !well_formed {
            return Err(Error::validation(format!("Invalid menu entry '{}'", id)));
        }
        if !out.contains(id) {
            out.push(id.clone());
        }
    }
    if out.len() > 100 {
        return Err(Error::validation("A role lists at most 100 menu entries"));
    }
    Ok(out)
}

/// The menu entries shown to a staff member: `None` (everything their
/// permissions allow) for global admins and when one of their roles does not
/// narrow its menu, the roles' entries added up otherwise
pub fn menu(is_admin: bool, roles: &[Option<&[String]>]) -> Option<Vec<String>> {
    if is_admin || roles.is_empty() || roles.iter().any(Option::is_none) {
        return None;
    }
    let mut entries: Vec<String> = Vec::new();
    for id in roles.iter().flatten().flat_map(|ids| ids.iter()) {
        if !entries.contains(id) {
            entries.push(id.clone());
        }
    }
    Some(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    fn ids(slots: &[LayoutSlot]) -> Vec<&str> {
        slots.iter().map(|s| s.widget.as_str()).collect()
    }

    fn slot(widget: &str, size: WidgetSize, hidden: bool) -> LayoutSlot {
        LayoutSlot {
            widget: widget.into(),
            size,
            hidden,
        }
    }

    #[test]
    fn registry_ids_are_unique_and_default_sizes_allowed() {
        let mut seen = HashSet::new();
        for w in WIDGETS {
            assert!(seen.insert(w.id), "{} twice", w.id);
            assert!(w.sizes.contains(&w.size), "{}", w.id);
        }
    }

    #[test]
    fn a_role_without_reports_is_not_offered_finance_widgets() {
        let counter = strings(&["orders:write", "inventory:read"]);
        let offered = offered(
            false,
            &[RoleWidgets {
                permissions: &counter,
                widgets: None,
            }],
        );
        assert_eq!(ids(&offered), ["orders", "stock", "invoices"]);
        // listing a widget does not open it either
        let listed = strings(&["finance", "orders"]);
        let offered = super::offered(
            false,
            &[RoleWidgets {
                permissions: &counter,
                widgets: Some(&listed),
            }],
        );
        assert_eq!(ids(&offered), ["orders"]);
    }

    #[test]
    fn a_role_list_sets_order_and_shows_its_widgets() {
        let manager = strings(&["reports:read", "orders:read"]);
        let listed = strings(&["finance", "orders", "nope"]);
        let offered = offered(
            false,
            &[RoleWidgets {
                permissions: &manager,
                widgets: Some(&listed),
            }],
        );
        assert_eq!(ids(&offered), ["finance", "orders"]);
        assert!(offered.iter().all(|s| !s.hidden));
    }

    #[test]
    fn roles_add_up_and_admins_get_everything() {
        let counter = strings(&["orders:read"]);
        let stock = strings(&["inventory:write"]);
        let only_stock = strings(&["stock"]);
        let offered = offered(
            false,
            &[
                RoleWidgets {
                    permissions: &counter,
                    widgets: None,
                },
                RoleWidgets {
                    permissions: &stock,
                    widgets: Some(&only_stock),
                },
            ],
        );
        assert_eq!(ids(&offered), ["orders", "invoices", "stock"]);
        assert_eq!(super::offered(true, &[]).len(), WIDGETS.len());
        assert!(super::offered(true, &[])
            .iter()
            .any(|s| s.widget == "finance" && s.hidden));
    }

    #[test]
    fn resolve_keeps_the_saved_order_drops_withdrawn_and_adds_new_widgets() {
        let offered = vec![
            slot("orders", Large, false),
            slot("stock", Medium, false),
            slot("finance", Medium, true),
        ];
        let saved = vec![
            slot("stock", Small, true),
            slot("sales", Large, false),  // no longer offered
            slot("orders", Small, false), // a size orders can take
            slot("orders", Large, false), // twice: the first one counts
        ];
        let layout = resolve(Some(&saved), &offered);
        assert_eq!(
            layout,
            vec![
                slot("stock", Small, true),
                slot("orders", Small, false),
                slot("finance", Medium, true),
            ]
        );
        assert_eq!(resolve(None, &offered), offered);
    }

    #[test]
    fn resolve_replaces_a_size_the_widget_cannot_take() {
        let offered = vec![slot("seasonality", Large, true)];
        let saved = vec![slot("seasonality", Small, false)];
        assert_eq!(
            resolve(Some(&saved), &offered),
            vec![slot("seasonality", Large, false)]
        );
    }

    #[test]
    fn validate_refuses_unknown_withdrawn_twice_and_wrong_sizes() {
        let offered = vec![
            slot("orders", Large, false),
            slot("seasonality", Large, true),
        ];
        let good = vec![
            slot("seasonality", Large, false),
            slot("orders", Small, true),
        ];
        assert_eq!(validate(&good, &offered).unwrap(), good);
        for bad in [
            vec![slot("nope", Large, false)],
            vec![slot("finance", Medium, false)],
            vec![slot("orders", Large, false), slot("orders", Small, false)],
            vec![slot("seasonality", Small, false)],
        ] {
            assert_eq!(validate(&bad, &offered).unwrap_err().status_code(), 400);
        }
    }

    #[test]
    fn role_lists_are_checked_and_deduplicated() {
        assert_eq!(
            validate_role_widgets(&strings(&["sales", "orders", "sales"])).unwrap(),
            strings(&["sales", "orders"])
        );
        assert!(validate_role_widgets(&strings(&["nope"])).is_err());
        assert_eq!(
            validate_menu(&strings(&["orders", "price_lists", "orders", "till-2"])).unwrap(),
            strings(&["orders", "price_lists", "till-2"])
        );
        for bad in ["", "Orders", "a b", "x".repeat(41).as_str()] {
            assert!(validate_menu(&strings(&[bad])).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn menu_is_unrestricted_when_any_role_is() {
        let a = strings(&["orders", "counter"]);
        let b = strings(&["stock", "orders"]);
        assert_eq!(
            menu(false, &[Some(&a), Some(&b)]),
            Some(strings(&["orders", "counter", "stock"]))
        );
        assert_eq!(menu(false, &[Some(&a), None]), None);
        assert_eq!(menu(true, &[Some(&a)]), None);
        assert_eq!(menu(false, &[Some(&[])]), Some(vec![]));
    }

    #[test]
    fn slots_serialize_with_lowercase_sizes() {
        let json = serde_json::to_string(&slot("orders", Medium, false)).unwrap();
        assert_eq!(
            json,
            r#"{"widget":"orders","size":"medium","hidden":false}"#
        );
        let parsed: LayoutSlot =
            serde_json::from_str(r#"{"widget":"stock","size":"small"}"#).unwrap();
        assert!(!parsed.hidden);
    }
}
