# Staff dashboard, own rights and list sorting

Three things back-office apps need besides the screens themselves: what the
signed-in staff member may do, a home page with the work waiting and the
store's figures, and lists sorted on any column.

## Own rights

`GET /api/v1/admin/staff/me` answers any staff member (any role) and global
admins; other accounts get 403.

```json
{ "is_admin": false, "permissions": ["inventory:read", "orders:write"] }
```

Global admins get `"is_admin": true` and an empty list. Apps build their menus
from it; the API still checks every call (see [staff-roles.md](staff-roles.md)).

## Dashboard

`GET /api/v1/admin/dashboard?location_id=<uuid>` answers the same callers.
Each part is filled only when one of the caller's roles can read its resource
(`write` includes `read`); the others are `null`. `location_id` narrows orders,
sales and stock to one branch.

| Part | Resource | Content |
|---|---|---|
| `orders` | `orders` | Open orders by next step: `pickups_to_prepare` (of which `pickups_late`, past the promised time), `pickups_waiting` at the counter (of which `pickups_waiting_long`, ready for more than two days), `deliveries_to_dispatch`, `deliveries_out`, and `awaiting_payment` (placed in the last 7 days). `next`: up to 8 orders still to prepare or dispatch, earliest promise first, in the `GET /orders` format. |
| `invoices` | `orders` | `unpaid_count`/`unpaid_amount`, `overdue_count`/`overdue_amount` (due date passed), `peppol_failed`, and `month_count`/`month_amount` (issued since the first of the month). |
| `sales` | `reports` | `today`, `week` (from Monday), `month`, and `previous_month_to_date` (the same days of last month), each `{orders, revenue, revenue_excl_vat}`; `days`: the last 14 days, oldest first, missing days as zero. Pending, cancelled, refunded and test orders are left out. |
| `stock` | `inventory` | Online products' stock lines: `out_of_stock` (nothing free), `low` (some left, at or below the reorder point), and `levels`: the 8 emptiest such lines, in the stock level list format. |
| `customers` | `customers` | `new_customers` and `new_companies` (last 7 days), `companies` (active). |
| `catalogue` | `products` | Products `online`, `offline`, and `online_without_stock` (free stock nowhere). |

"Today" and "this month" follow `[invoicing] timezone` (Europe/Brussels by
default). Test orders count as work, since they are on the counter boards too,
but not as sales.

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
