# Promotion preview

`POST /admin/promotions/preview` takes a promotion as `POST /admin/promotions` does
and answers what it would do, without saving anything. It only reads, so staff with
`coupons` read may call it. `?exclude=<id>` leaves the promotion being edited out of
the overlaps. A draft that would be refused on save is a `400` with the same message.

```json
{
  "reach": 42,
  "samples": [{ "product_id": "…", "title": "Cement 25 kg", "sku": "C25",
                "base": "8.50", "price": "7.65" }],
  "carts": [{ "label": "at_min", "threshold": "100", "subtotal": "102.00",
              "discount": "10", "free_delivery": false, "gift_quantity": 0,
              "lines": [{ "title": "Cement 25 kg", "quantity": 12, "unit_price": "8.50" }] }],
  "overlaps": [{ "id": "…", "name": "Winter -5 %", "kind": "percent_off", "combinable": false }],
  "warnings": ["first_order_only"]
}
```

- **`reach`:** active products the promotion covers (its scope: all, a category
  with the categories below it, a product, a variant).
- **`samples`:** up to 8 of them, with the price for one unit (or the minimum
  quantity) before and after, excluding VAT. Cart kinds keep the price.
- **`carts`:** example carts of one covered product, priced by the cart rules:

  | Label | When |
  |---|---|
  | `below_min`, `at_min` | just under and at the minimum amount |
  | `below_tier`, `at_tier` | around each spending tier (`threshold` names it) |
  | `buy_only`, `buy_and_get` | buy X get Y: X units, then X + Y |
  | `one` | other cart kinds without a minimum |
- **`overlaps`:** live promotions for the same audience that reach a covered product
  (checked on up to 200 of them).
- **`warnings`:**
  - `first_order_only`: shown in carts only, never on listings;
  - `starts_in_past`;
  - `already_ended`;
  - `inactive`;
  - `no_products`;
  - `cart_amount_needed`: a catalogue price that waits for a cart amount.
