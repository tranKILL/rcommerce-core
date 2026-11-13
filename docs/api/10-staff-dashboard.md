# Staff dashboard, own rights and list sorting

Four things back-office apps need besides the screens themselves: what the
signed-in staff member may do and is shown, a home page with the work waiting
and the store's figures, that home arranged by each staff member, and lists
sorted on any column.

## Own rights

`GET /api/v1/admin/staff/me` answers any staff member (any role) and global
admins; other accounts get 403.

```json
{
  "is_admin": false,
  "permissions": ["inventory:read", "orders:write"],
  "menu": ["orders", "counter", "stock"],
  "widgets": ["orders", "stock", "invoices"],
  "max_discount_percent": "5.00"
}
```

- `menu`: the back-office menu entries the caller's roles show, or `null`
  when one of their roles does not narrow its menu. The ids belong to the app.
- `widgets`: the dashboard widgets offered to the caller (see below).
- `max_discount_percent`: the largest manual discount the caller may give at
  the till: the highest cap of their roles, `0` when none sets one.

Global admins get `"is_admin": true`, an empty permission list, `menu: null`,
every widget and `100`. Apps build their menus from it; the API still checks
every call (see [staff-roles.md](staff-roles.md)): menus and widgets only
narrow what is shown.

## Dashboard

`GET /api/v1/admin/dashboard?location_id=<uuid>&widgets=<ids>` answers the
same callers. It computes the widgets listed in `widgets` (comma-separated;
default: `orders,stock,invoices,sales,customers,catalogue`) that are offered to
the caller, and leaves the others `null`. A widget is offered when one of the
caller's roles can read its resource (`write` includes `read`) and, if that
role lists widgets, lists it; global admins are offered every widget.
`location_id` narrows orders, sales, stock and the reports to one branch.

| Part | Resource | Content |
|---|---|---|
| `orders` | `orders` | Open orders by next step: `pickups_to_prepare` (of which `pickups_late`, past the promised time), `pickups_waiting` at the counter (of which `pickups_waiting_long`, ready for more than two days), `deliveries_to_dispatch`, `deliveries_out`, and `awaiting_payment` (placed in the last 7 days). `next`: up to 8 orders still to prepare or dispatch, earliest promise first, in the `GET /orders` format. |
| `invoices` | `orders` | `unpaid_count`/`unpaid_amount`, `overdue_count`/`overdue_amount` (due date passed), `peppol_failed`, and `month_count`/`month_amount` (issued since the first of the month). |
| `sales` | `reports` | `today`, `week` (from Monday), `month`, and `previous_month_to_date` (the same days of last month), each `{orders, revenue, revenue_excl_vat}`; `days`: the last 14 days, oldest first, missing days as zero. Pending, cancelled, refunded and test orders are left out. |
| `stock` | `inventory` | Online products' stock lines: `out_of_stock` (nothing free), `low` (some left, at or below the reorder point), and `levels`: the 8 emptiest such lines, in the stock level list format. |
| `customers` | `customers` | `new_customers` and `new_companies` (last 7 days), `companies` (active). |
| `catalogue` | `products` | Products `online`, `offline`, and `online_without_stock` (free stock nowhere). |
| `finance` | `reports` | The finance report of the last 12 months with 6 projected months, as `GET /admin/analytics/finance` returns it (see [12-finance-and-marketing.md](12-finance-and-marketing.md)). Computed once for the `finance` and `projection` widgets. |
| `marketing` | `reports` | The marketing report with only the sections the widgets need: `top_customers` and `top_companies` (widget `top_customers`), `seasonality`, `restock`, `promotions` (widget `promotion_results`), `loyalty`. The other sections are empty. |

"Today" and "this month" follow `[invoicing] timezone` (Europe/Brussels by
default). Test orders count as work, since they are on the counter boards too,
but not as sales.

## Widgets and personal layouts

| Widget | Resource | Sizes (default first) | Shown by default |
|---|---|---|---|
| `orders` | `orders` | large, small, medium | yes |
| `stock` | `inventory` | medium, small, large | yes |
| `invoices` | `orders` | medium, small, large | yes |
| `sales` | `reports` | large, medium | yes |
| `customers` | `customers` | medium, small, large | yes |
| `catalogue` | `products` | medium, small, large | yes |
| `finance` | `reports` | medium, small, large | no |
| `projection` | `reports` | large, medium | no |
| `top_customers` | `reports` | medium, large | no |
| `seasonality` | `reports` | large | no |
| `restock` | `reports` | large, medium | no |
| `promotion_results` | `reports` | large, medium | no |
| `loyalty` | `reports` | small, medium | no |

Sizes are `small`, `medium` and `large`: a quarter, half or the whole width on
wide screens, as the app lays them out. The analytics widgets cost more to
compute and stay hidden until someone shows them, unless a role lists them:
the widgets a role lists are shown, in its order.

Each staff member arranges their own dashboard:

| Method and path | Body | Answer |
|---|---|---|
| `GET /admin/staff/me/dashboard` | | `{"layout": [{"widget", "size", "hidden", "sizes"}], "customised"}` |
| `PUT /admin/staff/me/dashboard` | `{"layout": [{"widget", "size", "hidden"?}]}` | the same as `GET` |
| `DELETE /admin/staff/me/dashboard` | | 204; their roles' default applies again |

The layout lists every widget offered to the caller, in order. Until they save
one (`customised: false`) it is their roles' default. A saved layout keeps
only the widgets still offered, at a size they can still take, and widgets
offered later are added at the end as their default puts them. `PUT` refuses
with 400 a widget that is unknown, not offered, listed twice or at a size it
cannot take.

## Sorting staff lists

Staff lists take `sort=<column>`, with `-` in front for descending order
(`sort=-total`). Missing values come last either way, and rows with equal
values keep a stable order across pages. An unknown column is a 400 that lists
the valid ones.

| List | Columns (default first) |
|---|---|
| `GET /orders` | `-date`, `number`, `customer`, `items`, `total`, `mode` (pickup first), `stage` (awaiting payment, received, preparing, ready, shipped, completed, then cancelled or refunded) |
| `GET /admin/products` | `title`, `sku`, `price`, `available`, `active`, `featured`, `updated` |
| `GET /customers` (staff) | `-since`, `name`, `email`, `phone` |
| `GET /admin/invoices` | `-date`, `number`, `customer`, `total`, `due` (amount still due), `due_date`, `peppol` |
| `GET /admin/companies` | `-since`, `name`, `vat_number`, `status` |
| `GET /admin/inventory/levels` | `product`, `sku`, `branch`, `available`, `reserved`, `reorder_point` |
| `GET /admin/inventory/movements` | `-date`, `product`, `branch`, `quantity`, `reason`, `reference` |

Text columns sort without regard to case. The staff customer search (`q`) also
matches phone numbers, and the movement history takes `q` (product title, SKU
or the movement's reference).
