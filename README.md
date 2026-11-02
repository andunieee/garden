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
- **No owners.** Files have no owner and can't be deleted by users. The ledger is the only record of accounts and files. It is double-entry bookkeeping of our own, kept with [orangedb](https://github.com/andunieee/orangedb) in `ledger.redb`; the tables are in `src/ledger/schema.rs`. An upload is a transfer `user -> file`, which is how the operator interface finds the uploader of a file (and the files of an uploader).
- **Moderation** is done in the operator interface (below): find who uploaded a file, ban an account and optionally remove everything it uploaded, remove and block one file.

## The operator interface

Everything the operator does against the server happens in a [hyperb](https://github.com/andunieee/hyperb) app, at `hyperb://<your server>/operator`. There is no admin api.

The first time, with no `operators` in `garden.toml`, garden logs a setup link like `hyperb://cdn.example.com/operator#setup=<secret>` (on a VPS: `journalctl -u garden | grep setup=`). Open it in hyperb: the key of your hyperb identity becomes the operator and is added to `operators` in `garden.toml`. The secret only travels in the fragment, which hyperb sends in the first message of the session and never in a URL; it works once, changes every time the server starts, and goes away once there is an operator.

After that, add more keys to `operators` by hand (`--operator` in the installer does it too). A key that tries and isn't listed is written to the log, ready to be copied. Every request is signed by the key that opened the session.

It has six tabs:

- **Audit**: checks the books (below) over the last 7, 30 or 90 days and lists what looks wrong.
- **Books**: where you enter, if you want to, what happens outside the server: bills you paid to providers, sats you sold, the price of bitcoin and what each provider charges.
- **Moderation**: uploaders of a file, files of an account, bans. Destructive actions ask for confirmation.
- **Rent and orders**: take the day's rent now (it's idempotent), see orders waiting for their payment.
- **Payments**: pick the Arkade server (mainnet, mutinynet for test money, or another one) and create or restore the wallet, see what it holds, show its seed to back it up. Until this is done nobody can pay. The wallet can't be changed while orders are waiting for their payment, and if `[payments]` is in `garden.toml` the tab only shows it.
- **Wallet**: what the wallet holds and what the books say is available (earnings, unclaimed sats). Send sats out to an Ark address from any of those, or give unclaimed sats to an account without moving the wallet. Every send is recorded in the books as it happens, so the audit keeps matching; sending the same amount to the same address again needs a new note.

## The books and the audit

The ledger has two currencies: sats, and a fiat ledger (`[fiat]` in the settings) for the money you pay the providers with. Every sat that arrives is somewhere: in a user, in a file, earned as rent, sold, or *unclaimed* (paid and not used, such as an overpayment or a payment for a file that had already expired).

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

- The ledger needs nothing running: it is a file, `ledger.redb` in `data_dir` (or `path` in `[ledger]`).
- For development use `kind = "memory"` for the ledger and `kind = "mock"` for payments; orders are then marked as paid in the operator interface.
- To try it with real (test) payments, pick *Arkade, mutinynet* in the Payments tab. Any S3 server does for storage, e.g. SeaweedFS: `podman run -p 127.0.0.1:8333:8333 chrislusf/seaweedfs server -dir=/data -s3`, then a provider with `kind = "s3"`, `endpoint = "http://127.0.0.1:8333"` and `public_url = "http://127.0.0.1:8333/<bucket>"`.
- The test against mutinynet itself is ignored by default: `cargo test -- --ignored mutinynet`.

## Deploying on a VPS

Pushing a `v*` tag (`git tag v0.1.0 && git push origin v0.1.0`) runs
`.github/workflows/release.yml`, which builds
`garden-<target>.tar.gz` and publishes it with `install.sh` and checksums on the
GitHub release. The tarball contains the binary, the example config, the systemd
units, the Caddyfile example, and the installer.

Supported targets:

- `x86_64-unknown-linux-gnu`: Ubuntu 22.04+, Debian 12+, or anything with glibc 2.35+
- `aarch64-unknown-linux-gnu`: an arm64 system with glibc 2.39+ (the GitHub arm64 runner is Ubuntu 24.04)

### Install

Download the installer, look at it, then run it with your own public hostname
(the `cdn.example.com` below is only a placeholder):

```bash
curl -fsSLO https://github.com/andunieee/garden/releases/latest/download/install.sh
less install.sh
sudo bash install.sh --domain cdn.example.com --operator YOUR_HYPERB_PUBKEY_HEX
```

The installer:

- installs `garden` in `/opt/garden/bin`;
- creates `/etc/garden/garden.toml` (payments are then set up in the operator interface),
  `listen = "127.0.0.1:3000"`, and `data_dir = "/var/lib/garden/data"`;
- creates `/var/lib/garden`;
- installs the `garden` systemd unit.

It will not enable or start the services while `public_url` is still
`https://cdn.example.com` or the example provider credentials are still in the
config. Finish the config first:

```bash
sudoedit /etc/garden/garden.toml   # set public_url and the provider credentials
sudo systemctl enable --now garden.service
```

To deploy a config you already prepared, pass it in; the installer keeps a
timestamped backup of any config it replaces and starts the services:

```bash
sudo bash install.sh --config ./garden.toml
```

Other options: `--no-start` installs without starting the services, and
`--version v0.1.0` pins a release instead of using the latest one. The installer
is idempotent and keeps an existing `/etc/garden/garden.toml` and
`/var/lib/garden` on upgrades.

### TLS and the reverse proxy

garden speaks HTTP only. Put Caddy or nginx in front of it, terminate TLS there,
and forward to `127.0.0.1:3000`. The `public_url` in the config must be the exact
public URL, because the Blossom auth check uses its host.

An example Caddyfile is installed at `/etc/garden/Caddyfile.example`:

```caddyfile
cdn.example.com {
	request_body {
		max_size 110MB
	}
	reverse_proxy 127.0.0.1:3000
}
```

The body limit must be at least `restrictions.max_file_size`. With nginx, also
set `proxy_request_buffering off` and a long `proxy_read_timeout`, and raise
`client_max_body_size`; otherwise large uploads fail before they reach garden.

Open only ports 80 and 443. Do not expose 3000.

### What the VPS needs

- Outbound HTTPS to `arkade.computer` and to the S3/B2/R2 endpoints.
- A working clock (NTP); rent is charged per Unix day.
- A little disk for `garden.redb` and `ledger.redb`, which grow with the number
  of accounts, files and transfers. Keep `/var/lib/garden` on a local
  filesystem, not on NFS.
- Enough RAM for in-flight uploads: each upload is buffered up to
  `restrictions.max_file_size` while it is hashed, so 100 MiB uploads need
  headroom beyond the base process.
- Provider buckets whose `public_url` is publicly readable, because serving a
  blob is a redirect to the provider.

### Backups and upgrades

Back up these together:

- `/etc/garden/garden.toml` (provider keys, and the Arkade seed if `[payments]` is there)
- `/var/lib/garden/data/garden.redb` (orders, bans, pending deletions)
- `/var/lib/garden/data/ledger.redb` (accounts, files and every transfer)

To upgrade, run the installer again from the new release. It keeps the config
and data, replaces the garden binary, and restarts garden. Never delete
`ledger.redb`: it is the only record of who has paid for what.

## Things to know

- Money received on Arkade stays in a wallet derived from its seed (one address per order). Back the seed up (the Payments tab shows it); the Wallet tab can send it out, and the seed restores it in any Arkade wallet on the same network. The wallet's contracts are kept in `garden.redb`, so a restart still knows the addresses it handed out.
- An order credits exactly what it asked for. Overpayments are not credited: they are recorded as unclaimed, and so is the part of an order for a file that expired before it was paid (the order says `gone`). The audit shows the total; the Wallet tab can send it back or give it to an account.
- Uploading a file that is already alive is free and changes nothing.
- Money received on an address after its order was settled is not in the ledger; the audit shows it as a difference with the wallet.
- Selling sats means sending them out of the wallet and then telling the books. The Wallet tab does both in one step (it takes them from earnings); if you send from somewhere else, the audit flags the difference until it is recorded.
- The operator interface is served over http only (no iroh yet).
- Uploads are held in memory while they are hashed (up to `max_file_size` each).
- If the server is down for a day, that day's rent is simply not charged.
- Not implemented from blossom: `GET /list`, `DELETE` (by design), `PUT /mirror`, `/media`.
