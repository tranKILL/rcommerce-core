# Orders API: lists and pickup at the counter

Customers list their own orders; staff with `orders:read` list every order.
Pickup orders (see [collecting at a store location](../architecture/checkout.md#collecting-at-a-store-location))
are then prepared, made ready and handed over at the counter by staff with `orders:write`.

## List orders

`GET /api/v1/orders`, newest first. Lines are left out (`items` is empty); `item_count`
gives the number of articles. `GET /api/v1/orders/:id` returns one order with its lines.

| Parameter | Meaning |
|---|---|
| `page`, `per_page` | Paging; `per_page` defaults to 50, at most 100 |
| `status` | `pending`, `confirmed`, `processing`, `on_hold`, `completed`, `cancelled` or `refunded`; anything else is a `400` |
| `q` | Part of the order number or of the customer's e-mail (case-insensitive) |
| `location_id` | Orders whose stock comes from this location (pickup or delivery) |
| `pickup` | `true` for pickup orders only, `false` for the others |

```json
{
  "orders": [
    {
      "id": "…", "order_number": "ORD-20261109-2817106666",
      "status": "confirmed", "payment_status": "paid", "fulfillment_status": "pending",
      "total": "77.44", "item_count": 1,
      "fulfillment_location_id": "96b2803c-d951-4d24-9de2-26687cdc9cc4",
      "pickup": {
        "location_id": "96b2803c-d951-4d24-9de2-26687cdc9cc4",
        "location_name": "North branch",
        "ready_at": "2026-11-10T06:00:00Z"
      }
    }
  ],
  "meta": { "total": 1, "page": 1, "per_page": 50, "total_pages": 1 }
}
```

Every order response carries `pickup` (pickup orders only) and `fulfillment_location_id`
(when checkout chose a location).

## Pickup at the counter

| Step | Request | Allowed when | Effect |
|---|---|---|---|
| Ready | `POST /api/v1/orders/:id/pickup/ready` | `confirmed` or `processing`, not yet ready | status `processing`, fulfillment `processing`, `pickup.ready_marked_at` |
| Collected | `POST /api/v1/orders/:id/pickup/collected` with `{"collected_by": "J. Mason"}` | ready | status `completed`, a delivered fulfillment for every line, `pickup.collected_at` and `collected_by`, a note on the order |

- `collected_by` is required (at most 100 characters).
- Collecting takes the order's committed stock out of the location's reserved quantity:
  the goods have left the counter.
- Other orders, or a step out of order, give `400`; customers get `403`.
