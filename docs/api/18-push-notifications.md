# Push notifications

The store's mobile apps (Android, iOS) register their device so the signed-in customer
gets notifications on the phone. Messages go through Firebase Cloud Messaging (HTTP
v1). FCM also reaches iOS apps, once the Firebase project holds the app's APNs key.

## Settings

```toml
[push]
provider = "fcm"                                     # "log": development, logged only
fcm_project_id = "my-store"
fcm_credentials_file = "/run/secrets/firebase.json"  # the service account key (JSON)
```

With no provider nothing is sent; devices can still register. A missing or unreadable
key file stops the server at start. The key signs a short-lived OAuth token (scope
`firebase.messaging`), which is kept until 5 minutes before it expires.

## Devices (customer, signed in)

- `POST /customers/me/push-devices` `{ "platform": "android" | "ios", "token": "<FCM
  registration token>" }`: `201` with `{ id, platform, created_at, last_seen_at }`.
  - Apps call it after sign-in and whenever FCM hands out a new token.
  - A token already registered moves to the customer who sends it, as when a phone
    changes hands or another account signs in on it.
  - Tokens are 1 to 4096 printable characters.
  - A customer keeps at most 10 devices; the least recently seen goes first.
- `DELETE /customers/me/push-devices` `{ "token": "…" }`: `204`, or `404` when the device
  is not the caller's. Apps call it before signing out.
- `GET /customers/me/push-devices`: `{ "enabled": true, "devices": [...] }`. Tokens are
  never returned. `enabled` says whether the store sends push at all.

Devices are forgotten when FCM reports them unregistered, and when every session of the
account ends ("sign out everywhere", a password change or reset, a reused refresh
token, an admin's reset of the second factors). This way a lost phone stops showing the
account's notifications. The apps register again after signing in.

## What is sent

- Cart reminders ([17-cart-reminders.md](17-cart-reminders.md)), together with their
  e-mail and only to customers who accept commercial messages. The notification gives
  the item count and the total, and `data.url` opens the cart.

Table: `push_devices` (migration 052).
