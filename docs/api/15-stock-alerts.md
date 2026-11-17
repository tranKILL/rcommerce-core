# Stock alerts

A signed-in customer asks to be told when a product is in stock at a branch, or at
any branch, in the quantity they want. The store e-mails them once the stock is
there, then closes the alert.

Only stock at the branch counts: its saleable quantity, as shown per branch (free
stock minus active cart holds; see stock per branch). Stock expected from a supplier
does not.

## Customer routes (JWT)

### `POST /customers/me/alerts`

```json
{ "product_id": "…", "location_id": "…", "variant_id": null, "quantity": 4 }
```

- `location_id`: a public, active branch; leave it out for any branch.
- `location`: the branch's slug, instead of `location_id` (not both: `400`).
- `quantity`: 1 to 9999 (default 1).
- The same product, variant and branch again changes the quantity of the open alert
  instead of opening a second one.

Answers `201` with the alert. Refusals:
- `404`: unknown product, variant or branch (private branches count as unknown).
- `409`: the branch (or a branch, without one) holds the quantity now: it can be
  ordered.
- `400`: the product does not track stock (always in stock), a quantity out of
  range, or 50 alerts open or sent in the last day (asking again for an open one
  changes it and is not counted).

```json
{
  "id": "…", "product_id": "…", "variant_id": null, "location_id": "…",
  "quantity": 4, "created_at": "…", "notified_at": null,
  "product_title": "…", "product_slug": "…", "location_name": "North depot"
}
```

### `GET /customers/me/alerts`

`{ "alerts": [...] }`: open alerts (newest first), then those sent in the last 30
days (`notified_at` set).

### `DELETE /customers/me/alerts/:id`

Closes an open alert of the caller: `204`, or `404` when there is none (someone
else's, already sent or already closed).

## Sending

A background loop checks the open alerts every 5 minutes, one store instance at a
time (advisory lock `stock_alerts`). It runs only when the notification service is
configured: without one, alerts are accepted but never sent. Only customers with a
verified e-mail address are written to. An alert is due when its branch has at least the quantity wanted; an
alert without a branch is due when any public branch does, and the e-mail names
the first such branch in display order.

The e-mail (template `stock_alert_html`, section `[stock_alert]` of the e-mail
catalogues, in the customer's language) names the product, the branch and the
quantity available, and links to `<storefront_url>/products/<slug>?lang=<locale>`.
An alert closes once its e-mail is sent. A failed send (or one taking over 30
seconds) counts against that alert and ends the run, so an outage of the mail
service does not use up the others; the next try waits 15 minutes, doubling, and
the fifth failure closes it. Alerts tried less go first. Closed alerts are deleted
after 90 days. Each run also closes the
alerts that can no longer fire (product off sale or no longer tracking stock, branch
no longer public and active).

Table: `stock_alerts` (migration 049): one open alert per customer, product,
variant and branch.
