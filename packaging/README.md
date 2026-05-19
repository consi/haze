# Packaging notes

For end users, the published `.deb` / `.rpm` artefacts from the GitHub
release page are the recommended path - they install the binary, the
systemd unit, and a sample env file in one step. This document covers the
manual install for people building from source.

## Linux (systemd)

```bash
sudo install -m 755 target/release/haze /usr/bin/haze
sudo install -m 644 packaging/systemd/haze.service /lib/systemd/system/haze.service
sudo install -m 644 -D packaging/haze.env.example /etc/haze/haze.env.example
sudo systemctl daemon-reload
sudo systemctl enable --now haze
```

State lives in `/var/lib/haze`. Override env via `/etc/haze/haze.env`
(copy `/etc/haze/haze.env.example` as a starting point):

```ini
HAZE_BIND=0.0.0.0:4420
HAZE_LOG=haze=info
# HAZE_ORIGIN=https://haze.example.com   # required for WebAuthn passkeys
```

The unit declares `AmbientCapabilities=CAP_NET_RAW` so the binary can open
ICMP sockets without setcap or root.

## Bootstrap

First boot of the service creates an `admin` user with a randomly generated
password, printed to the service log:

```bash
sudo journalctl -u haze -f
# look for the line that contains "admin password"
```

Migrations are applied automatically on startup; no manual `migrate`
step is required.
