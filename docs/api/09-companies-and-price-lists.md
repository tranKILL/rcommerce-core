# Company accounts and price lists

Business customers buy through a **company account**: a company with a checked
VAT number, an owner and buyers. Companies belong to a **customer group**, and
**price lists** give a group or a single company negotiated prices.

## Company accounts (customer)

All routes need the customer's bearer token.

| Route | What it does |
|---|---|
| `POST /companies` `{vat_number, name?}` | Opens a company account; the caller becomes its owner. `name` defaults to the name in the VAT register. |
| `GET /companies/me` | The caller's company, their `role` (`owner` or `buyer`) and, for the owner, its `members`. 404 outside a company. |
| `POST /companies/me/members` `{email}` | The owner adds an existing customer as a buyer. |
| `DELETE /companies/me/members/:customer_id` | The owner removes a buyer. |

**VAT check.** The number may be typed with spaces, dots or dashes
(`be 0123.456.749`); old 9-digit Belgian numbers get their leading zero.
Belgian check digits are verified. With `companies.vat_check = "vies"` (the
default) the number must then be registered in the EU VIES register:

| Answer | When |
|---|---|
| 201 | The company is created, active at once, in the `trade` customer group |
| 400 | Not a valid VAT number, wrong check digits, or not registered (VIES) |
| 409 | The VAT number already has an account (ask its owner to add you), or the caller already belongs to a company |
| 503 | The VAT register did not answer; try again later |

`companies.vat_check = "format"` skips the register (development, tests).
`companies.vies_url` points the check at a test double.

A customer belongs to at most one company. A suspended company keeps its
members, but they get catalogue prices.

## Company accounts (staff)

Guarded as customer data (`customers:read`, `customers:write`).

| Route | What it does |
|---|---|
| `GET /admin/companies?q=&status=&page=&per_page=` | Newest first; `q` matches the name or VAT number |
| `GET /admin/companies/:id` | The company with its members |
| `PUT /admin/companies/:id` `{name?, status?, customer_group_id?}` | `status` is `active` or `suspended` |
| `GET /admin/customer-groups` | Every group (`trade` exists from the start) |
| `POST /admin/customer-groups` `{code, name, prices_exclude_tax}` | `prices_exclude_tax`: members see prices excluding VAT first |

## Price lists (staff)

Guarded as product data (`products:read`, `products:write`).

A price list targets **one customer group or one company** and can be limited
in time (`starts_at`, `ends_at`) or switched off (`is_active`). Its rules apply
to every product (`all`), a `category` (with its sub-categories), a `product`
or a `variant`, from `min_quantity` on a cart line:

| `adjustment` | `value` |
|---|---|
| `fixed` | the price excluding VAT |
| `percent_off` | 0 to 100 |
| `amount_off` | an amount off the catalogue price (never below zero) |

When several rules apply, **the lowest price wins**; a rule never raises a
price above the catalogue price. Values may be JSON numbers or decimal strings.

| Route | What it does |
|---|---|
| `GET /admin/price-lists?customer_group_id=&company_id=` | Newest first |
| `POST /admin/price-lists` `{name, customer_group_id \| company_id, starts_at?, ends_at?}` | Exactly one target |
| `GET /admin/price-lists/:id` | The list with its rules |
| `PUT /admin/price-lists/:id` `{name?, is_active?, starts_at?, ends_at?}` | `null` removes a date |
| `DELETE /admin/price-lists/:id` | With its rules |
| `POST /admin/price-lists/:id/rules` `{scope, target_id?, adjustment, value, min_quantity?}` | `target_id` for every scope but `all` |
| `DELETE /admin/price-lists/:id/rules/:rule_id` | |

## Prices a customer pays

`GET /customers/me/prices?product_ids=a,b&variant_ids=c` (at most 100 ids)
returns, for each product or variant, its `base_price` (catalogue), the
`price` the customer pays for one unit, the `price_list_id` it comes from, and
`tiers`: cheaper prices from larger quantities (`[{min_quantity, price}]`).
Catalogue responses stay the same for everyone, so they can be cached; a
storefront overlays these prices for a signed-in customer.

**In the cart**, a line takes the customer's price for its quantity whenever
the cart changes (adding, changing a quantity, merging a guest cart) and again
when checkout starts and completes, so a list that ended or changed never
reaches an order. Lines no price list ever priced are left as they are, so a
store without price lists behaves as before.
