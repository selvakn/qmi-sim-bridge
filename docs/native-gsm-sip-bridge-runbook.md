# Runbook: gsm-sip-bridge running natively on the OpenWrt 4G stick

This runbook covers running [gsm-sip-bridge](https://github.com/selvakn/gsm-sip-bridge) as plain processes on an
MSM8916 HiMI / HMUF02-V5 4G stick running OpenWrt, with no Docker or Podman. The stick's own SIM becomes a VoWiFi
line, and the bridge acts as the SIP server for phones on the LAN.

It assumes the stick already runs akbar-npj/msm8916-openwrt (25.12.5-r6, target `generic-hmuf02v5`) with LTE, the
Wi-Fi client uplink and SSH set up. See the stick's OpenWrt runbook for that.

## Why native, not a container

Under Podman, every stop or recreate of the gsm-sip-bridge container with a live line crashed the kernel: a
`list_del corruption` in `exit_mmap`, then oopses in unrelated tasks, or a hang ended by the hardware watchdog.
That is a kernel memory-corruption bug (a 2-byte `86 dd` write into freed memory), not a Podman or gsm-sip-bridge bug.
Container teardown just reliably exposed it. Run natively, the same workload has stopped and started cleanly.

## Two ways to give gsm-sip-bridge the SIM

| | **PC/SC mode (recommended)** | AT mode (original) |
|---|---|---|
| qmi-sim-bridge | `--vpcd 127.0.0.1:15963`: the virtual card behind pcscd's vpcd reader, renamed `QMI SIM` | pty at `/dev/ttyUSB3` pretending to be a Quectel AT port |
| gsm-sip-bridge line | `pcsc_reader = true` + `imsi_override` (+ `mcc`/`mnc`, `imei_override`) | `modem_port = "/dev/ttyUSB3"` |
| AT port use | none (QMI only) | `wwan0at1`, shared with ModemManager |
| Workarounds needed | reader rename | fake-usb sysfs, `AT+CGSN`/`QCFG`/`CFUN` emulation, `prestart` SIM wait, ModemManager port-ignore rule |
| Modem SMS sweep | not run (reader lines have no modem) | runs; times out now and then |

PC/SC mode removes every AT-related failure seen during bring-up. Both modes share the QMI SIM session
code: a persistent logical channel, `SELECT 7FFF` for re-selecting the USIM, and reconnecting after a qmi-proxy
restart. AT mode is still documented below.

### PC/SC mode setup

1. qmi-sim-bridge in PC/SC mode:
   `uci set qmi-sim-bridge.main.vpcd=127.0.0.1:15963; uci commit qmi-sim-bridge; /etc/init.d/qmi-sim-bridge restart`.
   It retries the connection every 300 ms until pcscd (started by supervise) listens.
2. `config.toml` from `deploy/native/config.pcsc.toml`, with the real IMSI (`mmcli -i any`) and IMEI
   (`qmicli -p -d /dev/wwan0qmi0 --dms-get-ids`) filled in on the stick only:
   ```toml
   [[vowifi.line]]
   pcsc_reader = true
   mcc = "404"
   mnc = "043"                   # zero-padded: "43" builds epdg.epc.mnc43…, which does not resolve
   imsi_override = "<IMSI>"
   imei_override = "<IMEI>"
   ```
   Pinning `mcc`/`mnc` avoids supervise reading them from the card the instant it starts pcscd, before
   qmi-sim-bridge has connected ("FATAL: could not derive MCC/MNC; skipping this line").
3. The init script's `setup_pcsc_reader` runs when the config has `pcsc_reader = true`:
   - points `$ROOT/etc/reader.conf.d/vpcd` (rewritten by supervise at every start as "Virtual PCD") at `/dev/null`;
   - writes `$ROOT/etc/reader.conf.d/qmisim`: one vpcd reader named `QMI SIM` on port 15963 (`0x3E5B`).

   gsm-sip-bridge's PC/SC transport skips readers named `Virtual PCD`, hence the rename. A *second* vpcd reader
   entry next to supervise's doesn't work: pcscd shows its slots, but never polls the card ("no card"). Keep no other
   files in `reader.conf.d` that define a reader on 15963, because pcscd reads every file there.

Verify:

```sh
LD_LIBRARY_PATH=$ROOT/usr/lib:$ROOT/lib $ROOT/usr/local/bin/gsm-sip-bridge --config $APP/config.toml pcsc-list
#   QMI SIM 00 00   ok   <imsi>   404   043   <carrier>
logread -e qmi-sim-bridge     # vpcd: connected to 127.0.0.1:15963
```

On Vi India the tunnel comes up and REGISTER returns `200 OK` within about 10 s of the service starting.

## Architecture (AT mode)

```
phones (Wi-Fi LAN / VPN) --SIP 5060/udp--> gsm-sip-bridge supervise   (host netns)
                                            ├─ vowifi-sip-agent        (SIP server, host netns)
                                            ├─ vowifi-ims-agent        (netns ims0, IMS REGISTER over the tunnel)
                                            ├─ vowifi-usim-bridge      (vpcd <-> AT+CSIM)
                                            ├─ pcscd + ifd-vpcd        (virtual reader for charon's eap-sim-pcsc)
                                            └─ charon (strongSwan)     (IKEv2 to the ePDG over LTE, xfrm tun23-0 in ims0)
                                                   │
                             /dev/ttyUSB3  (pty) ──┘
                                   │
                            qmi-sim-bridge  ── AT ──> /dev/wwan0at1
                                           ── QMI UIM (via qmi-proxy) ──> modem SIM
```

The modem (`HIMI_U01_MODEM_V1.0`) has no `AT+CSIM`, so qmi-sim-bridge (this repo) provides one over QMI UIM logical
channels. See the [README](../README.md) for what it answers itself.

## Files on the stick

| Path | What | Source in this repo |
|---|---|---|
| `/usr/sbin/qmi-sim-bridge` | AT/QMI bridge binary (static arm64 musl) | `dist/aarch64-linux-musl/qmi-sim-bridge` |
| `/etc/init.d/qmi-sim-bridge`, `/etc/config/qmi-sim-bridge` | its procd service (S96) and uci options | `openwrt/` |
| `/overlay/gsm-sip-bridge-root/` | the gsm-sip-bridge image's root filesystem, unpacked (~40 MB) | from `ghcr.io/selvakn/gsm-sip-bridge:<tag>` |
| `/etc/init.d/gsm-sip-bridge` | native procd service (S99) | `deploy/native/gsm-sip-bridge.init` |
| `/apps/gsm-sip-bridge/run-native` | launcher | `deploy/native/run-native` |
| `/apps/gsm-sip-bridge/config.toml` | config (`db_path` under `/apps`) | `deploy/native/config.pcsc.toml` (PC/SC) or `deploy/native/config.toml` (AT) |
| `$ROOT/etc/reader.conf.d/qmisim` | PC/SC mode: the `QMI SIM` vpcd reader (written by the init script) | `openwrt/qmisim.reader.conf` |
| `/apps/gsm-sip-bridge/.env` | `PHONE_1001_PASSWORD=…` (mode 600, never committed) | — |
| `/apps/gsm-sip-bridge/prestart` | waits for SIM READY + IMSI on `/dev/ttyUSB3` | `deploy/gsm-sip-bridge/prestart` |
| `/apps/gsm-sip-bridge/fake-usb/` | fake USB sysfs entry (EC200 whose AT port is `ttyUSB3`) | `deploy/gsm-sip-bridge/fake-usb/` |

## 1. Prerequisites on the stick

```sh
# IPsec/xfrm for the ePDG tunnel (kernel modules from the official 25.12.5 feed)
apk add kmod-ipsec kmod-ipsec4 kmod-ipsec6 kmod-xfrm-interface kmod-crypto-gcm
# Only needed once, to pull and unpack the image (podman can stay disabled afterwards)
apk add podman
```

The image needs about 41 MB to pull and 40 MB unpacked, both on the eMMC overlay. OpenWrt's `/var` is tmpfs, so if
Podman is used at all, set `graphroot = "/overlay/containers/storage"` in `/etc/containers/storage.conf` first.

## 2. Firewall (fw4)

```sh
# Per-line veth pair between the SIP agent (host netns) and the IMS agent (netns ims0).
# fw4's default input policy is REJECT on this box, and that applies to any interface outside a zone.
uci set firewall.gsmbridge=zone
uci set firewall.gsmbridge.name='gsmbridge'
uci add_list firewall.gsmbridge.device='veth-sip+'
uci set firewall.gsmbridge.input='ACCEPT'
uci set firewall.gsmbridge.output='ACCEPT'
uci set firewall.gsmbridge.forward='REJECT'

# Phones on the Wi-Fi LAN zone ("devices"): SIP 5060 + dynamic RTP, UDP only. LTE (wan) stays closed.
uci set firewall.devices_voip=rule
uci set firewall.devices_voip.name='Allow-SIP-RTP-devices'
uci set firewall.devices_voip.src='devices'
uci set firewall.devices_voip.proto='udp'
uci set firewall.devices_voip.family='ipv4'
uci set firewall.devices_voip.target='ACCEPT'

uci commit firewall && /etc/init.d/firewall reload
```

## 3. Install qmi-sim-bridge

Build it (`cargo build --release --target aarch64-unknown-linux-musl`, or natively in `rust:1-alpine` on an arm64
host), then:

```sh
scp -O dist/aarch64-linux-musl/qmi-sim-bridge root@stick:/usr/sbin/qmi-sim-bridge
scp -O openwrt/qmi-sim-bridge.init            root@stick:/etc/init.d/qmi-sim-bridge
scp -O openwrt/qmi-sim-bridge.config          root@stick:/etc/config/qmi-sim-bridge
ssh root@stick 'chmod 755 /usr/sbin/qmi-sim-bridge /etc/init.d/qmi-sim-bridge
  /etc/init.d/qmi-sim-bridge enable && /etc/init.d/qmi-sim-bridge start
  logread -e qmi-sim-bridge | tail -3'
```

Expected log: `QMI UIM client N on /dev/wwan0qmi0` and `serving /dev/ttyUSB3 -> /dev/pts/0`. At boot it waits up to
120 s for ModemManager's `qmi-proxy`.

To replace a running binary, copy it to `qmi-sim-bridge.new`, then `mv` it over (`scp` onto a running binary fails
with "Text file busy"), then restart the service.

## 4. Unpack the gsm-sip-bridge image

```sh
ssh root@stick 'R=/overlay/gsm-sip-bridge-root; mkdir -p $R
  C=$(podman create ghcr.io/selvakn/gsm-sip-bridge:8.22.0)
  podman export $C | tar -x -C $R && podman rm $C
  cat $R/etc/alpine-release'
```

Alpine's musl is 1.2.5, the same as OpenWrt 25.12's, so the image's binaries run against the host loader. Its own
libraries are loaded from the tree via `LD_LIBRARY_PATH`.

## 5. Install the app and the service

```sh
ssh root@stick 'mkdir -p /apps/gsm-sip-bridge/data'
scp -O -r deploy/gsm-sip-bridge/fake-usb      root@stick:/apps/gsm-sip-bridge/
scp -O deploy/gsm-sip-bridge/prestart         root@stick:/apps/gsm-sip-bridge/prestart
scp -O deploy/native/run-native               root@stick:/apps/gsm-sip-bridge/run-native
scp -O deploy/native/config.toml              root@stick:/apps/gsm-sip-bridge/config.toml
scp -O deploy/native/gsm-sip-bridge.init      root@stick:/etc/init.d/gsm-sip-bridge
ssh root@stick 'cd /apps/gsm-sip-bridge
  printf "PHONE_1001_PASSWORD=%s\n" "$(head -c 18 /dev/urandom | base64 | tr -dc A-Za-z0-9 | head -c 20)" > .env
  chmod 600 .env; chmod 755 prestart run-native /etc/init.d/gsm-sip-bridge
  /etc/init.d/gsm-sip-bridge enable && /etc/init.d/gsm-sip-bridge start'
```

`config.toml` (SIP server mode, VoWiFi only):

```toml
[sip]
local_port = 5062                  # 5060 is left for the phones

[sip_server]
enabled = true
ring_aor = "1001"

[[sip_server.account]]
username = "1001"
password = "env:PHONE_1001_PASSWORD"

[cs]
enabled = false                    # no Quectel audio path; every call is VoWiFi

[vowifi]
enabled = true

[[vowifi.line]]
modem_port = "/dev/ttyUSB3"        # qmi-sim-bridge's pty

[sms]
enabled = true
db_path = "/apps/gsm-sip-bridge/data/store.db"
```

### What the init script does on every start (all idempotent)

1. **Repoints the image's absolute symlinks into the tree.** For example, `timeout -> /bin/busybox`, outside a
   chroot, resolves to the host's busybox, which has no `timeout` applet. supervise runs
   `timeout … swanctl --list-sas`, reads "applet not found" as `vici connection broken`, and re-initiates the tunnel
   in a loop. The carrier answers `NO_ADDITIONAL_SAS`. The script marks the tree done with `$ROOT/.relinked`.
2. **Patches `$ROOT/etc/strongswan.d/ims.updown`.** charon runs it without our `PATH`/`LD_LIBRARY_PATH`, and its
   `ip netns exec ims0 ip addr add <inner-ip> dev tun23-0` failed silently (`2>/dev/null || true`). The tunnel came
   up, but the inner IPv6 address was never installed, so IMS REGISTER failed with `Host is unreachable`. The patch
   pins the image's userland at the top of the script and pipes `ip` errors to `logger -t ims.updown`. The original
   is kept as `ims.updown.orig`.
3. **Symlinks the absolute paths the binary and its helpers hard-code**, none of which exist on OpenWrt:
   `/usr/libexec/ipsec`, `/usr/lib/ipsec`, `/usr/lib/pcsc`, `/usr/sbin/swanctl`, `/usr/sbin/pcscd`,
   `/etc/strongswan.d`, `/etc/strongswan.conf`, `/etc/swanctl`, `/etc/reader.conf.d`, `/etc/gsm-sip-bridge` and
   `/usr/local/bin/gsm-sip-bridge`. Existing paths are never replaced.
4. **Adds `/usr/bin/ip -> /sbin/ip`.** `ip netns exec` with an empty `PATH` searches only `/bin:/usr/bin`.
5. **Bind-mounts `fake-usb/` read-only over `/sys/bus/usb/devices`.** gsm-sip-bridge discovers modems only by
   walking USB sysfs for Quectel IDs, and the stick is a USB *gadget*, so that directory is otherwise empty. The fake
   entry is `2c7c:0901` (EC200: AT-only, VoWiFi-only), with its AT port as `ttyUSB3`.

Then procd runs `run-native` with respawn and a 90 s stop timeout; supervise's own teardown budget is 60 s.
`run-native`:

1. Runs `prestart`. It waits up to about 3 min for `AT+CPIN?` to return `READY` and `AT+CIMI` to return an IMSI,
   because supervise probes the port only once at startup (5 s budget) and a miss disables VoWiFi for the whole run.
2. Loads `.env`.
3. Puts `$ROOT/{usr/local/bin,usr/sbin,usr/bin,sbin,bin}` first on `PATH` and `$ROOT/{usr/lib,lib}` on
   `LD_LIBRARY_PATH`.
4. Execs `gsm-sip-bridge --config /apps/gsm-sip-bridge/config.toml supervise`.

## 6. Verify

```sh
logread -e run-native | grep -E 'LINE_COUNT|tunnel UP|EAP_AKA|PROMINENT'
#   [supervise] discover: LINE_COUNT=1
#   [IKE] EAP method EAP_AKA succeeded, MSK established
#   [supervise] line 0: tunnel UP. P-CSCF: <ipv6>

set -a; . /apps/gsm-sip-bridge/.env; set +a
LD_LIBRARY_PATH=/overlay/gsm-sip-bridge-root/usr/lib:/overlay/gsm-sip-bridge-root/lib \
  /overlay/gsm-sip-bridge-root/usr/local/bin/gsm-sip-bridge --config /apps/gsm-sip-bridge/config.toml vowifi-status
#   state: Registered
#   gm_connection: up

ip -n ims0 -6 addr show tun23-0      # the carrier-assigned inner /128 must be present
logread -e ims.updown                # empty = no ip errors
```

On Vi India it registers within about 10 s of the tunnel coming up: IKE_SA_INIT, EAP-AKA, a virtual IPv6 address and
P-CSCF, then REGISTER returns `401`, then `200 OK` with Gm IPsec, and the reg-event subscription is confirmed.

Phones register to `<stick-wifi-ip>:5060/udp` as `1001`, with the password from `.env`. Inbound calls to the SIM's
number ring that account. Dial-out needs `[outbound] enabled = true`.

## 7. Operate

| Task | Command |
|---|---|
| Logs | `logread -e run-native`, `/tmp/ims-agent-0.out`, `/tmp/charon.log`, `logread -e qmi-sim-bridge` |
| Stop / start | `/etc/init.d/gsm-sip-bridge stop` / `start`. The stop is asynchronous, so wait until `pidof charon` is empty before starting again |
| Trace AT traffic | `uci set qmi-sim-bridge.main.trace=1; uci commit; /etc/init.d/qmi-sim-bridge restart` (noisy: the SMS sweep polls every ~20 s) |
| Upgrade the image | stop the service, remove `/overlay/gsm-sip-bridge-root`, repeat §4, start. The relink and updown patch re-apply themselves |
| Disable VoWiFi | `/etc/init.d/gsm-sip-bridge disable; /etc/init.d/gsm-sip-bridge stop` |

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `swanctl --list-sas failed (vici connection broken)`, then `NO_ADDITIONAL_SAS` in a loop | absolute busybox symlinks resolve to the host. Delete `$ROOT/.relinked` and restart |
| IMS agent: `TCP connect to [<P-CSCF>]:5060 failed: Host is unreachable` | inner address missing on `tun23-0`. Check that the `ims.updown` patch is present (`grep "qmi-sim-bridge native" $ROOT/etc/strongswan.d/ims.updown`) and read `logread -e ims.updown` |
| `discover: LINE_COUNT=0`, `sim_unreadable: AT+CIMI: no IMSI` | SIM not ready at discovery. Check `prestart`, then restart the service |
| `vowifi-ims-agent exited` loop with `AT command timeout` | the modem's `AT+CGSN` never sends `OK`. qmi-sim-bridge must rewrite it to `AT+GSN` (any current build does) |
| `AT+CSIM failed: ERROR`, qmi-sim-bridge logs `Broken pipe` | qmi-proxy restarted (ModemManager re-probe). Current qmi-sim-bridge reconnects by itself; restart it if it's an old build |
| LTE gone (`mmcli -L`: no modem) after VoWiFi starts | an old qmi-sim-bridge that reopens the SIM channel per SELECT/CFUN. Upgrade it, then `/etc/init.d/modemmanager restart` |
| `vowifi-status`: `secret variable PHONE_1001_PASSWORD is unset` | the CLI needs `.env` loaded (`set -a; . .env; set +a`) |
| No calls, or control-channel timeouts | fw4 `gsmbridge` zone missing (§2) |

## Known issues

- **Kernel memory corruption** on this image (see above). It hasn't been reproduced natively so far. After any
  unexpected reboot, save `/sys/fs/pstore/*`.
- **Modem SMS sweep:** gsm-sip-bridge polls modem SMS storage (`AT+CMGL=4`) through the pty and sometimes logs
  `AT command timeout`. SMS over IMS is unaffected, and ModemManager receives modem SMS over QMI anyway.
- **WCNSS crashes at boot** (`Data Abort`). The Wi-Fi phy is renumbered (`phy0`/`phy1`) with a different MAC, so a
  different DHCP lease; reserve both MACs. The station can stay down until `wifi down; wifi up`.
