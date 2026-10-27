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
- **No owners.** Files have no owner and can't be deleted by users. [TigerBeetle](https://tigerbeetle.com) is the only record of accounts and files. How it is laid out is documented at the top of `src/ledger/mod.rs`; the short version is that ids are the first half of the pubkey/hash and `user_data_128` holds the second half, so everything is recoverable from the ledger, and an upload is a transfer `user -> file`, which is how the operator interface finds the uploader of a file (and the files of an uploader).
- **Moderation** is done in the operator interface (below): find who uploaded a file, ban an account and optionally remove everything it uploaded, remove and block one file.

## The operator interface

Everything the operator does against the server happens in a [hyperb](https://github.com/andunieee/hyperb) app, at `hyperb://<your server>/operator`. There is no admin api.

Put the public key of your hyperb identity in `operators` (in `garden.toml`). If you don't know it yet, open the address once: the server logs the key that tried and wasn't allowed, ready to be copied. Every request is signed by that key, and the interface doesn't exist while the list is empty.

It has four tabs:

- **Audit**: checks the books (below) over the last 7, 30 or 90 days and lists what looks wrong.
- **Books**: where you enter, if you want to, what happens outside the server: bills you paid to providers, sats you sold, the price of bitcoin and what each provider charges.
- **Moderation**: uploaders of a file, files of an account, bans. Destructive actions ask for confirmation.
- **Rent and orders**: take the day's rent now (it's idempotent), see orders waiting for their payment.

## The books and the audit

The ledger has two currencies in TigerBeetle: sats, and a fiat ledger (`[fiat]` in the settings) for the money you pay the providers with. Every sat that arrives is somewhere: in a user, in a file, earned as rent, sold, or *unclaimed* (paid and not used, such as an overpayment or a payment for a file that had already expired).

What you enter is optional. The audit uses what it has:

| you enter | the audit can tell you |
| --- | --- |
| nothing | the ledger adds up; the Arkade wallet holds what the ledger says it should |
| the sats you sold (and what you got) | what you have unsold and in fiat, and the average price you got |
| the price of bitcoin | what the unsold sats are worth, and the margin of each provider |
| bills paid to a provider (reference, amount, date) | what each provider cost in the period against what its files earned |
| what a provider charges per GB stored and downloaded | what the bills should have been, a warning if they are much bigger, and whether the files already here will pay for their own hosting until their sats run out |

Entries are keyed by their reference (an invoice number, a trade id): entering one twice does nothing, and entering it again with other numbers is refused.

Downloads are not metered exactly, since people are redirected to the provider: the audit counts the size of the file for every redirect, which is an upper bound on the egress.

## Admin settings

See `garden.example.toml`. `[restrictions]` is where account/upload rules go (`Restrictions::check_account` and `check_upload` in `src/config.rs` enforce them, add new rules there).

## Running the pieces

- TigerBeetle: `tigerbeetle format --cluster=0 --replica=0 --replica-count=1 0_0.tigerbeetle && tigerbeetle start --addresses=3001 0_0.tigerbeetle`. The client is pinned to `0.17.9`, run the same server version.
- The test for the TigerBeetle ledger needs a running server: see `src/ledger/tigerbeetle.rs`.
- For development use `kind = "memory"` for the ledger and `kind = "mock"` for payments; orders are then marked as paid in the operator interface.

## Things to know

- Money received on Arkade stays in a wallet derived from `payments.seed` (one address per order). Back the seed up; to spend it, restore it in any Arkade wallet.
- An order credits exactly what it asked for. Overpayments are not credited: they are recorded as unclaimed, and so is the part of an order for a file that expired before it was paid (the order says `gone`). The audit shows the total, and it is yours to keep or return.
- Uploading a file that is already alive is free and changes nothing.
- Money received on an address after its order was settled is not in the ledger; the audit shows it as a difference with the wallet.
- Selling sats means sending them out of the wallet and then telling the books; the audit flags a wallet that holds less than the ledger says.
- The operator interface is served over http only (no iroh yet).
- Uploads are held in memory while they are hashed (up to `max_file_size` each).
- If the server is down for a day, that day's rent is simply not charged.
- Not implemented from blossom: `GET /list`, `DELETE` (by design), `PUT /mirror`, `/media`.
