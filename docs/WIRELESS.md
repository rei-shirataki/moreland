# Wireless: direct TLS over Wi-Fi (design)

**Status: design only. Nothing here is implemented or measured yet.** Every
latency figure below is an estimate and is marked as one; the plan is built so
the first step replaces them with measurements.

The project chose USB deliberately ([00-system-survey.md](00-system-survey.md)):
lossless, plug-and-play, no network exposure. This document designs a second
transport for when the cable is the problem, and keeps USB as the default.

## Two ways to go wireless

**A. ADB over Wi-Fi** (Android 11+ *Wireless debugging*, or `adb tcpip`). Every
coupling point to USB in the code is really a coupling to *adb*: `track-devices`
lists a wirelessly connected device as `192.168.x.y:port  device`, `adb shell`
and `adb forward` work unchanged, and the app's adbd-only peer check still
passes. So this needs almost no code. It is the right **first experiment** (see
[Phase 0](#phase-0--measure-before-building)), but a poor product: wireless
debugging drops on every network change and reboot, must be re-enabled in
Developer options, and every frame takes an extra hop through adbd's multiplexer.

**B. A direct connection** — the app speaks to the daemon over its own TLS
socket, with no adb anywhere. This document designs B.

## Goals and non-goals

Goals:

- Open the app on the tablet, and a monitor appears on the host — the wireless
  equivalent of plugging in.
- Desktop pixels and touch input are **never** sent in the clear, and **only a
  paired tablet** can receive the desktop or inject input. (Touch makes the
  second point critical: an unauthenticated peer could drive the host.)
- USB behaviour and latency do not change.
- Off unless the user enables it. The host listens on nothing by default.

Non-goals (for now):

- More than one tablet at a time. The daemon still runs one session.
- Internet / cross-subnet use. Same L2 network only.
- UDP with FEC. Deferred, with the reasons, in [Why TCP](#why-tcp-not-udp).

## Architecture

```
            tablet (initiator)                         host (listener)
   ┌────────────────────────────────┐        ┌────────────────────────────────┐
   │ NsdManager: _moreland._tcp ────┼─mDNS──▶│ advertised by the daemon       │
   │ user picks host / auto-connect │        │                                │
   │ SSLSocket (client cert,        │──TLS──▶│ rustls server, client auth     │
   │   pinned host cert)            │  1.3   │   required, pinned certs       │
   │ HELLO {display, board, caps}   │───────▶│ create output sized to HELLO   │
   │                                │◀───────│ StreamHeader + frames          │
   │ ACK / TOUCH / ORIENTATION/PING │───────▶│                                │
   └────────────────────────────────┘        └────────────────────────────────┘
```

**The tablet connects; the host listens.** That is the reverse of the USB path,
on purpose. Over USB the daemon can start the app (`am start`); without adb it
cannot, so the user opens the app, and opening it *is* the plug-in event. The
daemon is already a long-running service, so it is the natural listener.

## Transport

### Why TCP, not UDP

TCP for the first version, for a reason specific to this encoder: **`vah264enc`
exposes no intra-refresh** ([00-system-survey.md](00-system-survey.md#vaapi-encoder-capabilities-renderd129)). On a
lossy UDP link the only way to recover from a lost reference frame is a new IDR,
which is 200–400 KB against ~40 KB for a P-frame. That IDR is itself the frame
most likely to lose a packet on a congested link — loss causes keyframes, and
keyframes cause loss. Getting out of that needs intra-refresh (raw libva instead
of GStreamer), FEC, and NACK — a project in itself.

TCP keeps the stream lossless, so everything that relies on it still holds:
the 600-frame keyframe interval, the decoder never seeing a broken reference,
the existing framing. What TCP costs is **head-of-line stalls**: a lost segment
holds back everything behind it until it is resent. The in-flight cap
(`DEFAULT_MAX_IN_FLIGHT`) already limits how much video can pile up behind a
stall; the [rate control](#rate-control-and-queueing) changes below limit it
further. Wi-Fi also retransmits at the MAC layer, so most loss never reaches TCP.

UDP is revisited only if Phase 0/2 measurements show stalls dominate the tail.

### Encryption and authentication: TLS 1.3, pinned certificates

Both ends hold a long-term self-signed certificate and **pin** the other's
SHA-256 fingerprint at pairing time. No CA, no hostname check.

| | Host (Rust) | Tablet (Kotlin) |
|---|---|---|
| TLS | `rustls`, client auth required, custom verifier that accepts only pinned fingerprints | platform `SSLSocket` (Conscrypt), custom `TrustManager` that accepts only the pinned host fingerprint |
| Identity | Ed25519 or P-256 key + cert from `rcgen`, `~/.local/share/moreland/identity.{key,crt}`, mode 0600 | EC key generated in **Android Keystore** (non-exportable); the Keystore provides its self-signed cert |
| Paired peers | `~/.local/share/moreland/paired` — name + fingerprint per line | `SharedPreferences` (`allowBackup` is already false) |

The cost of AES-GCM at 20 Mbps is negligible on both ends (hardware AES on the
host's x86-64 and the tablet's ARMv8).

**The connection must be authenticated before anything else is sent.** The
daemon creates no virtual output and sends no pixels until the TLS handshake
has completed against a pinned client certificate. An unpaired peer gets only a
completed TLS handshake that tells it nothing, then a closed socket.

### Pairing: numeric comparison

Pairing has to establish both pins without a camera (a QR code would need the
`CAMERA` permission) and without a PAKE library (none ships with Android). It
uses the scheme Bluetooth calls *numeric comparison*:

1. On the host, `moreland pair` asks the running daemon (over a Unix socket in
   `$XDG_RUNTIME_DIR`) to open a **pairing window** for 120 s. Outside that
   window, unpinned certificates are rejected at the handshake.
2. On the tablet, the user picks the host. The app connects with TLS, accepting
   the host's certificate *provisionally*; the daemon accepts the tablet's
   certificate provisionally because the window is open.
3. Inside the encrypted channel: host sends `commit = SHA-256(Nh)`; tablet sends
   `Nt`; host reveals `Nh`; tablet checks the commitment.
4. Both compute `SAS = SHA-256(fp_host ‖ fp_tablet ‖ Nh ‖ Nt) mod 10⁶` and show
   it — six digits in the terminal and on the tablet.
5. The user confirms on **both** sides. Only then does each side store the
   other's fingerprint. Any mismatch, cancel, or timeout stores nothing.

The commitment in step 3 is what makes six digits enough: a man in the middle
must choose its certificates before it learns `Nh`, so it gets one 1-in-10⁶
guess per attempt rather than an offline search for a matching code.

`moreland unpair <name>` and `moreland paired` manage the host list; the app has
a matching "forget" action. Revoking a tablet on the host is immediate: its
fingerprint is no longer pinned, so its next handshake fails.

### Discovery: mDNS, with manual fallback

The daemon advertises `_moreland._tcp` with a TXT record carrying its protocol
version and a short fingerprint prefix (so the app can mark already-paired
hosts). The app browses with `NsdManager`, which uses the system's mDNS
responder, so no multicast lock is needed.

Host side: the pure-Rust `mdns-sd` crate rather than Avahi over D-Bus, to add no
system dependency. Whether it coexists cleanly with a running `avahi-daemon` on
UDP 5353 needs testing; if it does not, publishing through Avahi is the
fallback.

Networks that drop multicast (guest Wi-Fi, client isolation, some mesh systems)
break discovery, so the app also accepts **host:port typed by hand**. Pairing
and pinning are unchanged; discovery is only a convenience and never trusted.

### Host listener

`--listen [ADDR][:PORT]` enables it; off by default. Default port `27184` (next
to the probes' `27183`). Binding to a specific interface address is encouraged.
The doctor script gains a check that the port is reachable through the host
firewall.

## Wire protocol v4

The framing from v3 stays. The changes move everything the daemon currently
learns through `adb shell` into the stream, so both transports can share it.

### What adb does today, and where it goes

| Today (`adb shell`) | Wireless replacement |
|---|---|
| `pm list packages` (is the app installed?) | not needed: the app is the one connecting |
| `wm size` (panel size → stream resolution) | `HELLO.display` |
| `getprop ro.board.platform` (hi6250 tuning) | `HELLO.board` from `Build.HARDWARE` / `Build.SOC_MODEL` (API 31+) |
| `am force-stop`, `am start`, wake, dismiss keyguard | not needed: the user opened the app |
| `track-devices` (plug / unplug) | TLS connect / `BYE` / heartbeat timeout |

### New messages

Device → host gains two tags, alongside ACK `0x01`, TOUCH `0x02` and
ORIENTATION `0x03`:

```text
HELLO   tag 0x04, len u16, then:
          version u16, display_w u16, display_h u16,
          max_decode_fps u16, board_len u8, board utf-8, name_len u8, name utf-8
        sent once, first, before the host sends anything
PING    tag 0x05, seq u32       sent every 500 ms
```

Host → device today needs no tags: the first four bytes are either a frame
length or the magic `"MRLD"`, which as a `u32` exceeds `MAX_FRAME_BYTES`. A
second magic extends the same trick:

```text
CONTROL  magic "MRLC", type u8, len u16, payload
           type 0x01  PONG  seq u32
           type 0x02  BYE   reason u8   the host is ending the session
```

`"MRLC"` as a `u32` is `0x4D524C43`, also far above `MAX_FRAME_BYTES`, so the
existing `stream_header_magic_is_not_a_plausible_frame_length` test generalises.

`VERSION` becomes 4 on both ends. **The USB path moves to v4 as well** — it
sends `HELLO` over the adb socket — so there is one protocol, and `wm size` and
`getprop` go away from the USB path too. The app keeps no v3 fallback; host
and app are always built from the same tree.

### Liveness

On USB, unplugging is reported by adb at once. On Wi-Fi a dead link can look
alive for minutes (TCP keepalive defaults are far too slow), and acks cannot be
the signal because an idle output legitimately drops to ~1 fps. So: the app
sends `PING` every 500 ms, the host answers `PONG`, and either side ends the
session after **3 s** without one.

### Grace period

A Wi-Fi blip — roaming between access points, a microwave — should not tear
down the virtual output and send every window on it back to the primary
monitor. When a wireless session ends by timeout rather than `BYE`, the daemon
keeps the output for **15 s**; a reconnect from the same pinned tablet in that
window reattaches to it with a fresh encoder and keyframe. The capture and
encoder are torn down immediately, as on a rotation.

## Rate control and queueing

Over USB there is 13× headroom and none of this matters. Over Wi-Fi the
available rate changes by the second.

- **Keep the queue out of the kernel.** `TCP_NOTSENT_LOWAT` (Linux) set to a
  few tens of KB makes the socket report unwritable once that much is unsent,
  so backlog stays visible to the in-flight cap instead of hiding in a
  multi-megabyte send buffer. This is the single most important knob here.
- **Do not resume blindly after a stall.** `ACK_STALL` currently clears the
  in-flight set after 250 ms without an ack, which on Wi-Fi would pour frames
  into a link that is still stalled. On the wireless transport it becomes
  conditional on `PONG`s still arriving: a live link with no acks means the
  surface is gone (resume, as today); a link with no `PONG`s either means the
  network is stalled (keep holding).
- **Adaptive bitrate.** Start at 12 Mbps / 60 fps on Wi-Fi (an estimate, to be
  replaced by Phase 0 data), lower by 25% when the median round trip rises
  above twice the session's baseline for a second, and raise by 10% after 10 s
  of headroom, within `[4, --bitrate]` Mbps. Whether `vah264enc` accepts a new
  `bitrate` while playing must be verified; if not, the fallback is changing it
  only at the next encoder rebuild.
- **Tablet Wi-Fi power save.** While streaming, the app holds a
  `WifiManager.WifiLock` in `WIFI_MODE_FULL_LOW_LATENCY` (API 29+). Power save
  and background scans are the classic source of 100 ms+ spikes. This needs
  the `WAKE_LOCK` permission.
- **Host Wi-Fi.** A wired host is strongly preferred. If the host itself is on
  Wi-Fi, the doctor script reports whether power save is on (`iw dev <if> get
  power_save`).

## Daemon changes

`session::run` currently takes a serial and calls `adb` throughout. It becomes
transport-agnostic behind a small trait:

```rust
trait Link {
    /// Display size, board and decoder limits, from HELLO.
    fn device(&self) -> &DeviceInfo;
    /// The connected byte stream (adb-forwarded TCP, or TLS).
    fn stream(&mut self) -> &mut dyn Duplex;
    /// Transport-specific teardown: `am force-stop` for adb, BYE for Wi-Fi.
    fn close(self);
}
```

`AdbLink` does what `session::run` does now up to `Sender::connect`. `TlsLink`
wraps an accepted, authenticated connection.

**Threading.** The ack reader today works on a `try_clone` of the socket while
the drain thread writes. A `rustls` connection is one state machine and cannot
be split that way. Two options:

1. One I/O thread owns the connection and `poll`s the socket: it writes frames
   from a channel and delivers parsed device messages to another. Clean, but it
   adds a thread hop to every frame.
2. **kTLS**: after the `rustls` handshake, hand the session keys to the kernel
   (`setsockopt(TLS_TX/TLS_RX)`). The socket is then a plain fd again, so
   `try_clone` and the current threading work unchanged, and the kernel does the
   encryption. It needs the `tls` kernel module, which is in every mainstream
   distribution kernel.

Plan: implement (1) first, because it always works, and measure (2) as an
optimisation. Android's Conscrypt `SSLSocket` already supports one reader and
one writer thread at once, so `VideoStream`'s structure is unchanged.

**Main loop.** `main.rs` blocks on `tracker.next_update` with a 500 ms timeout.
A listener thread pushes authenticated `TlsLink`s into a channel, and the loop
waits on whichever arrives first. One session at a time, first come first
served: a USB plug-in during a wireless session starts nothing until that
session ends, and the reverse.

New crates: `rustls`, `rcgen`, `sha2`, `mdns-sd`. Today the transport crate
depends only on `anyhow`, `tracing` and `protocol`, so this is the largest
dependency addition the project has made; it all goes behind a `wireless` cargo
feature so a USB-only build stays as small as it is now.

## App changes

- **Two ways in.** Launched by the daemon over adb (the `SOCKET_EXTRA` intent
  extra present): behave exactly as today. Launched from the launcher: show the
  host picker — discovered hosts, paired ones first, plus "enter address". With
  a paired host present, connect to it automatically, so opening the app is
  enough.
- **Permissions.** `INTERNET` and `WAKE_LOCK` (both normal, granted at
  install), and `ACCESS_NETWORK_STATE` to tell the user when they are not on
  Wi-Fi. The README's claim that the app requests no permissions has to change.
- **Session end.** Without `am force-stop` from the host, a `BYE` or timeout
  returns the app to the host picker instead of closing it.
- **Battery.** Without a cable the tablet is not charging. The status overlay
  shows battery level while streaming.

## Security, revised

What changes from the current README section:

- **There is network exposure.** With `--listen`, the daemon accepts TCP
  connections on the LAN. Before pairing, all it offers a stranger is a TLS
  handshake that fails; nothing is created or sent until a pinned certificate
  is presented.
- **The pairing window is the sensitive moment.** It opens only on an explicit
  `moreland pair`, lasts 120 s, and requires confirmation on both devices.
- **A stolen tablet is a paired tablet.** Its Keystore key can still
  authenticate. `moreland unpair` revokes it on the host immediately.
- **Touch is remote input.** A paired tablet can tap and drag on the host's
  virtual output — the same power it has over USB, now reachable over the air.
  That is why authentication is mandatory, with no "trusted network" opt-out.
- **mDNS reveals the host's presence** and its fingerprint prefix on the LAN.
  The fingerprint is public by design; it is not a secret.

## Latency budget (estimates)

| Stage | USB (measured) | Wi-Fi 5 GHz (estimate) |
|---|---|---|
| Capture → DMA-BUF | 2.1 ms | 2.1 ms |
| VAAPI encode | 6.3 ms | 6.3 ms |
| Transport | 3–8 ms (est.) | 3–15 ms median; tail 50–200 ms on power save, scans, interference |
| TLS | — | < 0.5 ms (est.) |
| Decode + display queue | ~24 ms total with transport | unchanged |

The **median** is likely to be close to USB. The **tail** is what Wi-Fi makes
worse, and it is what the queueing changes above are for. Gaming and stylus
work were already out of scope; this does not change that.

## Plan

### Phase 0 — measure before building

No code. Connect the tablet with Android's Wireless debugging (`adb pair`,
`adb connect`) and run the existing benchmark:

```bash
moreland --once --seconds 25 --fps 60 --bitrate 12000
```

on 5 GHz and 2.4 GHz, with a full-screen animation on the virtual output, and
compare median / p95 / max / unacked against USB. This gives a ceiling estimate
(adb adds a hop that B removes) and shows whether head-of-line stalls dominate.
If p95 on 5 GHz is already acceptable, option A may be enough — and B can wait.

### Phase 1 — protocol v4 and the `Link` split

`HELLO`, `PING`, `CONTROL`, `VERSION = 4`; `session::run` behind `Link`; the
USB path migrated. No user-visible change — measured USB latency must not move.

### Phase 2 — the wireless session

Identity, `rustls` listener, pairing over the control socket, app host picker
with manual address entry, liveness. Measure against Phase 0.

### Phase 3 — make it pleasant

mDNS discovery, auto-connect to the last host, the 15 s grace period.

### Phase 4 — tail latency

`TCP_NOTSENT_LOWAT`, the stall rule, adaptive bitrate, `WifiLock`, kTLS. Each
one measured on its own against Phase 2.

### Not planned unless measurements demand it

UDP transport with FEC and NACK, which also requires intra-refresh and so moves
encoding from GStreamer to raw libva.

## Decisions needed

1. **Is adding `INTERNET` to the app acceptable?** "No permissions at all" is a
   headline security property today. A separate build flavour (`usb`/`wireless`)
   could keep the USB-only APK permission-free.
2. **Is Phase 0 enough?** If Wireless debugging measures well, maybe B is not
   needed at all.
3. **Grace period length** (15 s proposed) and whether the auto-connect on app
   launch should be on by default.
