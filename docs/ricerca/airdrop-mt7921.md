# airdrop-mt7921 — engineering digest for a Rust reimplementation ("lindrop")

Source: `~/Git/airdrop/airdrop-mt7921` at HEAD `7eb26f9` (2026-10-17, "Supporto iwd come
backend di NetworkManager"). Also consulted (read-only) the jedbillyb/owl fork and
opendrop 0.13.0 checkouts under the session scratchpad `ref/`.
References: `F§n` = docs/FINDINGS.md section, `N` = docs/NOTES.md, `R` = README.md,
`D` = daemon/README.md, `P:<name>` = patches/opendrop-<name>.patch, `PR` = patches/README.md,
`H` = daemon/airdrop-helper, `A` = airdrop.sh, `AD` = daemon/airdropd, `BW` = daemon/ble-watch,
`OWL:` = files in the owl fork.

FINDINGS is a dated log, and several sections retract earlier ones. **Only the final
conclusion is given below.** Retracted claims are listed explicitly in §0 so nobody
rebuilds on them.

---

## 0. Retracted claims (do not build on these)

| retracted claim | where | corrected by |
|---|---|---|
| Kernel 6.18 broke mt76 monitor RX, so pin 6.12.97 | F§2, N | F§11/§12: it was runtime-PM. 6.18 captures ~10x more than 6.12. Ignore kernel pinning |
| "txpower stuck at 3 dBm" is a symptom | F§2 | cosmetic mt76 reporting quirk, also in managed mode |
| MT7921 monitor mode is "nominal, not functional" | F§8 | F§9: runtime-PM + in-place type switch |
| Active monitor destroys 96-99% / 81% of RX | F§10-§12a | F§14: busy vs quiet channel confound; a lone active vif sits on an empty 5180 |
| AirDrop is impossible on MT7921 (no ACKs) | F§10, §11, §13 | F§14/§15: the monitor "pair" ACKs; transfers work |
| ping6 100% loss is inconclusive | F§15 | F§23: iOS answers ping6 from strangers; loss = genuine TX failure |
| Follow the most-recent data peer's sequence | F§16 | F§17: 6x worse (phase mismatch) |
| Rotate by sync_offset (as first prescribed) | F§17 | F§21: sync_offset was never assigned upstream; rotation was a no-op |
| `-S pin` maximises duty cycle | F§21 | F§25/§26: iOS 26 stops sending unicast to us; PIN is dead |
| "Phone needs ≥1 empty slot" | F§26 run 2 | F§26 run 3: `-W 12` (16/16) accepted; content not width matters |
| Control Centre makes the phone discoverable | F§19 | zero AWDL frames; the share sheet is required |
| "The phone never advertises _airdrop._tcp" | F§22 | F§27: it does, once the BLE advert is up |
| /Upload refused for container / framing / Expect / Content-Length / headers | F§29-§35 | F§37: the only cause was the TransferID not announced in /Ask |
| SenderRecordData needed to send | F§35/§37 pre-test | F§37 result: not required in Everyone mode |
| Two-part mt76+mac80211 patch to give monitor its own chanctx | F§42 | F§43/§44: gate is cfg80211+mac80211+firmware; firmware ignores it |
| "One chip cannot do both" | F§44 | F§45/§46: CNM time-slicing exists for P2P-GO; solved via a GO chanctx |
| AP must be on the phone's channel / move router to 149 | F§38, §40b | F§46: P2P-GO picks our own channel |
| avahi is not a problem (§40) | F§40 | D "Your current setup", H `avahi-down`: avahi answers for the host on awdl0 and wins the race; airdropd stops it |
| ServiceBrowser alone is enough | — | P:zeroconf-update-service, P:mdns-repeat |

---

## 1. Radio setup on MT7921 that actually works

### 1.1 The three silent mt7921 traps (F§9, F§12, F§13, R "MT7921 driver traps")

1. **Runtime PM kills monitor RX.** `runtime-pm` and `deep-sleep` default to 1. In monitor mode
   nothing keeps the chip awake, so it delivers 0 frames with no error. Write `0` to both,
   **after** creating each monitor vif (H `up` writes them after mon0 and again after mon1).
   Path: `/sys/kernel/debug/ieee80211/<phy>/mt76/{runtime-pm,deep-sleep}`. debugfs may not be
   mounted: `mountpoint -q /sys/kernel/debug || mount -t debugfs none /sys/kernel/debug`.
   Resolve `<phy>` from `/sys/class/net/$IFACE/phy80211`, never a hard-coded phy0. After a
   driver reload the card is phy1 and the write lands on the wrong radio silently (H lines 66-97).
   Restore `1` on teardown. Writing the knob also wakes the chip ("wake and stay awake").
2. **An in-place `iw dev wlp2s0 set type monitor` never retunes.** The hardware stays on 5180 MHz
   whatever you request, and `iw` reports the requested channel anyway. Always create a
   **dedicated vif**: `iw phy <phy> interface add mon0 type monitor` (F§9 table).
3. **A lone `flags active` monitor vif is stuck on 5180 MHz.** This holds for every ordering:
   create-active, active-after-tune, tune-while-down (F§13). `iw dev X set monitor active` on an
   up vif returns EBUSY (-16). Monitor flags cannot be read back (`iw dev X info` shows nothing).
   Judge only by accept/refuse and by the frames.

**Methodology rule (F§12, F§13, R):** `iw`'s reported channel is unreliable on this chip. The
only truth is the **radiotap frequency of received frames**. Check it in every test.

### 1.2 The monitor "pair": ACKs plus a chosen channel (F§14, F§15)

An active vif created **alongside** a plain vif that already owns and has tuned the channel rides
that channel context at full reception (710 frames vs 667 for plain alone), and it ACKs unicast.
The ACKs were proven when the phone initiated: 481 packets including TCP POSTs (F§15). Exclusive
recipe (= H `excl-up`, A layer 1):

```sh
systemctl stop NetworkManager [iwd]            # iwd as NM backend keeps the iface otherwise (H excl-up, A NM_UNITS)
pkill -x wpa_supplicant; pkill -x dhcpcd; sleep 1
iw reg set IT                                  # pin BEFORE/after teardown, see 1.4
ip link set wlp2s0 down
iw phy phyN interface add mon0 type monitor    # plain FIRST
ip link set mon0 up
echo 0 > $MT76/runtime-pm; echo 0 > $MT76/deep-sleep
iw dev mon0 set freq 5180                      # mon0 owns + tunes the chanctx
iw phy phyN interface add mon1 type monitor flags active   # alongside
ip link set mon1 up
echo 0 > $MT76/runtime-pm; echo 0 > $MT76/deep-sleep
owl -i mon1 -c 36 -N -S verbatim -vv           # OWL on the ACTIVE vif, -N mandatory
```

`-N` is mandatory. Otherwise OWL's own set-monitor-mode on an up monitor vif gets EBUSY and it
aborts with "Could not put device in monitor mode" (F§9 bug 3). OWL must run as root because it
creates the `awdl0` tun. Unprivileged, it prints its banner and never makes awdl0 (F§40).
Restore with `excl-down` (H): delete the vifs, PM back to 1, `rfkill unblock wifi`,
`iw dev wlp2s0 set type managed`, link up, restart iwd then NM. A detached `setsid` watchdog
should restore networking even after a SIGKILL (A lines ~287-307, N "Test harness safety").

Stack bring-up is fast: pair + PM off 0.057 s, OWL ready 0.117 s, awdl0 up 0.015 s, **0.19 s
total** (F§39). Every `sleep` in airdrop.sh is conservative.

### 1.3 Channels and frequencies

| ch | MHz | notes |
|---|---|---|
| 6 | 2437 | 2.4 GHz social. The phone's **slot 8 is always 6** (F§46). Only legal channel in the world domain |
| 36 | 5180 | not canonical-social, but iPhones on a 5 GHz AP on 36 put many slots there (`36,36,149,0,0,0,0,36,6,36,149,36,0,0,0,36`, 6/16; D channel policy) |
| 44 | 5220 | canonical social |
| 149 | 5745 | primary 5 GHz social; phone 149-dominant when idle/transferring (F§21, F§40) |
| 132 | 5660 | appears in sweeps/slot 0 but **OWL rejects it** (OWL:daemon/owl.c switch; D "buildable set") |

The OWL fork accepts only `-c 6|36|44|149` (opclass encodings `{6,0x51}`, `{36|44|149,0x80}`,
OWL:src/channel.h). The same set must be enforced in three places that must agree (D Tuning).
A mismatch shows up as "awdl0 never appeared".

**Slot 0 is the device's infra channel.** It was 36 while the phone was on the 5 GHz AP and 2
after it roamed to 2.4 GHz. Non-social values (3, 108, 112, 124, 132, 134) are real and are not
decode bugs (F§21, F§47). Example sequences by phone Wi-Fi state (D "Exclusive mode"):
`2,0,149,0,0,149,2,2,6,...` (2.4 GHz), `36,36,149,0,0,0,0,36,6,...` (5 GHz), `6,0,149,0,0,0,0,0,6,...`
(Wi-Fi off). The phone's channel cannot be predicted, so exclusive mode **sweeps**
(A layer 0): order `36 149 6 44 132` (cached winner first). For each channel: `set freq`,
1 s settle, 5 s `tcpdump -i mon0 "wlan addr3 00:25:00:ff:94:73" -c 300`. Pick the channel with
the most frames. Stop early at ≥100 frames, or at ≥25 on the cached channel. 0 frames on every
channel means the phone is not advertising. The sweep is what actually chooses the channel (see 1.5).

### 1.4 Regulatory pinning (H `excl-up`, issue #7, A)

While associated, the country comes from the AP IE. Killing the station drops the box to the
world domain, where **all 5 GHz is no-IR**. Injected frames are then refused silently and OWL
runs "deaf-mute". The fix: read `iw reg get` **before** teardown (or take `AIRDROP_REG`),
validate it as `[A-Z][A-Z]`, and run `iw reg set XX` after the station is gone. If there is no
country to pin, refuse 5 GHz exclusive mode and allow only ch6 (commit 27edf28). An intersection
domain like "98" has no code to re-set. In KEEP_WIFI/P2P-GO **do not** touch regdom while
associated (A layer 1). hostapd gets `country_code=` and `ieee80211d=1` (H go-up). For IT, verify
ch36/149 permissions with `iw reg get` after `iw reg set IT`. The repo only verified NZ
(149 = 36 dBm, no DFS, F§40b).

### 1.5 Channel hopping is effectively fiction. Design around one channel.

- With a plain mon0 alone, OWL did hop 149→6, but the radio reached ch6 only 2.7% of the time
  against 16.7% scheduled. Firmware switch latency eats the away window (F§9).
  `set_channel()` costs a mean of 8.5 ms (p90 9.5, max 10.1) blocking in upstream. The fork made
  it async with errors deliberately unreported (F§16, OWL README, F§38).
- **With the pair, mon0 owns the chanctx and OWL's netlink retunes on mon1 are accepted and
  ignored.** RX rate was unchanged, and all four bisect builds transmitted on 149 to a phone on
  149 (F§24, restated F§38). Conflict: F§14 step 4 saw `iw dev mon1 set freq` move both vifs on
  2.4 GHz. The later 5 GHz evidence is that OWL-driven hops do not move the radio. Treat the radio
  as **single-channel**, chosen by the sweep (or by the GO).
- Under P2P-GO, 2425/2425 frames were on 5180 while OWL logged 20 switches. Switching is a no-op
  (F§47 #4, D limitation 8).
- Corollary: `-S verbatim` works because of **what it advertises**, not because we follow (F§25).

### 1.6 Coexistence with the station without P2P-GO (KEEP_WIFI, F§38-§41)

Monitor vifs can be added next to an associated managed vif with no link impact. Monitor is a
mac80211 `software_iftype`, so it is not counted in the interface combinations (F§43). But a
monitor vif has **no channel context of its own**. `iw dev mon0 set freq` returns **EBUSY** and
radiotap shows the AP's frequency (F§38). So KEEP_WIFI works only if the phone's AWDL is on the
AP's channel, and F§40 showed the phone on 149 while the AP was on 36: 0 peers. Remain-on-channel
fails (`offchannel` ETIMEDOUT) and could only carry action frames, not AWDL data (F§41). Kernel
patches to all three gates (cfg80211 `cfg80211_has_monitors_only` chan.c:1550, mac80211 virtual
monitor `open_count==0` iface.c:1403, mt7921 `mcu_config_sniffer` only from change_chanctx) get
the full call chain to run. `MCU_UNI_CMD(SNIFFER)` returns success **and the radio does not move**
(593/593 frames on the BSS channel). The firmware has a CNM time-slicer for GO/WFD, but the
sniffer is not a client of it (F§44, F§45). **Dead end. Use P2P-GO.**

### 1.7 P2P-GO chanctx trick: Wi-Fi and AWDL on one MT7921 (F§46, F§47, F§48, H `go-up`, D)

**Why it works.**
1. The mt7921 advertises `#{managed,P2P-client}<=2, #{P2P-GO}<=1, #{P2P-device}<=1, total<=3,
   #channels<=2`, but `#{AP}<=1 … #channels<=1` (F§42). **Only a P2P-GO may hold a second
   channel.** The firmware CNM scheduler time-slices managed and GO (F§45). Stock hostapd always
   forces iftype AP, so it fails with `nl80211: Beacon set failed: -16` when the station is on a
   different channel.
2. **Monitor TX borrows another vif's chanctx by MAC.** `ieee80211_monitor_start_xmit()`
   (net/mac80211/tx.c ~2377) matches the injected frame's **addr2** against running non-monitor
   vifs and transmits on that sdata's chanctx. So alias mon0's and mon1's MAC to the GO's MAC, and
   OWL's frames (addr2 = own MAC) go out on the GO's channel.

**hostapd patch.** A one-line, env-gated change to hostapd 2.11 (built at
`/mnt/shared/build/hostapd-2.11`, installed root-owned as `/usr/local/bin/airdrop-go-hostapd`):
with `HOSTAPD_P2P_GO=1`, the nl80211 driver keeps the interface's iftype as `NL80211_IFTYPE_P2P_GO`
instead of resetting it to `AP`. `is_ap_interface()` already accepts P2P_GO, so nothing else
changes (F§46). The patch text lives in the separate repo `jedbillyb/mt7921-dual-channel`, which
is **not** in this checkout. In a Rust implementation you could skip hostapd entirely: create a
`P2P_GO` iftype vif and issue `NL80211_CMD_START_AP` with a minimal beacon yourself. That is
untested, an idea only.

**Exact sequence (H `go-up <6|36|44|149>`):**
```sh
iw phy $PHY interface add go0 type __p2pgo
sysctl -w net.ipv6.conf.go0.disable_ipv6=1     # BEFORE link up, see trap 3
ip link set go0 up; GOMAC=$(cat /sys/class/net/go0/address)
# hostapd.conf (chmod 600, fresh random WPA2 passphrase per run, nobody joins):
#  interface=go0 driver=nl80211 ssid=airdrop-relay-$$ country_code=XX ieee80211d=1
#  hw_mode=a|g channel=$chan ieee80211n=1 wmm_enabled=1 auth_algs=1 wpa=2
#  wpa_passphrase=<rand> wpa_key_mgmt=WPA-PSK rsn_pairwise=CCMP ctrl_interface=/var/run/hostapd
setsid env HOSTAPD_P2P_GO=1 airdrop-go-hostapd $CONF &   # setsid load-bearing: else dies with helper,
                                                         # and wlp2s0 went through 3 deauth/reassoc cycles
wait for 'AP-ENABLED' in log (<=6 s)
iw phy $PHY interface add mon0 type monitor; ip link set mon0 down
ip link set mon0 address $GOMAC; ip link set mon0 up
iw phy $PHY interface add mon1 type monitor flags active; down; address $GOMAC; up   # ACK vif
echo 0 > runtime-pm; echo 0 > deep-sleep
owl -i mon1 -c $chan -N -S intersect -vv      # intersect, never pin
```
Proven: RX on 149 while associated on 52, 222 frames; TX of 82 probe responses; 4 AWDL peers;
iPhone ping6 reply at 460 ms (F§46). mon1 with `flags active` MAC-aliased to go0 does ACK: a
transfer through it reached 99.1% (D "P2P-GO mode", F§47). A complete transfer has only been
**proven on an MT7922** (D "Known-good").

**Traps.**
1. **Silent-drop trap:** while associated, monitor injection whose addr2 matches no non-monitor
   vif is dropped inside mac80211. `send()` succeeds and tx_packets/tx_dropped/tx_errors all stay
   0. A no-GO control also gives 0. GO MAC gave 82 responses, an invented MAC gave 0 (F§46).
2. **2.4 GHz station plus any GO loses the association**, even with the GO on ch6. A 5 GHz station
   survives a GO on any 5 GHz channel: STA36+GO149 held, STA36+GO36 held, STA157+GO149 held,
   STA100+GO149 held (AD `go_target`, D "If the station is on 2.4 GHz"). For a 2.4 GHz station,
   the daemon locks the NM profile to `802-11-wireless.band a` once per run, only if the SSID has
   a 5 GHz BSS, using `nmcli connection up` and never disconnect+connect. The lock is released at
   the next start (D).
3. **go0 IPv6 alias:** because mon0 copies go0's MAC, awdl0's link-local equals go0's.
   ifaddr/zeroconf pick the interface by IP string and chose go0, so the `_airdrop._tcp` announce
   silently left via go0. Fix: `disable_ipv6=1` on go0 before link up (H lines 264-277).
4. **The GO cannot retune.** `hostapd_cli chan_switch` FAILs in every form, even 149→153 ("invalid
   frequency settings provided"), and the phy advertises no CSA. Changing channel needs a full
   teardown plus bring-up of a few seconds (F§47 #4, D limitation 7). That kills the "§46
   zero-cost GO on ch6 plus CSA" plan.
5. **Wait for awdl0's link-local to leave `tentative` (DAD)** before starting mDNS. zeroconf binds
   5353 once, does not retry, and otherwise "announces into nothing" (AD `stack_up` end).
6. Interface autodetect must skip mon*/go0 (they are mt7921-driven and sort first) and prefer an
   **associated** managed vif (H lines 44-64, D Gotchas).

**Costs.** Uplink latency rises from ~2.6 ms to ~40 ms median and 280-590 ms max (F§46). D measured
**200-400 ms, up to ~590 ms**, the whole time go0 is up, at 0% loss, back to 66 ms the instant
it comes down. On an MT7922, a **GO on the AP's own channel gives 2-8 ms**, against 59-66 ms with
the GO on 44 and the station on 36 (D channel policy). Same channel means no time-slicing.
**For a 5 GHz AP on ch36 the GO should sit on 36.**

**Channel policy, final (F§48, AD `go_target`, D "Channel policy in auto"). The station decides,
not the peer:**
1. Station unreadable (mid-roam, `iw dev X info` has no channel line): refuse and retry every
   2 s, never guess.
2. Station on 2.4 GHz: band-lock to 5 GHz, else refuse.
3. Explicit `AIRDROP_GO_CHAN`.
4. Station on a buildable channel (6/36/44/149): use the same channel.
5. Peer hint, if buildable.
6. Station on another 5 GHz channel: use 149.
7. No managed vif at all: 149.

Reason: with the AP on 36 and the phone advertising `36,36,149,0,0,0,0,36,6,36,149,36,0,0,0,36`,
following the peer to "dominant" 149 gave 2/16 overlap and 0 bytes in 60 s. Staying on 36 gives
6/16. Wrong-channel watch: OWL logs `NO OVERLAP with <mac> on ch X - peer wants ch Y` at INFO.
After `AIRDROP_WRONGCHAN_AFTER`=20 s the daemon may rebuild the GO (`AIRDROP_GO_FOLLOW=1`) or
mark it `unreachable`. Under station-first it mostly stays put.

**Which mode to use, as of HEAD:** the switch defaults to `AIRDROP_MODE=exclusive` (R Settings).
This drops Wi-Fi, builds the pair on `AIRDROP_EXCL_CHAN`=149, runs owl `-S verbatim`, and turns
off 60 s after the last transfer or 180 s with nothing. The station reconnected within ~7 s
(D "Exclusive mode"). Shared/P2P-GO needs the patched hostapd and only finds the phone when the
phone uses that channel. Note: excl-up does **not** sweep, and since hopping does not move the
radio (1.5) it effectively sits on 149. That is a nuance the repo does not discuss.

---

## 2. OWL usage and AWDL lessons

### 2.1 Flags (OWL:daemon/owl.c getopt `"Dc:dvi:h:a:t:fNS:W:"`)

`-i <vif>` (the active mon1), `-c 6|36|44|149` (static master channel; upstream default 6 = 2.4 GHz
hears nothing in a 5 GHz room, F§4), `-N` (mandatory: skip set-monitor-mode), `-v/-vv`
(`STATS` every 10 s), `-h awdl0` (tun name), `-f` (no RSSI filter), `-D` daemonise, `-d` dump.
`-S verbatim|widen|intersect|rotate|pin` (default verbatim), `-W 0..16` (widen fill, default 4).
Run under `stdbuf -oL`: piped stdout is 4 KiB block-buffered (F§39). Health signals (F§47 env
notes): `STATS tx_action tx_data tx_unicast tx_multicast | rx_action rx_data rx_unknown`.
`tx_unicast` frozen while `tx_multicast` climbs means the unicast gate is shut. `rx_action`=0 means
the phone is not talking at all. The log is not truncated between restarts, so stale `add peer`
lines lie.

### 2.2 What the jedbillyb/owl fork changes vs seemoo-lab/owl (OWL README "What differs")

1. **9bac866 follow-the-peer:** adopt the elected sync master's sequence instead of the static
   16× master channel (`awdl_chanseq_init_static`, upstream state.c). Validated with the peer
   held 9.78 s instead of evicted at 2-3 s (F§6, F§9). The peer timeout is really 2 s swept every
   1 s: the constants are µs despite `/* in ms */`.
2. **`-S` strategies** (OWL:src/channel.h, daemon/core.c `awdl_adopt_chanseq`; the source is always
   the elected sync master, `awdl_chanseq_source`):
   - `verbatim`: memcpy the master's sequence plus its encoding. The only one with a full transfer
     in exclusive mode.
   - `widen`: fill up to N empty slots with the peer's dominant channel, copying the peer's own
     encoded bytes, never touching slot 0. Accepted at every width up to 16/16 (F§26).
   - `intersect`: keep only the slots on our `-c` channel and blank the rest. On 0 overlap, keep
     the last advertised sequence (never advertise all-empty, which reads as "never available")
     and log `NO OVERLAP`. Use it for P2P-GO (F§47 #5).
   - `rotate`: rotate the peer's array by the slot delta derived from sync_offset, with
     2-tick hysteresis (F§21). Never validated live.
   - `pin`: 16× dominant channel. **iOS 26 stops sending us unicast** (ping 100% in 4/4 arms,
     F§25). Kept only as a negative control.
3. `sync_offset` is now populated for every peer from the sync-params TLV (`awdl_sync_error_tu`,
   int64, signed µs; OWL:src/rx.c:39-54). Upstream declared and read it but never assigned it.
   It is deliberately **not** applied in the unicast gate unless `rotate`, because for non-master
   peers the value is arbitrary (OWL:src/schedule.c).
4. Bug fix: `awdl_same_channel_as_peer()` decoded the peer's bytes with **our** encoding. It must
   use `peer->sequence_enc`, or with differing encodings the gate never opens: RX fine, TX zero
   (F§23).
5. Async non-blocking `set_channel()` with errors unreported. PIN channel picker: the operator
   channel only breaks exact ties (F§22). Channel set reduced to 6/36/44/149.
   Build: `cmake -DCMAKE_POLICY_VERSION_MINIMUM=3.5` for CMake 4 (F§3). googletest fails under
   current GCC, so build only the `owl` target (R).

### 2.3 AWDL facts used (OWL src, F§16, F§21)

AWDL BSSID `00:25:00:ff:94:73`. Slot = EAW = presence_mode(4) × aw_period(16 TU) = 64 TU =
**65.536 ms**. Sequence = 16 slots = **1.048 s**. PSF interval 110 TU as master, 440 TU as slave.
Each action frame carries the sender's own AW counter, so a channel-locked capture places frames
in the **sender's** slot: `slot = (aw_seq_number / presence_mode) % 16` (F§21, tools/slotmap.py).
Election compares (master_counter, metric), then tree height, then MAC; the phone wins (F§5).
TX is hardcoded at 12 Mbit/s legacy OFDM (src/tx.c:312, F§16).

### 2.4 Lessons, final

- **Zero slot means absent.** Devices transmit only in the slots their sequence names, despite
  `fill_channel=0xffff` ("repeat current") (F§21).
- **Phase:** the slot index is computed in the elected master's clock, so copying a non-master's
  array verbatim puts you on the right channels at the wrong times. That made throughput 6x
  worse, 7.2 vs 45 kB/s (F§17). The fast baseline happened only because the phone **was** the
  sync master.
- **The phone escalates during a transfer:** idle `132,0,149,0,0,0,0,0,6,0,149,0,0,0,0,0`
  (2/16 on 149), then 5/16, then 9/16, peaking at `149×6,6,6,6,149×5,6,6` (11/16, up to 15/16
  seen under intersect), oscillating at ~1 Hz (F§21, F§47). It also **narrows mid-transfer**
  (15→5→2/16), which is the likely cause of tail loss. The reason is unknown (F§47, D limitation 1).
- **The ~45-50 kB/s ceiling is duty cycle.** Bursts are 68 ms (one slot) at ~3 Mbit/s in-burst
  with 400-550 ms gaps (max 1049 ms = one sequence period). That is ~22-25 kB per availability
  window, fixed whatever the TCP window (F§16, F§18). The baseline duty cycle was 12.3% = exactly
  2/16, because we followed the sync master's 2-slot sequence while the data peer offered 5-11
  (F§21). RF loss ~4% retransmits. Blocking set_channel was 30% of loop time but did not explain
  the gaps. Measured end-to-end: exclusive 40-67 kB/s (R), P2P-GO+intersect ~73 kB/s (F§47).
  The widen-vs-verbatim transfer comparison (predicted 110-250 kB/s) was **never run** (R Open
  questions).
- **Availability is not the constraint, acceptability is** (F§25). The advertised sequence must
  look like an Apple sequence derived from the peer's own bytes: keep slot 0 (infra) and slot 8
  (2.4 GHz). Occupancy is fine. Content kills it (F§26).
- **ping6 to the peer's link-local over awdl0 is the 8-second TX test** (F§23): 5 pings, -W 2.
  A loss above 20% predicts the transfer will fail. 0% reached the Accept prompt, 60% could not
  complete a round trip (F§28). A re-measure up to 3 tries is built in. ping6 is categorical and
  cannot size an effect. Its noise is 20-60% across identical runs (F§26).

---

## 3. AirDrop protocol on iOS 26: every behaviour the patches fix

### 3.0 Baseline wire facts

- HTTPS over IPv6 link-local on awdl0. The phone listens on **8770**, opendrop on **8771**
  (8772 if busy). The cert is self-signed (`openssl req -x509` in `config.py`) and not verified
  in Everyone mode (F§50). TLS keylog: `ctx.keylog_filename` to decrypt captures
  (P:tls-keylog).
- mDNS: `_airdrop._tcp.local.` instance `<12-hex service_id>`, TXT `flags=136`
  (0x08 MIXED_TYPES | 0x80 DISCOVER; DVZIP 0x02 is **not** advertised and iOS still sends dvzip),
  host `<hostname>.local.`, AAAA = awdl0 link-local, IPv6-only. With the share sheet open the
  phone queries `PTR (QM)? _applicationServicePairing._tcp`, `_appSvcPrePair._tcp` and
  `_airdrop._tcp.local.` (F§19).
- Endpoints: `POST /Discover`, `POST /Ask`, `POST /Upload`. Bodies for Discover/Ask are binary
  plists.
- **Direction rules:** to receive, the phone's share sheet must be **open** (it is a sender and
  browses). To send, the phone must be unlocked with the share sheet **closed** and AirDrop on
  Everyone. An open sheet never advertises `_airdrop._tcp` (F§19, F§22, D Sending).

### 3.1 Headers Apple sends (F§33, F§34). Copy exactly, in this order, nothing else.

```
POST /Discover               POST /Ask                    POST /Upload
User-Agent: AirDrop/1.0      User-Agent: AirDrop/1.0      User-Agent: AirDrop/1.0
Connection: close            Connection: keep-alive       TotalBytes: 1303171
Transfer-Encoding: chunked   Transfer-Encoding: chunked   Content-Type: application/x-dvzip
                                                          SenderPseudonym: pseud:RDUDcHgUEfGhmdJSNF0FmQ
                                                          SenderPushToken: 1D0DD878…0493 (64 hex, upper)
                                                          TransferID: 664D3979-F245-4E9E-9EAC-80453E255E31
                                                          Connection: keep-alive
                                                          Transfer-Encoding: chunked
```
No Host, no Accept*, no Content-Type on Discover/Ask. Upstream opendrop sent 7 headers including
`Accept-Encoding: br, gzip, deflate` and `Host`. Python's `http.client.request()` always inserts
Host and Accept-Encoding and reorders headers. Use raw `putrequest(skip_host=True,
skip_accept_encoding=True)` and hand-written chunked framing (F§35, P:upload-arms `raw`). As a
**receiver**, accept both chunked and Content-Length, never require Content-Type on Discover/Ask,
and handle `Expect: 100-continue` if present. The captured iOS 26 /Upload in F§34 has none,
although F§29 claimed it had one.

### 3.2 Receive-side fixes (opendrop as server)

| # | behaviour | fix | ref |
|---|---|---|---|
| R1 | iOS sends /Discover and /Ask **chunked, no Content-Length**, and `int(None)` killed the connection. The phone's sheet showed nothing | `_read_request_body()`: loop `len=int(readline().rstrip() or 0,16)`; on 0 **consume the trailing CRLF** (one `readline()`); else `read(len)` then consume CRLF after each chunk. Trailers are not handled (none seen) | P:ios26-airdrop fix 1, F§15 |
| R2 | /Upload `Content-Type: application/x-dvzip`, so a 406 after the user tapped = "Failed" on the phone | accept `application/x-cpio` and `application/x-dvzip` | P:ios26-airdrop fix 2 |
| R3 | **dvzip container**: sequence of `[u32 BE header][payload]`. Bit 31 = STORED, low 31 bits = payload length on the wire, else a zlib stream. Each block decompresses to 128 KiB except the last. No trailer: stop at EOF or a zero-length header. Inside is an **ODC cpio** (`070707`, terminator `TRAILER!!!`). 2.06 MB = 17 blocks. Photos are mostly STORED blocks, so the raw size tracks progress against `TotalBytes` | `dvzip_to_cpio()`; **buffer the whole body to disk first**, extract after, and keep the raw file if extraction fails | P:ios26-airdrop fix 3 |
| R4 | Sender's opening 64 kB burst outran the first `read()`, and the window collapsed 64766→528 B and took ~40 s to recover | (a) `SO_RCVBUF` 4 MB on the listener; (b) answer `100-continue` only after the output file is open and the reader is built. **Correctness only, throughput unchanged** (F§18). **Caveat found in this review:** the SO_RCVBUF hunk lands after `return properties` in `get_properties()`, so it is unreachable dead code in the committed series (it also references an undefined `httpd`). Only (b) is live. In Rust, set SO_RCVBUF on the listening socket **before `listen()`** so window scaling is negotiated | P:recv-window, F§18 |
| R5 | No consent: opendrop 200s every /Ask, so an Everyone-mode stranger writes files unattended | `AIRDROP_CONFIRM` hook `(sender, "f1, f2 (+n more)")`. Non-zero exit → **403** (phone shows "Declined"). Hook unrunnable, no Wayland, or timeout → **decline (fail closed)**. Unset → accept (harness) | P:ask-confirm, F§39, D Security |
| R6 | Consent UI race: `swaynag --button-dismiss-no-terminal` runs the button command from a detached child after dismiss, so the marker was checked ~2 ms early and every Accept became a 403 | each button writes its own marker; poll for the answer with timeout as the outer bound plus 1 s grace. "If a helper's exit is not the answer, wait for the answer" | F§48, daemon/airdrop-confirm |
| R7 | **Single-threaded HTTPServer wedged:** iOS holds idle `Connection: keep-alive` sockets, the server parks in `handle_one_request()` and never `accept()`s the connection carrying the file (Recv-Q 1496, 12 min silence, phone "Waiting…") | `ThreadingHTTPServer`, `daemon_threads=True`. Also fixes a slow prompt filling the listen backlog with iOS validation connects | P:threaded-server, F§47 #6 |
| R8 | **URL items:** a shared link arrives in the /Ask body `Items` (array of strings; tolerate dict `URL`). `Files` is empty and **no /Upload ever follows**: our 200 on /Ask *is* the transfer | parse Items, show the URL in the prompt, open it only after consent, detached (`Popen` in a new session, not awaited, or /Ask times out). Allow only `http(s)`, checked twice | P:url-items |
| R9 | **Staging wrapper:** the cpio always holds `NSIRD_AirDrop_<6 random>/` (directory member **first**), `._NAME` (AppleDouble ~1.6 KB) and `NAME`. The suffix is per-transfer and meaningless | flatten after extraction, drop `._*`, suffix name clashes with `-1,-2`. Do not flatten a directory modified <~2 s ago (still being written) | F§49, tools/airdrop-tidy |
| R10 | Truncated transfers: opendrop crashed (`int(b'',16)` in `_next_chunk`) and discarded everything. **9 of 17** interrupted transfers already held the complete file and lost only the terminator. Stops range from 440 B to 943,642 B short (0.03-24%) | catch EOF, salvage a truncated dvzip block (`decompressobj`), walk ODC headers by hand (mode @18, namesize @59, filesize @65, octal). libarchive **zero-pads** truncated members, so compare *bytes present*. Ignore non-regular members (the wrapper dir is always "complete"). **Truncate** partial members to `n - data_off` and rename `.partial`, each in a separate try. Guard against `../` escape and negative or zero sizes (infinite loop in the request thread). Gzip cpio: raw `decompressobj(16+MAX_WBITS)` | P:salvage-truncated, P:salvage-trim |
| R11 | Stalls: no deadline on reads, a phone going quiet during /Ask hung the handler before the prompt | handler-class socket timeout `AIRDROP_STALL_TIMEOUT`=30 s. /Ask timeout → **408 + `Connection: close`**, then close (else the keep-alive socket idles). Upload read timeout adapts as `clamp(6 × worst inter-read gap, 8 s, 30 s)` | P:salvage-truncated |
| R12 | Receiver invisible ~30 s after arming: python-zeroconf announces once and **the iPhone never polls** | re-send PTR+SRV+TXT+AAAA (QR\|AA response, TTL 0 timing) every `OPENDROP_ANNOUNCE_INTERVAL`=5 s (2 s in exclusive mode) from a thread, **without re-registering**. The old kill-and-respawn every 30 s made the receiver appear and vanish | P:mdns-reannounce, F§47 #1/#3 |

### 3.3 Send-side fixes (opendrop as client)

| # | behaviour | fix | ref |
|---|---|---|---|
| S1 | Py ≥3.12 / Pillow 10 / libarchive-c API rot: `HTTPSConnection(key_file…)`, `ArchiveEntry(None, p)`, `Image.ANTIALIAS` → no packet ever sent | context-only TLS, new ArchiveEntry, `Resampling.LANCZOS` | P:py314-send, F§19 |
| S2 | **ServiceBrowser backoff** (queries at t=0,1,3,7,15,31,63 s) is wrong for AWDL: co-channel only ~2/16 of each 1.048 s, one unacked multicast frame each way. 7 queries got 0 replies | re-ask `PTR _airdrop._tcp.local.` every `OPENDROP_QUERY_INTERVAL`=1.5 s from a thread. Result: 68 mDNS packets, **14 replies** from the phone | P:mdns-repeat, F§19 |
| S3 | `update_service` missing → AttributeError kills the ServiceBrowser thread silently. A re-announcing receiver fires *update*, not *add* | handle update like add | P:zeroconf-update-service |
| S4 | Discovery report written after `zeroconf.close()`, which hangs >2 min on AWDL, so `send` read a 13,313 s-old report: "Receiver does not exist" | write the report on every discovery, before stop, atomically (tmp + fsync + rename). Bounded teardown: SIGINT, ≤5 s, SIGKILL (F§20). `timeout` sends SIGTERM, which is not KeyboardInterrupt (`timeout -s INT`) | P:find-report, F§19 #4, F§20, F§27 |
| S5 | Pick the receiver by **ID or name**, never index 0: in Everyone mode every Apple device, and our own receiver, are listed. Filter out own hostname and own awdl0 addresses | AD `send_peers`, F§27 #2, PR#9 | |
| S6 | **/Upload refused with TLS close_notify ~70 µs after our headers** (F§31). The cause: **our /Ask never announced the TransferID** | /Ask body adds `TransferID = {'id': <UPPER-UUID4>}` (a **dict**) and `TransferType = {'files': {}}`. /Upload repeats the **same** id in the `TransferID:` header. Result: `/Ask → 200`, `/Upload → 200` about 1 s later, file on the phone (F§37 result) | P:upload-arms, F§37 |
| S7 | Exact /Upload: same TLS connection as the accepted /Ask (a refused upload kills the socket; a fresh one has no /Ask and is refused). Chunked, dvzip of a cpio (128 KiB blocks, zlib, no trailer), Apple header order. `TotalBytes`=payload length, `SenderPseudonym = "pseud:" + urlsafe-b64(16 random bytes) without '='` (22 chars), `SenderPushToken` = 32 random bytes as upper hex, generated once per client. These are **forged, and not verified** | arm `reuse-chunked-dvzip-rawhdr` (only default). Body sent as **one chunk** `%x\r\n<payload>\r\n0\r\n\r\n` | P:upload-arms, F§34, F§35 |
| S8 | /Ask body (upstream plus fix): `SenderComputerName, BundleID="com.apple.finder", SenderModelName, SenderID=<service_id>, ConvertMediaFormats=False, TransferID, TransferType, Files=[{FileName, FileType=<UTI>, FileBomPath="./name", FileIsDirectory, ConvertMediaFormats=0}], FileIcon=<JPEG2000 thumb ≤540px>` (or `Items=[url]`), `SenderRecordData` only if available. The phone's own /Ask also carries `SenderRecordData` (CMS) and `SenderIdentityAuthTag` (3 bytes). Phone's /Ask reply: `ReceiverComputerName, ReceiverModelName, IDSSessionID, SupportsContactExchange=False, ReceiverPseudonym, ReceiverPushToken` (stable identity; stash it) | F§29, F§37, P:upload-arms | |
| S9 | Multi-file: one /Ask with N `Files` entries, **each sniffed for its own UTI** (one shared type mis-routed PDFs as public.image), one cpio holding all files. One Accept total | P:send-multifile | |
| S10 | **Send stall (phone stops ACKing):** e.g. 971,233 of 6,351,692 B acked, then 5+ min with the RTO at 120 s and no error | bound only the **upload write**: 64 KiB slices, each with `AIRDROP_STALL_TIMEOUT`=30 s to find buffer room. Keep the /Ask response read unbounded (a human is deciding). Restore the timeout before reading the response | P:send-stall |
| S11 | Exit status: `send` always exited 0 | return 1 on every failure path | P:send-status |

### 3.4 Send-direction transport (F§28). Distinct from S10.

The phone's 93-byte /Ask response was retransmitted 13 times over 18 s, then RST. OWL sent every
ACK (13 `Send data (len 133)` on the right channel and slot), and they were lost on air. RX was
perfect and TX broken in the same second. The layer-2.5 ping predicts it: an HTTP-layer
`RemoteDisconnected` is what TX loss looks like, so **do not patch HTTP on that evidence**. One
untested lead: the stalled ACKs cluster at tu 55-61 of 64, the first ~5 TU of the AW. Mitigation:
re-measure ping6 before committing the user's taps.

---

## 4. BLE

### 4.1 Wake advert used for sending (tools/blewake.sh, AD `send_ble_wake`, tools/blewake-dbus.py)

Continuity type 0x05 "AirDrop" (furiousMAC/continuity), legacy AD, 24 bytes:
```
17 FF 4C 00 05 12 | 00 00 00 00 00 00 00 00 | 01 | HH HH | HH HH | HH HH | HH HH | 00
len mfr Apple  t  L   8-byte zero prefix      ver  AppleID phone   email   email2  suffix
```
Each HH HH is the first 2 bytes of SHA256(identifier). Use all zeros in Everyone mode: Stute et al.
say a receiver wakes on a contact match **or** when discoverable by Everyone. Exact hex used:
`17FF4C000512000000000000000001000000000000000000`. BlueZ decodes it as "Company: Apple / Type:
AirDrop (5) / Data[18]".
- `btmgmt add-adv -d <hex> -g <instance>` (stdin `/dev/null`, or btmgmt drops into a shell),
  `btmgmt rm-adv`, `btmgmt advinfo` ("Instances list with N"). `hcitool cmd` is refused
  (0x0C Command Disallowed) because bluetoothd owns the controller. Claim the **lowest free**
  instance, record it, and remove only that one; never reclaim (issue #15). There are 20 instances
  max (H `ble_pick_instance`).
- **bluetoothd D-Bus (preferred, proven on MT7922):** `org.bluez.LEAdvertisingManager1.
  RegisterAdvertisement` on `/org/bluez/hci0` with a `LEAdvertisement1` object:
  `Type="broadcast"`, `ManufacturerData={0x004C: 05 12 00×8 01 00×9}`, `IncludeTxPower=False`.
  Keep a reference to the object, re-arm on `Release()` and on a timer, and guard against a
  double registration while the async reply is pending. On the MT7922, `btmgmt add-adv` failed
  after the radio sweep, or registered and the phone ignored it. **Every successful daemon send
  had blewake-dbus.py running** (D Known-good, tools/README).
- Proven effect: with the advert up the phone was found in ~4-6 s. With it expired, nothing was
  found despite 0% ping loss (F§27). After registering, wait 3 s before browsing (AD, A layer 3).
  Advert lifetime 600 s. Whether it must stay up during the upload is **unknown** (F§27).

### 4.2 "Same chip" problem (F§36, F§39, tools/blewake-dbus.py)

The MT7921 is a combo Wi-Fi/BT part. In the exclusive path, layer 1 (station down, monitor vifs,
runtime-pm/deep-sleep writes) resets the shared BT controller and **kills a previously registered
btmgmt advertising instance**, although the advertising process survives. Check `btmgmt advinfo`,
not `pgrep`. So start the advert **after** radio setup, verify the instance registered, then wait
3 s. Under KEEP_WIFI the controller stayed `powered` (F§39, advert survival not checked). On an
MT7922, a bluetoothd advert survived radio sweeps, `hci0` power cycles and `modprobe -r mt7921e`.
Net: register late, verify, re-arm on loss.

### 4.3 ble-watch: the on-demand receive trigger (BW, F§39, F§40)

- Runs `stdbuf -oL btmon` (load-bearing: 4 KiB buffering gave 0 output in 20 s) as root, plus an
  active scan `btmgmt find -l` restarted every 8 s (passive sees far less).
- Parses **only incoming `>` HCI records** that are `LE (Extended) Advertising Report`. btmon also
  shows our outgoing `<` commands, so without this our own type-0x05 advert self-triggers.
- Parses the **bytes, not btmon labels** (labels vary by BlueZ version). It anchors on `FF 4C 00`,
  validates that the preceding byte is a sane AD length, and walks the Continuity TLVs
  `type,len,data`, stopping at type 0x00 (padding) or an overrun. Type 0x01 len 0 ("Identifier")
  is real, not an artefact.
- Emits `{"ts","addr","rssi","type":5,"kind":"airdrop"}`. Filters: `--min-rssi` (daemon −70),
  `--cooldown` per (addr,type) (daemon 30 s). The daemon then brings the stack up for
  `AIRDROP_WINDOW`=90 s.
- Live result: **297 type-0x05 adverts at −26 dBm** the moment the share sheet opened, armed in
  **5 s**. A 4-minute control with the sheet closed saw **0**. In the same room there are ~96 Apple
  Continuity adverts per 10 s, mostly type 16 (F§40). The BLE trigger is proven. In that test the
  transfer failed only on the channel lock (§1.6).
- A Rust option: BlueZ D-Bus discovery with `SetDiscoveryFilter{Transport:"le", DuplicateData:true}`
  and reading `ManufacturerData[0x004C]` would avoid root and btmon. It is untested in the repo.

---

## 5. Discoverability and security

### 5.1 Everyone vs Contacts Only (F§7, F§22, F§50, D limitation 3)

- Only **Everyone** works, in both directions. iOS silently reverts to Contacts Only after
  **~10 minutes**, the most common cause of "it stopped seeing me" (D First three things).
  Everyone accepts an unsigned peer. The AirDrop-code handshake feared in F§7 never appeared.
- Contacts Only needs **three** secrets (seemoo-lab airdrop-keychain-extractor):
  `validation_record.cms` (Apple-signed, and harvestable: the phone sends its own as
  `SenderRecordData` in every /Ask, which ends up in `~/.opendrop/debug/receive_ask_request.plist`),
  `certificate.pem` (the Apple-issued Apple-ID client cert) and `key.pem` (never leaves the device).
  A harvested record is not a bearer token, because our TLS cert is self-signed. The record
  (Version 2, altDsID/encDsID, SuggestValidDuration 2592000 = 30 d, ValidAsOf,
  ValidatedEmailHashes, ValidatedPhoneHashes) has **no device binding**. So the theoretical path
  is to extract the cert and key **once** from a Mac signed into the same Apple ID (SIP off,
  `amfi_get_out_of_my_way=1`, extractor signed with a dev cert) and keep refreshing the record by
  harvesting it. **Unverified** that the peer accepts a record minted for another device on the
  same account. OpenDrop never verifies incoming records.

### 5.2 Security notes (D "Security", D Install, H header, patches)

- opendrop 0.13.0 accepts every /Ask. For anything unattended, consent is a **requirement**,
  fail-closed (6 verified paths, §3.2 R5). Always-on mode sets the consent timeout to 15 s
  (`AIRDROP_CONFIRM_TIMEOUT`, default 45). It is no longer needed that tight now the server is
  threaded (D limitation 10).
- Privilege: a single **root-owned wrapper with fixed verbs** under NOPASSWD, never NOPASSWD on
  `tcpdump` (`-w` = arbitrary root file write), `iw`, `ip` or owl. Binaries the helper runs as root
  (`/usr/local/bin/airdrop-owl`, `airdrop-go-hostapd`) are root-owned copies outside the repo; the
  helper checks `stat -c %U == root`. Never point sudoers at user-writable files. Validate with
  `visudo -cf` before installing. waybar has no tty, so sudo must be passwordless.
- Inputs: the channel comes from a fixed allowlist. BLE data must be hex, even length, ≤62 chars.
  The strategy must be `[a-z]`, ≤16, allowlisted, and passed as an argument: **root never opens a
  user path** (symlink or FIFO attacks). The country must be `[A-Z][A-Z]`. The interface is resolved
  by the helper, not passed in.
- GO network: a fresh random WPA2 passphrase per run, conf file `chmod 600`, nobody joins.
- Unauthenticated peer data: URL schemes are filtered twice. cpio member names are realpath-checked
  under the receive directory. Negative or zero namesize/filesize could spin the request thread
  forever. The consent prompt shows peer-chosen names.
- BLE instances: never remove another tool's advert (#15). Leaking a slot is the smaller failure.
- Ghost receivers: a killed opendrop never deregisters, so the phone shows duplicates. Flush by
  toggling AirDrop on the phone (D limitation 9).

---

## 6. Known open problems and unproven items (as of 7eb26f9)

1. **No complete MT7921 transfer through always-on P2P-GO.** It reached 99.1% (660,490 of
   666,346 B) at ~73 kB/s. The §48 accept-race fix was never retested against a phone on MT7921.
   Complete transfers exist only on an MT7922 (D Known-good, F§47, F§48).
2. **Tail loss and mid-transfer narrowing** (15→5→2/16). Cause unknown. Hypothesis: intersect
   mirrors the narrowing back, a feedback collapse. The untested fix is to stop mirroring downward
   and use `awdl_chanseq_widen`, since we are on the GO channel in all 16 slots (F§47, D limitation 1).
3. **Send on MT7921:** only `airdrop.sh send` has completed, and the proving send was **68 bytes**.
   Send speed is unmeasured. `airdropd send` is proven only on an MT7922 with blewake-dbus.py
   running. The daemon's own btmgmt advert is unproven for sending (R Open questions, D).
4. **TX loss bursts with perfect RX** (F§28) are unexplained. The AW-start (tu 55-61) lead is
   untested. 60% ping loss is the normal baseline.
5. **Throughput beyond ~45 kB/s:** the widen-vs-verbatim transfer A/B (F§21 prediction 110-250
   kB/s) was never run. TX is fixed at 12 Mbit/s OFDM.
6. **Hopping does not move the radio** with the pair or under a GO (F§24, F§47). Exclusive daemon
   mode starts on 149 without a sweep, so it relies on the phone being on 149.
7. **KEEP_WIFI** (borrow the AP channel) has never completed a transfer (R Modes, F§38).
8. **Station beacon loss** after 1-5 min of an active AWDL session on an MT7922 (BEACON-LOSS 1/s,
   only a driver reload recovers). Not checked on MT7921. Suspects: a monitor vif disables runtime
   PM and the firmware beacon filter, and the runtime-pm write clears `CONNECTION_MONITOR`
   (D limitation 12, issue #7).
9. P2P-GO limits: no roam following; 2.4 GHz-only SSIDs are unsupported; 5 GHz channels outside
   6/36/44/149 fall back to 149 (tested on 157 and 100 only); the GO cannot CSA; the
   wrong-channel watch is not exercised against a real mid-session channel move (F§48, D 4-7).
10. **BLE:** must the advert stay up for the whole upload (F§27)? Does a btmgmt advert survive
    layer 1 on MT7921 in each mode (F§36 vs F§39)? The receive trigger works.
11. **Contacts Only:** the harvest-record plus Mac-cert/key path is unverified (F§50).
12. `rotate` was never validated live. `sync_offset` for non-master peers is not meaningful
    (OWL:src/schedule.c comment).
13. Consent timeout of 15 s in always-on mode auto-declines (D limitation 10). Killed instances
    leave ghost entries (D 9).
14. Patch-level defect found in this review: `opendrop-recv-window.patch`'s SO_RCVBUF block is
    unreachable (placed after `return properties`), so the "window fixed" behaviour of F§18 is not
    reproduced by the committed series.
15. Environment-specific to this machine: iwd-backend support (7eb26f9) covers only
    `excl-up/excl-down/wifi-reset` and airdrop.sh (stop/start `iwd NetworkManager`). P2P-GO or
    KEEP_WIFI alongside an iwd-managed station is **untested**: whether iwd tolerates foreign
    go0/mon vifs on its wiphy is unknown. The repo's regulatory numbers are for NZ; IT limits
    need checking.

### Implementation guidance distilled (for this machine: MT7921, AP on 5 GHz ch36, IT)

- **Exclusive mode** is the only path with byte-exact complete receives on MT7921 (2.56 MB,
  PIL-verified, D). Recipe: pin `IT`, build the pair, sweep 36/149/6/44 on AWDL-BSSID frames, run
  owl `verbatim` on mon1, re-announce mDNS every 2 s, use a threaded HTTPS server with chunked,
  dvzip and salvage support, and get consent before writing.
- **Shared mode** for this AP: GO on **ch36**, the same channel as the station, so there is
  near-zero latency cost (MT7922: 2-8 ms). Use intersect. It reaches the phone when the phone is on
  the same AP, because its slot 0 and several more slots are then 36.
- **Send:** BLE type-0x05 advert via bluetoothd D-Bus (registered after radio setup). Browse with a
  1.5 s PTR re-query. Choose the receiver by name. On one TLS connection: /Discover, then /Ask with
  `TransferID={'id':U}`, `TransferType={'files':{}}`, then /Upload with the exact Apple headers and
  `TransferID: U`, a dvzip body in one chunk, and a progress-bounded write.
