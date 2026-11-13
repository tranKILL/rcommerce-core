# Finance and marketing reports

Two reports for staff with `reports:read` (global admins always): finance
(sales, VAT, margin, invoices, payments, a projection) and marketing
(seasonality, restocking, best customers, promotions, loyalty). Each has a
CSV export.

## What counts

- **Sales** are orders that are neither `pending`, `cancelled`, `refunded`
  nor test orders. Amounts are excl. VAT (`total - tax_total`) unless a field
  says otherwise.
- **Days and months** follow the store's time zone (`invoicing.timezone`,
  Europe/Brussels by default).
- **Branch:** `location_id` keeps the orders fulfilled by that branch
  (`metadata.fulfillment_location_id`).
- **Audience:** `retail` keeps orders placed without a business account,
  `trade` those placed for one. Checkout records the account in
  `orders.company_id` (migration 041, which also fills it in for existing
  orders from their pricing snapshot, their invoice, or the customer's
  company today).
- **Margin** uses the cost price snapshot taken when the order was placed
  (`order_line_pricing.cost_price`), on the lines that have one. `costed`
  says how much of the goods sold that covers; older orders without a
  snapshot count in `goods` but not in `costed`.

## Query

Both reports take the same parameters:

| Parameter | Effect |
|---|---|
| `location_id` | One branch (default: all) |
| `audience` | `all` (default), `retail` or `trade` |
| `from`, `to` | Days, both included (default: the first day of the month 11 months ago, to today). `from` after `to` is a 400. |
| `horizon` | Finance only: months projected after the current one (default 6, at most 12) |

## Finance

`GET /api/v1/admin/analytics/finance`

```json
{
  "from": "2025-12-01", "to": "2026-11-13",
  "months": [{"month": "2025-12-01", "orders": 41, "sales": "5120.40", "sales_incl_vat": "6195.68",
              "vat": "1075.28", "discounts": "84.00", "goods": "4980.40", "costed": "3900.00",
              "cost": "2730.00", "margin": "1170.00"}],
  "total": {"orders": 512, "sales": "...", "sales_incl_vat": "...", "vat": "...", "discounts": "..."},
  "month_to_date": {"from": "2026-11-01", "to": "2026-11-13", "current": {...}, "last_year": {...}},
  "invoices": {"issued_count": 34, "issued_amount": "...", "paid_count": 30, "paid_amount": "...",
               "outstanding_count": 6, "outstanding_amount": "...", "overdue_count": 1, "overdue_amount": "..."},
  "takings": [{"channel": "online", "gateway": "stripe", "method": "credit_card", "payments": 480, "amount": "..."}],
  "open_orders": {"orders": 12, "sales": "..."},
  "projection": [{"month": "2026-11-01", "sales": "5400.00", "method": "last_year"}]
}
```

- `months` lists every month of the period, oldest first, zero when nothing
  was sold. `margin` is `costed - cost`.
- `month_to_date` compares this month up to today with the same days of the
  same month last year.
- `invoices`: `issued_*` and `paid_*` cover invoices issued in the period
  (`paid_count`: paid in full); `outstanding_*` and `overdue_*` are every
  invoice with something left to pay today, whenever issued.
- `takings`: payments with status `paid` received in the period, by channel
  (`online`, or the order's `metadata.source`), provider and method. Incl. VAT.
- `open_orders`: orders `confirmed`, `processing` or `on_hold`, whatever the
  period.
- `projection` covers the current month and the next `horizon` months, from
  the 24 complete months before the current one:
  - `last_year`: the same month last year × the trailing ratio (the last
    three complete months against the same months a year earlier, kept
    between ×0.25 and ×4);
  - `trend`: without last year, a straight line through up to the last 12
    complete months;
  - nothing with fewer than three complete months of history.

`GET /api/v1/admin/analytics/finance.csv` gives `months` as a CSV file.

## Marketing

`GET /api/v1/admin/analytics/marketing`

| Field | Content |
|---|---|
| `seasonality` | Per top-level category (sub-categories roll up): `sales` over the 24 complete months before the current one, whatever the period, and `index`, January to December, where 100 is the category's average month |
| `restock` | Up to 25 products whose sales rose in the coming 8 weeks last year (`last_year_coming` against the 8 weeks before, `last_year_before`). `expected` is last year's quantity times this year's trend (the last 8 weeks, `recent`, against the same weeks last year, between ×0.5 and ×2; ×1 when they sold nothing). `available` is the free stock at the branch (or everywhere, with `branches` per branch); `suggested` is what is missing. |
| `top_customers` | The 20 customers who bought most in the period, with their company, orders, sales, first and last order |
| `top_companies` | The 10 business accounts that bought most, with their number of buyers |
| `customer_months` | Per month: customers placing their first order ever (`new_*`), customers coming back (`returning_*`), guest orders |
| `baskets` | Orders, sales and average basket of `retail` and `trade` customers |
| `channels` | Orders and sales per channel |
| `promotions` | The 20 promotions that saved most in the period: orders, `discount`, sales of those orders, and the daily sales of the products the promotion priced in the 28 days before it started (`daily_before`) and while it ran (`daily_during`, up to now). Both are null for promotions without a start date or that priced no line. |
| `loyalty` | Store-wide: points earned, spent and lapsed in the period; points on cards today, card holders, `point_value` and the `liability` (outstanding × point value) |

`GET /api/v1/admin/analytics/marketing.csv?section=` gives one part as a CSV
file: `customers` (default), `companies`, `customer_months`, `seasonality`,
`restock` or `promotions`. Another section is a 400.

## CSV files

Comma-separated, a dot for decimals, UTF-8 with a byte order mark,
`Content-Disposition: attachment` with the report and period in the name.
