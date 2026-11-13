# Price and promotion history

What each order line was charged and why, what each promotion saved each
order, and every negotiated rule price lists had over time. The history lives
in its own tables, so promotions, price lists and orders stay light:

| Table | Written | Content |
|---|---|---|
| `order_line_pricing` | when an order is placed | One row per order line: catalogue price (`base_price`), price before catalogue promotions (`negotiated_price`, from the price list when one applies), price charged (`unit_price`), the line's share of cart promotions, coupon and points (`discount`), cost price, price list and its name, and the catalogue promotions behind the price with what each saved (`promotions`: `[{id, name, kind, saving}]`). Customer and company at the time. |
| `promotion_uses` | when an order is placed | One row per promotion and order (usage limits), with the promotion's `name` and `kind` and what it saved the order (`discount`). Rows stay when the promotion is deleted; a cancelled or refunded order removes its rows. |
| `price_list_rule_history` | by database triggers | One row per rule and period, with the list as it was (name, company or group, active, dates). `valid_to` is empty for the rules in force. Adding, changing or removing a rule closes its row and opens the next; changing a list's name, holder, dates or switch does the same for all its rules. |
| `price_history` | by database triggers | Every catalogue price of a product or variant, from when (already used for the lowest price of the last 30 days). |

Orders placed before this history existed get a row per line with the price
charged, and the catalogue price of the day when it is known.

How a saving is shared between catalogue promotions: a single promotion takes
the whole difference between the negotiated price and the price charged; a
combinable chain shares it step by step in the order it is applied (fixed
price, percentages, amounts off). A gift's saving is its catalogue price.
Cart promotions (buy X get Y, tiers) count at order level, in
`promotion_uses.discount`; per line they are part of `discount`.

## A product's history

`GET /api/v1/admin/products/:id/price-history` (products area, read)

| Parameter | Effect |
|---|---|
| `customer` | Negotiated rules of that customer's company and group only; prices charged to that customer only |
| `from`, `to` | RFC 3339 instants: rule periods that overlap, lines charged in between |
| `limit` | Lines charged, newest first (default 200, at most 1000) |

```json
{
  "title": "Panneau OSB 18 mm", "sku": "ST-0101",
  "catalogue": [{"variant_id": null, "variant_title": null, "price": "30.13", "valid_from": "..."}],
  "promotions": [{"id": "...", "name": "Semaine OSB", "kind": "percent_off", "audience": "customer",
                  "value": "15", "combinable": false, "is_active": true, "starts_at": null, "ends_at": "..."}],
  "negotiated": [{"price_list_name": "Trade", "company_name": null, "group_name": "Trade",
                  "scope": "category", "adjustment": "percent_off", "value": "10", "min_quantity": 1,
                  "valid_from": "...", "valid_to": null, "list_active": true, "price": "27.12", "...": "..."}],
  "charged": [{"order_number": "ORD-...", "status": "completed", "created_at": "...",
               "customer_name": "Marie Client", "company_name": null, "quantity": 2,
               "base_price": "30.13", "negotiated_price": "30.13", "unit_price": "25.61",
               "discount": "0.00", "net_unit_price": "25.6100", "price_list_name": null,
               "promotions": [{"name": "Semaine OSB", "kind": "percent_off", "saving": "9.04"}], "...": "..."}]
}
```

`promotions` lists the promotions that target the product (by product,
variant, category or the whole catalogue), live or not, without spending
tiers and free delivery. A rule period's `price` is what it gave the product
at the catalogue price when the period started. 404 for an unknown product.

## A customer's prices

`GET /api/v1/admin/customers/:id/price-history` (customers area, read) takes
`product`, `from`, `to` and `limit` and answers the customer's `name`,
`email`, `company_name`, the `lines` they were charged (as `charged` above)
and the `negotiated` rules in force for their company and group, each with
the name of the product, variant or category it targets (`target_name`). 404
for an unknown customer.

## A promotion's uses

`GET /api/v1/admin/promotions/:id/uses?page=&per_page=` (coupons area, read)

```json
{ "name": "Semaine OSB", "orders": 12, "discount": "84.30", "sales": "1210.55",
  "uses": [{"order_id": "...", "order_number": "ORD-...", "status": "completed", "created_at": "...",
            "total": "61.98", "customer_id": "...", "customer_name": "Marie Client",
            "email": "...", "discount": "9.04"}] }
```

`sales` adds up the orders' totals (VAT included). The name stays available
after the promotion is deleted.
