# Staff roles

Staff roles give back-office accounts part of the admin API, for example
product management without statistics or billing. Global admins
(`customers.role = 'admin'`) keep full access and need no role. An account
without a role has no admin access, which is the behaviour before roles existed.

## Permissions

A role lists permissions in the API key scope format, `<resource>:<action>`:

- `read` opens the GET routes of an area, `write` opens all of them (write
  includes read).
- Only `read` and `write` are accepted. Wildcards (`read`, `write`, `admin`) and
  `<resource>:admin` are refused, so a role never grants admin rights.
- An account holding several roles gets the union of their permissions.
- Only the role's permissions count on admin routes, not the scopes in the
  account's token.
- Changes to a role or to an account's roles apply to the next request; nothing
  is cached and tokens do not need to be renewed.

| Admin routes (`/api/v1`) | Resource |
|---|---|
| `/admin/products/...`, `POST /products`, `PUT/DELETE /products/:id` | `products` |
| `/admin/orders/...` | `orders` |
| `/coupons/...` (`POST /coupons/validate` needs only `read`) | `coupons` |
| `/admin/statistics/...`, `/admin/stats` | `reports` |
| `/admin/subscriptions/...`, `/admin/dunning/...` | `payments` |
| `/admin/inventory/...` | `inventory` |
| `/admin/companies/...`, `/admin/customer-groups/...` | `customers` |
| `/admin/price-lists/...` | `products` |
| `/admin/invoices/...` | `orders` |

`GET /admin/dashboard`, `GET /admin/staff/me` and the caller's own layout
(`GET`, `PUT` and `DELETE /admin/staff/me/dashboard`) are open to any staff
member, whatever their roles; the dashboard shows each part only to roles that
can read it (see [10-staff-dashboard.md](10-staff-dashboard.md)).

Other routes stay reserved to global admins: `/admin/api-keys` and
the staff management routes below. A new admin route is closed to staff until
it is added to the table in `crates/rcommerce-api/src/middleware/staff_access.rs`.

## Other accounts' data on customer routes

Some customer routes let admins reach any account's data. A staff role opens
them as follows (an account's own data needs no role):

| Route | Needs |
|---|---|
| `GET /orders` (all orders instead of the caller's), `GET /orders/:id`, `GET /orders/:id/invoice` | `orders:read` |
| `POST /orders` for another customer, `POST /orders/:id/cancel`, `POST /orders/:id/fulfillments`, `PUT /orders/:id/status` | `orders:write` |
| `POST /orders/:id/refund` | `payments:write` |
| `GET /customers`, `GET /customers/:id` | `customers:read` |

Global admins pass without a role lookup. For anyone else, the lookup (one
indexed query) is made only when the data is not the caller's own, except on
`GET /orders`, which checks on each call whether to list all orders.
Payment, subscription and dunning routes still reserve other accounts' data to
global admins.

## Managing roles

These routes are for global admins only.

| Method and path | Body | Answer |
|---|---|---|
| `GET /admin/staff/roles` | | `{"roles": [...]}` |
| `POST /admin/staff/roles` | `{"name", "description"?, "permissions": [...], "menu"?, "widgets"?, "max_discount_percent"?}` | 201 `{"role": {...}}` |
| `PUT /admin/staff/roles/:id` | any of `name`, `description`, `permissions`, `menu`, `widgets`, `max_discount_percent` (`null` clears the last three) | `{"role": {...}}` |
| `GET /admin/staff/widgets` | | `{"widgets": [{"id", "resource", "sizes", "size", "shown"}]}` |
| `DELETE /admin/staff/roles/:id` | | 204; the role's assignments go with it |
| `GET /admin/staff/members` | | `{"members": [{"customer_id", "email", "role_ids"}]}` |
| `PUT /admin/staff/members/:customer_id/roles` | `{"role_ids": [...]}` | `{"customer_id", "role_ids"}`; replaces the account's roles, `[]` removes them |

What a role shows, without changing what it may do:

- `menu`: the back-office menu entries shown to its holders. The ids belong to
  the app (1 to 40 characters among `a-z`, `0-9`, `_` and `-`). `null` (the
  default) shows every entry its permissions allow. A holder of several roles
  sees the entries of all of them, and everything if one of them is `null`.
- `widgets`: the dashboard widgets it offers, in their default order, from
  `GET /admin/staff/widgets`. `null` (the default) offers every widget its
  permissions read. A listed widget the role cannot read is still not offered.
- `max_discount_percent`: the largest manual discount its holders may give at
  the till, 0 to 100, as a number or text. `null`: none.

Errors: 400 for an invalid permission, menu entry, widget or cap, or a
duplicate role name; 404 for an unknown role or account.

## Example

```bash
# A catalogue role
curl -X POST https://shop.example/api/v1/admin/staff/roles \
  -H "Authorization: Bearer $ADMIN_TOKEN" -H "Content-Type: application/json" \
  -d '{"name": "Catalogue", "permissions": ["products:write", "coupons:read"]}'

# Give it to an account
curl -X PUT https://shop.example/api/v1/admin/staff/members/$CUSTOMER_ID/roles \
  -H "Authorization: Bearer $ADMIN_TOKEN" -H "Content-Type: application/json" \
  -d '{"role_ids": ["'$ROLE_ID'"]}'
```

The account then signs in as usual (`POST /auth/login`) and uses its token on
the product and coupon admin routes.
