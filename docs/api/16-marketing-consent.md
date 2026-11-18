# Marketing consent and the e-mail list

`customers.accepts_marketing` says whether a customer accepts commercial e-mails.
Every change is recorded in `marketing_consents` as proof (GDPR art. 7): yes or no,
where (`register`, `account`, `unsubscribe`), the version of the text shown, the IP
address and browser, and when. Recording a choice equal to the current one changes
and records nothing.

## Customer

- `POST /auth/register` accepts `accepts_marketing` (default `false`: the box starts
  unticked) and `marketing_wording` (the version of the text shown, up to 32
  characters).
- `PUT /customers/me` with `accepts_marketing` (and `marketing_wording`) records the
  change from the account. A "yes" needs `marketing_wording` (`400` without), at
  sign-up too; the storefront sends `<version>/<language>` (`2026-11/fr`). The IP
  address and browser are kept with each choice for as long as the account exists,
  and are not returned by the API (an access request reads them from the table).
- `GET /customers/me/marketing`: `{ "accepts_marketing": true, "history": [{ "granted",
  "source", "wording_version", "created_at" }] }`, newest first.

## Unsubscribe link (public)

Each exported row carries the customer's link,
`<storefront_url>/account/unsubscribe?token=<customer id>.<signature>`. The token is
an HMAC of the customer id, keyed with `security.unsubscribe_key` (else the JWT
secret: set the key so links already sent survive a JWT secret change). It does not
expire, and can only turn commercial e-mails off. Accepted trade-off: whoever holds
an exported list (or a forwarded e-mail) can unsubscribe the customers in it and see
the masked address (`a***@e***.com`). `security.unsubscribe_key`, when set, must be
at least 32 bytes and differ from the JWT secret.

- `GET /marketing/unsubscribe?token=`: `{ "email": "a***@example.com",
  "accepts_marketing": true }` (the address masked); `404` for a wrong token.
- `POST /marketing/unsubscribe` `{ "token": "…" }`: records "no" from `unsubscribe`;
  `404` for a wrong token.

If the store sends commercial e-mails itself later, add `List-Unsubscribe` and
`List-Unsubscribe-Post` (RFC 8058) with this link.

## Staff

- `GET /customers?marketing=true|false` filters the staff customer list.
- `GET /admin/customers/:id/marketing`: a customer's choice and history (`customers`
  read).
- `GET /admin/exports/marketing/count?audience=&lang=` (with `customers` read) and
  `GET /admin/exports/marketing.csv?audience=&lang=` (`exports` read for staff
  roles):
  - `audience`: `retail` (no company), `trade` (company members) or `all`
    (default); `lang`: a language prefix (`fr`, `nl-be`), the store's e-mail
    language counting for customers without one.
  - Only customers who accept commercial e-mails, whose consent is on record (a
    "yes" set before the record existed is left out) and who confirmed their
    address.
  - The CSV needs `customers` read too, and a proof of identity in the last 5
    minutes (`/auth/reauth`), else `403` with `code: "reauth_required"`. Each
    export is logged in `security_events` (`marketing_export`: audience, language,
    rows) before it is sent: if the log fails, nothing is sent.
  - Columns: `email`, `first_name`, `last_name`, `language`, `audience`, `company`,
    `consented_at`, `consent_source`, `unsubscribe_url`. UTF-8 with a byte order
    mark; `Cache-Control: no-store`.

Every CSV export of the API guards its cells against spreadsheet formulas: text
starting with `=`, `+`, `-`, `@`, a tab or a carriage return gets a leading `'`
and so does each part after a `;` or a tab, since spreadsheets set to `;` split
there (plain numbers such as `-12.30` stay numbers, and so do plain addresses such as
`-sales@example.com` in the list's e-mail column).

Table: `marketing_consents` (migration 050). The older `customers.marketing_opt_in`
and `customer_notification_preferences.marketing_emails` columns are not used.
