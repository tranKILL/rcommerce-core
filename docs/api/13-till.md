# Till (point of sale)

Counter sales go through the same services as the web shop: prices (price
lists, promotions, coupon, loyalty points), VAT, stock, the order and its
history, points and invoices. Every route below needs the `pos` permission
(`pos:read` for `GET`, `pos:write` otherwise; see [staff-roles.md](staff-roles.md)).
A suitable role also reads products, customers and stock and writes orders:

```json
{ "name": "Cashier", "permissions": ["pos:write", "orders:write", "customers:read",
  "products:read", "inventory:read"], "max_discount_percent": 10 }
```

## Sessions and cash

A cashier opens one session at a time, at an active branch, with the cash
float in the drawer. Sales, refunds, cash put in and cash taken out are its
movements; closing counts the drawer and numbers the branch's Z report.

| Method and path | Body | Answer |
|---|---|---|
| `POST /admin/pos/sessions` | `{"location_id", "opening_float"}` | 201, the session; 400 when the cashier already has one open |
| `GET /admin/pos/sessions/current` | | the caller's open session as a report (below), or `null` |
| `GET /admin/pos/sessions?location_id=&limit=` | | `{"sessions": [...]}`, newest first |
| `GET /admin/pos/sessions/:id` | | the report |
| `POST /admin/pos/sessions/:id/cash` | `{"kind": "cash_in" or "cash_out", "amount", "reason"}` | the report; a reason is required to take cash out |
| `POST /admin/pos/sessions/:id/close` | `{"counted_cash", "notes"?}` | the Z report |

Only the cashier who opened a session moves cash in it, sells in it or closes
it (403 otherwise). A report is `{"session", "totals", "movements"}`:

- `totals.sales` and `totals.refunds`: count and amount per method (`cash`,
  `card`);
- `cash_in`, `cash_out`;
- `net`: sales minus refunds;
- `expected_cash`: float plus every cash movement.

A closed session also has `counted_cash`, `expected_cash`, `difference`
(counted minus expected) and `z_number` (per branch, from 1). Amounts are
accepted as JSON numbers or text, with a decimal point or comma.

## Codes scanned

`GET /admin/pos/lookup?session_id=&code=` tells what a scanned or typed code
names (USB scanners type the code and Enter):

| `kind` | When |
|---|---|
| `customer` | 13 digits starting with `29` with a valid EAN-13 check digit: a loyalty card; the holder and their company |
| `product` | a variant's or product's GTIN (8, 12, 13 or 14 digits, compared in their 13-digit form) or SKU (case ignored); `available` is the free stock at the session's branch |
| `order` | an order number |
| `not_found` | nothing |

## Card terminal

`[pos] terminal` picks the terminal: `simulated` (default) or `worldline`
(not available yet: card payments are refused with a message, take cash).

| Method and path | Body | Answer |
|---|---|---|
| `POST /admin/pos/terminal` | `{"session_id", "amount", "refund"?}` | 201, the transaction |
| `GET /admin/pos/terminal/:id` | | the transaction, asked again while `pending` |
| `POST /admin/pos/terminal/:id/cancel` | | the transaction, `cancelled`; 400 unless pending |

A transaction's `status` is `pending`, `approved`, `declined` or `cancelled`.
The simulated terminal declines amounts ending in ,13, keeps those ending in
,14 pending, and approves the others.

## Baskets and sales

`POST /admin/pos/baskets` creates the session's basket, or replaces the lines
of `basket_id`:

```json
{ "session_id": "…", "basket_id": null, "customer_id": "…",
  "lines": [{ "product_id": "…", "variant_id": null, "quantity": 3,
              "discount_percent": 10, "discount_reason": "Damaged bag" }],
  "coupon": "SPRING", "points": 200 }
```

- **Manual discount:** a line may carry a percentage with its reason. Above the
  caller's cap (`max_discount_percent`, the highest of their roles; 100 for
  global admins), the basket is refused with 403.
- **Pricing:** the discount applies after price lists and catalogue promotions.
  The order's price history keeps it as a saving of kind `manual`, named after
  its reason.
- **The answer:** the priced lines, excl. VAT:
  - `unit_price`, `original_price`, `manual_discount` per unit;
  - `discount_amount`, the line's share of the coupon, points and cart
    promotions;
  - `available` at the branch.

  It also has the buyer, `promotions` and the totals with VAT (`subtotal`,
  `discount_total`, `tax_total`, `total`).

`POST /admin/pos/sales` sells a basket:

```json
{ "session_id": "…", "basket_id": "…", "cash_given": 50,
  "card_payments": ["<approved terminal transaction>"], "invoice": false }
```

- **Payment:** card payments must be approved, unused and from this session.
  Cash covers what the cards did not, and the change is given back (400 when
  short).
- **The order:** goes through checkout with the branch's pickup rate, so stock
  is taken at the branch (400 when there is not enough). It is then handed
  over at once, completed and delivered.
- **What is recorded:**
  - order metadata `source: "pos"`, `till_session_id`, `cashier_id`;
  - a `sold_at_till` event with the cash, card and change;
  - the session's movements;
  - the payment rows by method (`cash`, `debit_card`).
- **Points and invoice:** loyalty points are earned. Business customers (or any
  sale with `"invoice": true`) get their invoice when invoicing is on.
- **The answer:** `{"order", "settlement": {"card", "cash", "cash_given",
  "change"}, "invoice_number"}`.

Finance and marketing reports count these orders under the `pos` channel.
