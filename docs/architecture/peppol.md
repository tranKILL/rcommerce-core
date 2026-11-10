# Invoices and Peppol e-invoicing

## Invoices

With `[invoicing] enabled = true`, an order gets its legal invoice when its
goods are handed over: collected at the counter
(`POST /orders/:id/pickup/collected`) or delivered by the store
(`POST /orders/:id/delivery/delivered`). Staff can issue it again with
`POST /admin/orders/:id/invoice` (completed orders only; a second issue
returns the same invoice).

- **Numbers** are `{number_prefix}{year}-{sequence:05}` (`F2026-00001`). The
  sequence restarts each year and has no gaps: it is taken from a counter row
  locked inside the issuing transaction.
- **Snapshots.** The invoice keeps the seller (`[invoicing.seller]`), the
  buyer, the lines and the VAT per rate. The buyer is the customer's company
  account (legal name, VAT and company numbers, Peppol id) with the order's
  billing address, else the customer. Later changes to the order, the
  customer or the store do not change an issued invoice.
- **Lines** are the order items, the delivery charge and the discount (spread
  over the VAT rates in proportion). Each rate's VAT is computed on the rate's
  taxable total and rounded to cents (EN 16931 BR-CO-17). A zero rate to a
  company in another country is an intra-EU reverse charge.
- **Payment.** An order paid online is settled (`paid_amount`); otherwise the
  invoice is due after `payment_terms_days`, with a Belgian structured
  communication (`+++123/4567/89012+++`, modulo-97 check digits).
- **Delivery.** The buyer receives the invoice by e-mail, in the order's
  language, with its PDF. Invoices to companies also go through Peppol when it
  is enabled.

| Route | Who | What |
|---|---|---|
| `GET /invoices?order_id=` | customer | Their invoices and their company's |
| `GET /invoices/:id` | customer, staff | One invoice |
| `GET /invoices/:id/pdf` | customer, staff | The PDF |
| `GET /invoices/:id/ubl` | customer, staff | The Peppol BIS Billing 3.0 UBL document |
| `GET /admin/invoices?q=&peppol_status=&company_id=` | staff (`orders:read`) | Newest first, paged |
| `POST /admin/invoices/:id/peppol` | staff (`orders:write`) | Send through Peppol now (again) |
| `POST /admin/orders/:id/invoice` | staff (`orders:write`) | Issue a completed order's invoice |

## Peppol

With `[peppol] enabled = true`, invoices to companies (a VAT number on the
buyer) wait with `peppol_status = pending` and are sent at once in the
background; a loop (every `poll_minutes`, one store instance at a time) sends
what is still pending and follows what is on its way.

1. The invoice is checked: the seller's VAT number, address and IBAN (when an
   amount is due), the buyer's name and country, the lines adding up.
2. The buyer's participant id is its company's `peppol_id`, else (Belgium)
   `0208:` and its company number. The access point is asked whether it
   receives invoices; if not, the invoice is `not_on_peppol` (the e-mail is
   its delivery).
3. The UBL document (with the PDF embedded) is sent: `sent`, then `delivered`
   or `failed` with the access point's reason.

The store's participant id is `peppol.sender_id`, else `0208:` and
`invoicing.seller.company_number`. Secrets may be given as `env:NAME`.

### Access points

Each provider is one adapter behind the `AccessPoint` trait (look up a
participant, send a document, read its status). Peppyrus is the default.

| `provider` | Service | Credentials | State |
|---|---|---|---|
| `peppyrus` | Peppyrus (BE, free) | `api_key` | Tested end to end against a stand-in of its API; test service by default (`production = true` for the real one) |
| `scrada` | Scrada (BE) | `api_key`, `api_secret` (password), `account_id` (company id) | Written from its API, not yet tried on an account |
| `einvoicebe` | e-invoice.be (BE) | `api_key` | Written from its API, not yet tried on an account |
| `falco` | Falco (BE) | `api_key`, `api_secret` (app secret) | Written from its API, not yet tried on an account; status reading assumed |
| `acube` | A-Cube | `api_key` | Written from its API, not yet tried; no participant lookup (the send reports it) |
| `b2brouter` | B2BRouter | `api_key`, `account_id` | Written from its API, not yet tried on an account |
| `iopole` | Iopole (FR) | `client_id`, `api_secret` (OAuth) | Written from its API, not yet tried; no participant lookup |
| `superpdp` | SUPER PDP (FR) | `client_id`, `api_secret` (OAuth) | Written from its API, not yet tried; no participant lookup |
| `simulated` | none | none | Every participant receives, every document is delivered (development) |

Not offered: **Billit** (its sending API takes its own order format rather
than a UBL document), **Codabox** (oriented to receiving documents for
accountants) and **Qonto** (no sending API used).

Receiving supplier invoices through Peppol is not part of a webshop and is not
covered.

### Configuration

```toml
[invoicing]
enabled = true
number_prefix = "F"
payment_terms_days = 30
timezone = "Europe/Brussels"

[invoicing.seller]
name = "Store SA"
vat_number = "BE0123456749"
company_number = "0123456749"
street = "Rue du Commerce 1"
city = "Namur"
postcode = "5000"
country = "BE"
iban = "BE68539007547034"
bic = "GKCCBEBB"
email = "invoices@store.example"

[peppol]
enabled = true
provider = "peppyrus"
production = false            # the provider's test service
api_key = "env:PEPPOL_API_KEY"
# sender_id = "0208:0123456749"
poll_minutes = 15
```
