# garden

A [blossom](https://github.com/hzrd149/blossom) server where files stay alive as long as someone keeps paying for them.

```
cp garden.example.toml garden.toml   # the admin settings
cargo run --release -- garden.toml
```

## How it works

- **Accounts** are keyed by nostr pubkey and only exist once the first payment arrives. Asking for an account creates an *order*; each order gets its own Arkade address, and when the sats show up the account is created and credited.
- **Uploads** (`PUT /upload` with a blossom auth event, or the web UI at `/`) are stored in one of the configured providers (B2, S3, R2; all through the S3 API, picked by weight). The file gets its own account funded from the uploader's balance with enough for at least `pricing.min_days` (minimum 30) days.
- **Rent**: once a day every file pays `base_sats_per_day + ceil(MiB * sats_per_mib_per_day)`. When a file's balance hits zero its account is closed and the blob is deleted (deletions that fail are retried).
- **Serving**: `GET /<sha256>[.ext]` redirects (302) to the provider that holds the file, or 404 if it isn't alive.
- **Anyone can pay for any file**: `POST /api/topup {"files": {"<sha256>": sats, ...}}` needs no account or authorization. One order can pay for many files and the sats go straight to the file accounts when it is paid.
- **No owners.** Files have no owner and can't be deleted by users. [TigerBeetle](https://tigerbeetle.com) is the only record of accounts and files. How it is laid out is documented at the top of `src/ledger/mod.rs`; the short version is that ids are the first half of the pubkey/hash and `user_data_128` holds the second half, so everything is recoverable from the ledger, and an upload is a transfer `user -> file`, which is how `GET /admin/files/<sha256>` finds the uploader.
- **Moderation** (`Authorization: Bearer <admin_token>`): `POST /admin/ban {"pubkey": "...", "purge": true}` bans an account and removes everything it uploaded, `{"sha256": "..."}` removes and blocks one file, `GET /admin/bans`, `POST /admin/sweep` runs the daily rent now.

## Admin settings

See `garden.example.toml`. `[restrictions]` is where account/upload rules go (`Restrictions::check_account` and `check_upload` in `src/config.rs` enforce them, add new rules there).

## Running the pieces

- TigerBeetle: `tigerbeetle format --cluster=0 --replica=0 --replica-count=1 0_0.tigerbeetle && tigerbeetle start --addresses=3001 0_0.tigerbeetle`. The client is pinned to `0.17.9`, run the same server version.
- The test for the TigerBeetle ledger needs a running server: see `src/ledger/tigerbeetle.rs`.
- For development use `kind = "memory"` for the ledger and `kind = "mock"` for payments; orders are then paid with `POST /admin/orders/<id>/confirm`.

## Things to know

- Money received on Arkade stays in a wallet derived from `payments.seed` (one address per order). Back the seed up; to spend it, restore it in any Arkade wallet.
- An order credits exactly what it asked for. Overpayments are logged, not credited. If a file expires between the order being created and being paid, that part of the money is not credited (the order says `gone`).
- Uploading a file that is already alive is free and changes nothing.
- Uploads are held in memory while they are hashed (up to `max_file_size` each).
- If the server is down for a day, that day's rent is simply not charged.
- Not implemented from blossom: `GET /list`, `DELETE` (by design), `PUT /mirror`, `/media`.
