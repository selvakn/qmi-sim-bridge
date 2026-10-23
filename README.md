# at-qmi-bridge

Presents a modem's AT port as a serial-like pty (e.g. `/dev/ttyUSB3`) and
answers `AT+CSIM` (raw SIM APDUs) over QMI UIM logical channels. It is for
modems whose firmware lacks `AT+CSIM` but whose QMI UIM service can still
send APDUs.

Built for the MSM8916 HiMI HMUF02-V5 4G stick on OpenWrt, so that
[gsm-sip-bridge](https://github.com/selvakn/gsm-sip-bridge)'s VoWiFi path
(EAP-AKA for the ePDG tunnel, IMS-AKA for registration) can use the stick's SIM.
On this modem (`HIMI_U01_MODEM_V1.0`), `AT+CSIM`, `AT+CCHO` and `AT+CGLA` all
return `+CME ERROR: 4`.

## What it does

Commands from the pty are forwarded to the real AT port (default
`/dev/wwan0at1`) unchanged, except for these:

| Command | Handling |
|---|---|
| `AT+CSIM=<n>,"<apdu>"` | Sent as QMI UIM *Send APDU* on a logical channel to the USIM. CLA is rewritten to that channel and `61xx` is followed with GET RESPONSE. |
| SELECT by AID of the USIM (`00A404..`) | The modem refuses application selection through raw APDUs (QMI error 82). The bridge reopens the logical channel, which selects the USIM, and answers `9000` (or FCP + `9000`). |
| `AT+QCFG="ims"` | Answers `+QCFG: "ims",2,0`: the modem has no IMS stack, so "disabled" is the truth. A set is acknowledged and ignored. |
| `AT+CFUN=<n>` | Not forwarded, because it would drop the radio and the LTE data link. The bridge resets the SIM session (closes and reopens the channel) and returns `OK`. |
| `AT+CGSN` | Sent as `AT+GSN`. The HIMI_U01 firmware answers `AT+CGSN` with the IMEI but never sends the final `OK`, so callers time out. |
| `ATE0` / `ATE1` | Forwarded, and tracked so that locally answered commands are echoed consistently. |

Modem responses and unsolicited result codes are copied back to the pty as-is.

QMI goes through libqmi's `qmi-proxy` (the abstract socket `qmi-proxy`), so it
coexists with ModemManager. If the proxy is not running, the bridge opens the
QMI device directly. One UIM client is held for the process lifetime, because a
logical channel belongs to the client that opened it. The modem rejects APDUs on
the basic channel (0).

At startup the USIM AID is discovered from `AT+CUAD` (EF_DIR). It can be
overridden with `--aid`.

## Usage

```
at-qmi-bridge [--at /dev/wwan0at1] [--qmi /dev/wwan0qmi0] [--link /dev/ttyUSB3]
              [--slot 1] [--aid <hex>] [--no-proxy]
```

The bridge logs to stderr. On SIGTERM it closes the channel, releases the QMI
client and removes the symlink. It exits if the AT port goes away (modem
restart); procd respawns it.

## Build (static arm64 musl)

```
cargo test --release
cargo build --release --target aarch64-unknown-linux-musl
```

The only dependency is `libc`. The release binary is about 450 KB, with about
400 KB RSS at runtime. Built natively in `rust:1-alpine` on an arm64 host; the
output is in `dist/aarch64-linux-musl/`.

## Install on OpenWrt

```
scp -O at-qmi-bridge                   root@stick:/usr/sbin/at-qmi-bridge
scp -O openwrt/at-qmi-bridge.init      root@stick:/etc/init.d/at-qmi-bridge
scp -O openwrt/at-qmi-bridge.config    root@stick:/etc/config/at-qmi-bridge
ssh root@stick 'chmod 755 /usr/sbin/at-qmi-bridge /etc/init.d/at-qmi-bridge &&
  /etc/init.d/at-qmi-bridge enable && /etc/init.d/at-qmi-bridge start'
logread -e at-qmi-bridge
```

Then point gsm-sip-bridge at it:

```toml
[[vowifi.line]]
modem_port = "/dev/ttyUSB3"
```

## Verified on hardware (HMUF02-V5, Vi India SIM)

Each step went through the pty: SELECT MF, then EF_DIR, then READ RECORD (the
USIM AID), then SELECT USIM by AID, then AUTHENTICATE (3G context). With dummy
RAND/AUTN, AUTHENTICATE returns `9862` (MAC failure), which is the card doing a
real AKA check. Also verified: EF_IMSI read, STATUS, `AT+QCFG="ims"`,
`AT+CFUN=0` interception, and passthrough of `AT+CIMI` and `AT+CSQ`.

## Deploying gsm-sip-bridge on the stick

`deploy/gsm-sip-bridge/` holds the compose app that lives in
`/apps/gsm-sip-bridge` on the stick. It runs VoWiFi only, in SIP server mode,
and phones register as `1001` on port 5060. `.env` (holding
`PHONE_1001_PASSWORD`) is not committed.

- gsm-sip-bridge discovers modems only by walking `/sys/bus/usb/devices` for
  Quectel IDs. `fake-usb/` is mounted over that path inside the container and
  describes one EC200 (AT-only, VoWiFi-only) whose AT port is `ttyUSB3`.
- `openwrt/podman-devfix.init` (S97): crun cannot create `/dev/shm` inside the
  bind-mounted host `/dev`, because OpenWrt makes it a symlink to `/tmp/shm`.
  This script turns it into a bind mount of `/tmp/shm` and creates
  `/dev/mqueue`.
- `openwrt/podman-apps.init` (S99): runs `docker-compose up -d` for each
  `/apps/*` at boot, through podman's Docker API socket. It first runs the
  app's executable `prestart`, if present. gsm-sip-bridge's `prestart` waits
  until `/dev/ttyUSB3` answers `AT+CPIN?` with `READY`, because gsm-sip-bridge
  probes its port only once at startup (5 s budget), and a miss disables
  VoWiFi for the whole run.
- Podman storage must live on the eMMC: OpenWrt's `/var` is tmpfs, so the
  default `graphroot` loses every image at reboot and holds them in RAM. Set
  `graphroot = "/overlay/containers/storage"` in
  `/etc/containers/storage.conf`. Use the ext4 `/overlay` mount directly, not a
  path under the overlayfs root.
- Firewall: two additions to fw4, below. The WAN (LTE) still rejects everything.

### Firewall (fw4)

```sh
# Each VoWiFi line runs its IMS agent in a netns, joined to the SIP agent in
# the host namespace by a veth pair (veth-sip<N> <-> veth-ims<N>, 10.99.0.0/30
# per line). Interfaces outside any zone fall to fw4's default input policy,
# REJECT on this box, so the agents' control channel and RTP hop between them
# need their own zone.
uci set firewall.gsmbridge=zone
uci set firewall.gsmbridge.name='gsmbridge'
uci add_list firewall.gsmbridge.device='veth-sip+'
uci set firewall.gsmbridge.input='ACCEPT'
uci set firewall.gsmbridge.output='ACCEPT'
uci set firewall.gsmbridge.forward='REJECT'

# Phones on the Wi-Fi LAN (zone "devices") reach SIP 5060 and RTP (dynamic
# ports). UDP only, IPv4, LAN zone only.
uci set firewall.devices_voip=rule
uci set firewall.devices_voip.name='Allow-SIP-RTP-devices'
uci set firewall.devices_voip.src='devices'
uci set firewall.devices_voip.proto='udp'
uci set firewall.devices_voip.family='ipv4'
uci set firewall.devices_voip.target='ACCEPT'

uci commit firewall && /etc/init.d/firewall reload
```

### Run

```
cd /apps/gsm-sip-bridge
DOCKER_HOST=unix:///run/podman/podman.sock docker-compose up -d
podman exec gsm-sip-bridge gsm-sip-bridge --config /etc/gsm-sip-bridge/config.toml vowifi-status
```
