# Cart reminders

A signed-in customer who leaves a cart without ordering gets a reminder e-mail. Reminders
count as commercial e-mails, so they only go to customers who accept them, with a "yes" on
record (see [16-marketing-consent.md](16-marketing-consent.md)) and a confirmed address.
Guest carts get none.

## Settings

```toml
[carts]
reminder_after_hours = [4, 48]     # hours after the customer's last change; empty: off
reminder_min_subtotal = "20.00"    # optional: smaller carts (excl. VAT) get none
```

The job runs every 15 minutes, one store instance at a time, and only with a
notification service configured.

## When a reminder goes

- The schedule counts from the cart's last change. Each step is sent at most once in
  that idle period, and a new change starts a new period. Any write to the cart counts,
  not only the customer's: a staff edit or a loyalty repricing also starts a new period
  (the quiet hours below still apply).
- When the job has been off for a while, only the latest step due is sent, not all of
  them.
- An order by the customer since the period started stops it, and so does a "no" to
  commercial e-mails.
- A customer gets at most one reminder in 20 hours, and one per run when several of
  their carts are due.
- The customer's choice is read again just before sending: a "no" given while a run is
  under way stops their reminder.
- Carts idle for longer than the last step plus a day get none, so turning the setting
  on does not remind old carts.
- A reminder that cannot be prepared (repricing or VAT fails, nothing left to show)
  counts as a failed try and the run goes on with the next cart.
- A failed send is tried again after 15 and 30 minutes, then given up for that step.
  It also ends the run, since the mail service may be down.

## What it says

Before each reminder the cart is repriced with the customer's current price lists and
promotions, as checkout does, so the e-mail shows today's prices. Repricing changes the
cart's `updated_at`, which `cart_reminders.cart_seen_at` records, so the cart stays in
the same idle period.

- Up to 6 lines, each with its picture (web addresses only), name, quantity and price.
  Further lines are counted.
- The total after cart discounts, delivery included.
- Prices for trade customers (company members) exclude VAT. For others they include
  VAT, worked out as checkout does at the customer's default delivery address, else at
  the first public branch. When no rate applies, prices exclude VAT and the e-mail says
  so.
- A link to `<storefront_url>/cart?lang=<language>`.
- The customer's unsubscribe link, in the text and in the `List-Unsubscribe` header.
  One-click unsubscribe (`List-Unsubscribe-Post`, RFC 8058) needs a public API address
  and is not sent.
- Texts in the customer's language (10 catalogues, section `cart_reminder`).
- The same reminder as a push notification on the customer's phones when the store
  sends push (see [18-push-notifications.md](18-push-notifications.md)): the item count
  and total, opening the cart.

## Staff

`GET /admin/analytics/cart-reminders?days=30` (`reports` read; `days` from 1 to 366):

```json
{ "days": 30, "schedule_hours": [4, 48],
  "stats": { "sent": 12, "customers": 10, "orders": 3, "revenue": "412.50" } }
```

`orders` counts the orders (not drafts, cancelled or refunded) that a reminded customer
placed within 7 days of a reminder. `revenue` is their total, VAT included.

Table: `cart_reminders` (migration 051). Rows go when their cart is deleted, for example
by the cart cleanup (`delete_inactive_after_months`).
