# at-qmi-bridge

Gives software access to a modem's SIM over QMI UIM logical channels, for
modems whose firmware lacks `AT+CSIM` but whose QMI UIM service can still send
APDUs. It has two front ends, chosen at startup:

| Mode | Selected by | Presents the SIM as |
|---|---|---|
| **AT mode** (original) | default | a serial-like pty (e.g. `/dev/ttyUSB3`) that answers `AT+CSIM` |
| **PC/SC mode** | `--vpcd <host:port>` | a virtual smart card behind pcscd's vpcd reader |

Both share the same QMI SIM session code (logical channel handling, `61xx`
chaining, reconnecting to qmi-proxy).

Built for the MSM8916 HiMI HMUF02-V5 4G stick on OpenWrt, so that
[gsm-sip-bridge](https://github.com/selvakn/gsm-sip-bridge)'s VoWiFi path
(EAP-AKA for the ePDG tunnel, IMS-AKA for registration) can use the stick's SIM.
On this modem (`HIMI_U01_MODEM_V1.0`), `AT+CSIM`, `AT+CCHO` and `AT+CGLA` all
return `+CME ERROR: 4`.

PC/SC mode is the recommended one: it needs no AT port, no fake Quectel USB
tree and none of the AT emulation below. See the
[runbook](docs/native-gsm-sip-bridge-runbook.md).

## AT mode

Commands from the pty are forwarded to the real AT port (default
`/dev/wwan0at1`) unchanged, except for these:

| Command | Handling |
|---|---|
| `AT+CSIM=<n>,"<apdu>"` | Sent as QMI UIM *Send APDU* on a logical channel to the USIM. CLA is rewritten to that channel and `61xx` is followed with GET RESPONSE. |
| SELECT by AID of the USIM (`00A404..`) | The modem refuses application selection through raw APDUs (QMI error 82). The first one opens the logical channel, which selects the USIM, and is answered `9000` (or FCP + `9000`). Later ones are sent as SELECT `7FFF` (the current-ADF alias) on the open channel. Reopening the channel makes the modem announce SIM changes. |
| `AT+QCFG="ims"` | Answers `+QCFG: "ims",2,0`: the modem has no IMS stack, so "disabled" is the truth. A set is acknowledged and ignored. |
| `AT+CFUN=<n>` | Not forwarded, because it would drop the radio and the LTE data link. Answered `OK`. |
| `AT+CGSN` | Sent as `AT+GSN`. The HIMI_U01 firmware answers `AT+CGSN` with the IMEI but never sends the final `OK`, so callers time out. |
| `ATE0` / `ATE1` | Forwarded, and tracked so that locally answered commands are echoed consistently. |

Modem responses and unsolicited result codes are copied back to the pty as-is.

## PC/SC mode

`--vpcd 127.0.0.1:15963` makes at-qmi-bridge the *card* in a
[vsmartcard](https://frankmorgner.github.io/vsmartcard/) virtual reader. pcscd
loads `libifdvpcd` and listens on a TCP port; at-qmi-bridge connects to it and
answers as the card. PC/SC clients (strongSwan's `eap-sim-pcsc`,
gsm-sip-bridge's PC/SC transport) then talk to the SIM like any other reader.
No AT port and no pty are used (`--at` and `--link` are ignored).

- **Framing.** Each message is a 2-byte big-endian length plus payload. A
  1-byte payload is a control message (`00` power off, `01` power on, `02`
  reset, `04` get ATR); anything longer is a command APDU, answered with the
  response APDU. The ATR is the minimal `3B 00`. APDUs sent while the card is
  powered off get `6F00`.
- **T=0 presentation.** Clients expect classic T=0, but QMI returns data and
  status word together. For SELECT (`A4`) and INTERNAL AUTHENTICATE (`88`)
  answers that carry data and end in `9000`, the bridge replies `61xx` and
  serves the cached data on the following GET RESPONSE (`C0`), which never
  reaches the SIM. Everything else passes through unchanged.
- **USIM channel.** The channel is opened by the USIM AID, which is the 7-byte
  prefix `A0000000871002` unless `--aid` is given (`AT+CUAD` discovery needs
  the AT port, so it isn't used here). SELECT-by-AID handling and the
  `SELECT 7FFF` re-select trick are the same as in AT mode.
- **Reconnecting.** pcscd is started and restarted by gsm-sip-bridge's
  `supervise`, so the bridge retries the TCP connection every 300 ms until the
  listener appears, and again 1 s after a connection drops.
- **Reader config.** pcscd needs a vpcd reader with a `FRIENDLYNAME` other
  than `Virtual PCD`, because gsm-sip-bridge skips readers with that name.
  `openwrt/qmisim.reader.conf` is the reference (`QMI SIM`, port `0x3E5B` =
  15963); `deploy/native/gsm-sip-bridge.init` writes it automatically when the
  config has `pcsc_reader = true`.
- **gsm-sip-bridge config.** `pcsc_reader = true` plus `imsi_override` (and
  `mcc`/`mnc`, `imei_override`) in the `[[vowifi.line]]`; see
  `deploy/native/config.pcsc.toml`.

## QMI

QMI goes through libqmi's `qmi-proxy` (the abstract socket `qmi-proxy`), so it
coexists with ModemManager (in AT mode, the udev rule
`openwrt/77-mm-at-qmi-bridge.rules` also keeps ModemManager off `wwan0at1`). If the proxy is not running, the bridge opens the
QMI device directly. One UIM client is held for the process lifetime, because a
logical channel belongs to the client that opened it. The modem rejects APDUs on
the basic channel (0).

In AT mode the USIM AID is discovered at startup from `AT+CUAD` (EF_DIR). It can
be overridden with `--aid` in both modes.

## Usage

```
at-qmi-bridge [--at /dev/wwan0at1] [--qmi /dev/wwan0qmi0] [--link /dev/ttyUSB3]
              [--slot 1] [--aid <hex>] [--no-proxy] [--proxy-wait 120]
              [--trace] [--vpcd <host:port>]
```

`--proxy-wait` is how many seconds to wait for `qmi-proxy` before opening the
QMI device directly. `--trace` logs every AT line (AT mode) or vpcd message
(PC/SC mode). `--vpcd` switches to PC/SC mode.

The bridge logs to stderr. On SIGTERM it closes the channel, releases the QMI
client and removes the symlink. In AT mode it exits if the AT port goes away (modem
restart); procd respawns it.

## Build (static arm64 musl)

```
make test          # unit tests
make arm64         # cargo build --release --target aarch64-unknown-linux-musl -> dist/
make arm64-docker  # same, inside rust:1-alpine on an arm64 host
make deploy STICK=root@stick
make help          # all targets
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

Options live in `/etc/config/at-qmi-bridge` (`enabled`, `at`, `qmi`, `link`,
`slot`, `aid`, `trace`, `vpcd`). Setting `vpcd` selects PC/SC mode.

AT mode: also install `openwrt/77-mm-at-qmi-bridge.rules` as
`/lib/udev/rules.d/77-mm-at-qmi-bridge.rules` and restart ModemManager, then
point gsm-sip-bridge at the pty:

```toml
[[vowifi.line]]
modem_port = "/dev/ttyUSB3"
```

PC/SC mode: set the option and restart; the reader entry and the gsm-sip-bridge
line are covered by the runbook.

```sh
uci set at-qmi-bridge.main.vpcd=127.0.0.1:15963
uci commit at-qmi-bridge && /etc/init.d/at-qmi-bridge restart
```

```toml
[[vowifi.line]]
pcsc_reader = true
mcc = "404"
mnc = "043"
imsi_override = "<IMSI>"
```

## Verified on hardware (HMUF02-V5, Vi India SIM)

AT mode: each step went through the pty: SELECT MF, then EF_DIR, then READ RECORD (the
USIM AID), then SELECT USIM by AID, then AUTHENTICATE (3G context). With dummy
RAND/AUTN, AUTHENTICATE returns `9862` (MAC failure), which is the card doing a
real AKA check. Also verified: EF_IMSI read, STATUS, `AT+QCFG="ims"`,
`AT+CFUN=0` interception, and passthrough of `AT+CIMI` and `AT+CSQ`.

PC/SC mode: `gsm-sip-bridge pcsc-list` shows the `QMI SIM` reader with the
card's IMSI, the ePDG tunnel comes up, and IMS REGISTER returns `200 OK` within
about 10 s of the service starting.

## Deploying gsm-sip-bridge on the stick (Podman, AT mode)

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

## Running gsm-sip-bridge natively (no podman; AT or PC/SC mode)

The full step-by-step setup, including PC/SC mode, is in [docs/native-gsm-sip-bridge-runbook.md](docs/native-gsm-sip-bridge-runbook.md). Summary:

Stopping the gsm-sip-bridge container repeatedly crashed the stick's kernel
(see the kernel-issue draft), so `deploy/native/` runs it as plain processes
from the image's unpacked root filesystem instead:

```sh
C=$(podman create ghcr.io/selvakn/gsm-sip-bridge:8.22.0)
podman export $C | tar -x -C /overlay/gsm-sip-bridge-root && podman rm $C
scp -O deploy/native/gsm-sip-bridge.init root@stick:/etc/init.d/gsm-sip-bridge
scp -O deploy/native/run-native         root@stick:/apps/gsm-sip-bridge/run-native
scp -O deploy/native/config.pcsc.toml   root@stick:/apps/gsm-sip-bridge/config.toml   # PC/SC mode; config.toml for AT mode
/etc/init.d/podman-apps disable; /etc/init.d/gsm-sip-bridge enable; /etc/init.d/gsm-sip-bridge start
logread -e run-native
```

What the init script does on start (all idempotent):

- Symlinks the absolute paths the binary hard-codes into the unpacked tree:
  `/usr/libexec/ipsec`, `/usr/lib/ipsec`, `/usr/lib/pcsc`, `swanctl`, `pcscd`, `/etc/strongswan.d`,
  `/etc/strongswan.conf`, `/etc/swanctl`, `/etc/reader.conf.d`, `/etc/gsm-sip-bridge` and
  `/usr/local/bin/gsm-sip-bridge`. None of these exist on OpenWrt.
- Rewrites the image's absolute symlinks to point inside the tree. Otherwise
  `timeout -> /bin/busybox` resolves to the host busybox, which has no `timeout`
  applet, and supervise misreads that as `swanctl --list-sas ... vici connection
  broken`, re-initiating the tunnel in a loop.
- Patches `ims.updown`. charon runs it without our `PATH`, so its
  `ip netns exec … ip addr add` failed silently. The tunnel address never
  appeared, and IMS REGISTER got "Host is unreachable". The script now pins the
  image's userland and logs `ip` errors (`logread -e ims.updown`).
- Adds `/usr/bin/ip -> /sbin/ip` for `ip netns exec` run with an empty `PATH`.
- Bind-mounts `fake-usb/` read-only over `/sys/bus/usb/devices`. It's empty on the
  stick, because the stick is a USB gadget and not a host. Only AT mode needs it (for
  modem discovery), but the script does it in both modes.
- PC/SC mode (config has `pcsc_reader = true`): points pcscd's `reader.conf.d/vpcd`
  at `/dev/null` and writes the `QMI SIM` reader entry (port 15963).

`run-native` runs `prestart`, loads `.env`, puts the unpacked tree first on
`PATH` and `LD_LIBRARY_PATH` (both sides use musl 1.2.5), and execs
`gsm-sip-bridge supervise`. Status:

```sh
set -a; . /apps/gsm-sip-bridge/.env; set +a
LD_LIBRARY_PATH=/overlay/gsm-sip-bridge-root/usr/lib /overlay/gsm-sip-bridge-root/usr/local/bin/gsm-sip-bridge \
  --config /apps/gsm-sip-bridge/config.toml vowifi-status
```
