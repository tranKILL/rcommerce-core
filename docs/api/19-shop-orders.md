# Web orders to the shop

A web order goes by e-mail to the shop that prepares it, with the order slip as a PDF,
so the shop's staff, or a printer with an e-mail address, get it without watching the
back office. The order's shop is `metadata.fulfillment_location_id`: the pickup
location, or the shop delivering it.

## Who receives a shop's orders

Each location keeps a list of up to 20 addresses: staff members, a shared mailbox, or
the e-mail address of a printer that prints what it receives.

- `GET /admin/locations/{id}/order-recipients`: `{ "recipients": [...], "slip_format":
  "a4" }`. Needs `settings` read.
- `PUT /admin/locations/{id}/order-recipients` replaces the list:

  ```json
  { "recipients": [
      { "email": "orders@central.example.com", "locale": "fr", "active": true },
      { "email": "alex@example.com", "customer_id": "<staff account id>", "locale": "nl" }
  ] }
  ```

  - Needs `settings` write **and** `orders` read: the addresses receive customers'
    names, phones and addresses.
  - Addresses are stored in lower case, once per shop, at most 254 characters, with a
    domain that has a dot.
  - `locale` is one of the store's e-mail languages (default: the store's);
    `active: false` keeps an address without sending to it.
  - `customer_id`, when given, must be an existing account.
  - `400` for a broken list, `404` for an unknown location. Each change is logged
    (`audit` target: who, which shop, how many addresses) and records who added
    each address.
- `POST /admin/locations/{id}/order-recipients/test` sends a sample slip to every active
  address, so the shop sees the e-mail and its printer prints it. Same permissions as
  `PUT`. At most one test a minute per shop (`429` before that); `400` when e-mail is not
  configured. Answers `{ "sent": 2 }`.

## Sending

A job runs every minute (first run 30 seconds after start, one server at a time per
store). It takes up to 50 orders that:

- are not drafts, and are confirmed or later (confirmed, processing, shipped,
  delivered, completed);
- were placed on the web: till sales are left out;
- were placed in the last 24 hours and not sent yet.

Each active address gets one e-mail in its language, with the slip in the shop's
paper size. The job then sets `metadata.shop_notified` and adds the order event
`shop_notified`, so each order goes once whatever the checkout path. When no
address could be reached, the order is tried again on the next run. When the order
has no shop, or its shop has no address, the operator's new-order e-mail
(`notifications.admin_email`) goes instead, unless it was already sent.

## The order slip

`GET /admin/orders/{id}/slip.pdf?format=a4|ticket80&lang=fr` (`orders` read) returns the
slip to print, `Cache-Control: no-store`. `format` defaults to the shop's `slip_format`
(A4 when the order has no shop); `lang` defaults to the store's e-mail language.

It shows the store and order number, when the order was placed (in the shop's time
zone), pickup at the shop with the time it should be ready, or delivery with the
service chosen, whether it is paid or still to pay, the customer (name, e-mail, phone,
company), the delivery address, the customer's notes (cut at 500 characters), and the
lines (quantity, name, variant, SKU) with the total. Test orders are marked as such.
`ticket80` lays it out for 80 mm receipt printers, as long as the content needs.
Texts come in the store's 10 languages (`[order_slip]` and `[shop_order]` in the
notification catalogues).

## Web orders in the staff order list

`GET /orders` (staff) accepts `web=true` for web orders only, or `web=false` for till
sales only. A back office can poll `?web=true&location_id=<shop>` to print new
orders as they arrive.

## Printing tips

- E-mail printers accept mail only from the senders they allow: add the store's
  sender address.
- A browser that prints without a dialog (Edge or Chrome started with
  `--kiosk-printing`) lets a back-office screen print each new order on its own.

Tables: `location_order_recipients`, and `inventory_locations.slip_format`,
`image_url`, `order_test_sent_at` (migration 054).
