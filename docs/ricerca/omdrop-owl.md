# omdrop-owl digest for lindrop (MT7921 / mt7921e, AP on ch36, country IT)

Sources: `~/Git/airdrop/omdrop-owl` @ 8213aee (v0.3.0, OWL pin `832d70f` from jedbillyb/owl
plus 11 patches); GitHub t4t5/omdrop-owl issues #14, #38, #44, #57 and PRs #13, #24, #54, #55, #58,
#59, #62, #64; shaecodes-wilde/mt76 commits `02b6c9a1` and `2a2f085d`; mainline mt76
(`mt76_connac_mac.c`, `mt7921/pci_mac.c`, `tx.c`, `mac80211.c`); the jedbillyb/owl README; the
jedbillyb/mt7921-dual-channel README; the omdrop-plugin `docs/radio-backend.md`.
Local read-only facts (from `iw`): wlp63s0 is `mt7921e` `14c3:7961` (the same chip as the main
MT7921 tester verusferro). The station is on ch36, 80 MHz, center 5210. The regdomain is IT
(DFS-ETSI): 36/40/44/48 at 23 dBm, 149 allowed at 13 dBm, none flagged No-IR. Interface modes have
**no IBSS** (only the frame tables list it). "Device supports active monitor" is present.
Combinations: `#{managed,P2P-client}<=2, #{P2P-GO}<=1, #{P2P-device}<=1, total<=3, #channels<=2`
and `... #{AP}<=1 ... #channels<=1`.

---------------------------------------------------------------------------------------------------

## 0. TL;DR for us

* **Stock mt7921e cannot carry AWDL unicast on 5 GHz.** Unicast data injected from a monitor vif
  goes out at **1 Mbit/s CCK** whatever rate radiotap asks for. CCK is illegal on 5 GHz, so it never
  airs: 0 of 132 frames on ch44 reached an independent sniffer. Multicast and management frames
  do air, at 6 Mbit/s. On ch6 unicast airs (115/121, 100 ACKed) but at 1 Mbit/s.
* Root cause in mt76: for unicast data the TXWI has no `FIX_RATE`, so the firmware uses the WTBL
  rate of the global or reserved wcid, which defaults to CCK 1M. `IEEE80211_TX_CTRL_RATE_INJECT`
  is ignored. An extra trap: `mt76_tx_status_skb_add()` memsets the mt76 tx cb, which aliases
  `info->control.flags`, so a check placed after it sees RATE_INJECT already cleared.
  **A ~40-line patch in `mt7921e_tx_prepare_skb()` fixes it.** It is tested on 14c3:7961,
  14c3:0616 and 14c3:7922, but is not in mainline or openwrt/mt76 as of 2026-10.
* With that patch plus the HT extension, the MT7921 on **ch44** in active-monitor mode moves an iPad
  upload at **9-14 MB/s** and a download to the iPad at about 3 MB/s. The MT7922 in **shared** mode
  on ch44 runs for more than 25 h at 1.2-1.4 MB/s with legacy 12 Mbit/s.
* AWDL needs our radio on **6, 44 or 149** (the social channels). **ch36 is not usable for shared
  mode**: omdrop-owl refuses it. Our AP's 80 MHz block 36-48 contains 44, but the primary is 36,
  and the firmware's monitor follows the BSS's primary channel. Options are in §3.

---------------------------------------------------------------------------------------------------

## 1. Architecture of omdrop-owl

### 1.1 Components and layering

```
omdrop (plugin/CLI, user) --pkexec--> /usr/lib/omdrop/omdrop-discoverable (bash, root)
                                         |-- creates mon0 (+ awdlibss0 | awdlap0), tunes radio
                                         |-- setsid owl -i mon0 -c <chan> -N -S <strategy> [...] -v
                                         |      owl creates TAP "awdl0" (AWDL addr = MAC), libpcap inject/capture on mon0
                                         |-- setsid omdrop-discoverable __supervise  (loop)
                                         |      awdl-airdrop-adv.py --iface awdl0 --plain --flags 137 --port 8771
omdrop receiver airdrop-serve.py (user) : HTTPS on [fe80::..%awdl0]:8771  (/Discover, /Ask, /Upload)
send-to-peer / airdrop-send.py / ble-airdrop-adv.py (omdrop-awdl tools, unmodified, pinned 534f91a)
```

* **Helper contract (omdrop-awdl 0.8.1).** `start <secs>` (0 means until stopped),
  `stop [reason]`, `status [--json]`, `peers`, `probe --json`.
  * `status --json` returns
    `{"visible","remaining","reason","rx_proven","peer_op"}`.
  * Exit codes: 0 ok; 2 usage or not root; 3 retryable (Wi-Fi not connected, AP not on an AWDL
    channel); 4 no AWDL radio; 5 already visible (the window is extended instead).
  * `start` returns only once `awdl0` has a **non-tentative IPv6 link-local**.
  * `stop` must **never reload the driver**.
  * `probe --json` is unprivileged, must answer within 5 s, and returns
    `{"backend":"owl","version","contract","hardware":bool,"missing":[{"id","say"}]}`.
* **Privilege model.** polkit action `io.github.t4t5.omdrop-owl.discover`:
  * `allow_active=yes`; `allow_any` and `allow_inactive` are `auth_admin`.
  * It is bound by `org.freedesktop.policykit.exec.path` to the root-owned
    `/usr/lib/omdrop/omdrop-discoverable`, with `exec.allow_gui=true`.
  * pkexec drops the environment, so everything uses fixed paths: state in `/run/omdrop-owl`,
    knobs in `/etc/omdrop`.
  * `status` and `peers` run unprivileged. They check liveness through `/proc/<pid>/cmdline`
    matching, not `kill -0`, which gives EPERM on root processes.
  * Serialization uses `flock` on `$RUN/lock`. Children are started with `9>&-` so long-lived
    daemons do not hold the lock.
* **Peers.** Parsed from OWL's INFO log: `add peer <mac>` and `remove peer <mac>`, in ether_ntoa
  unpadded form. OWL also installs each peer as a **permanent IPv6 neighbour on awdl0**, which
  send-to-peer merges in. RSSI is unknown (`rssi=?`). iPhones randomise their AWDL MAC.
* **`rx_proven`** means "add peer" has appeared in owl.log during this window.
* **Announcement.** mDNS `_airdrop._tcp` on awdl0, with receiver flags `0x89` (137 =
  SUPPORTS_MIXED_TYPES | SUPPORTS_DISCOVER_MAYBE | SUPPORTS_URL), port 8771, every 3 s.
  * Shared mode sends a burst of 3 copies 50 ms apart.
  * Exclusive mode sends **64 copies every 16.4 ms (16 TU = one AW)**, so one full 16-slot period
    is covered. Under `-F` the phone may visit our channel for only one slot in 16.
* **Sending** uses BLE wake-up (`ble-airdrop-adv.py`) first, then `/Discover` and `/Ask`.
* The **firewall** must allow TCP 8771 from fe80::/10 on awdl0. ufw silently breaks
  phone-to-PC transfers.

### 1.2 Profile table (`userspace/owl-profiles`)

The format is `driver status mode mon_mac reconnect mt76_pm notes...`.
`/etc/omdrop/owl-profiles` is read first. A driver with no line falls back to the `*` line, which
needs `/etc/omdrop/allow-untested`. The current lines:

```
mt7925e  tested    shared          station yes yes
mt7921e  untested  active-monitor  station no  no   # "1 Mbit/s CCK whatever rate OWL asks; on 5 GHz none of it reaches the air"
mt76x2u  tested    active-monitor  station no  no
iwlwifi  tested    exclusive       station no  no
rtw88_8822be tested exclusive station no no ; rtw89_8852ae/be untested shared ; rtw89_8852bu untested exclusive
*        untested  shared          station yes no
```

* `mon_mac station`: mon0 gets the station's MAC, and OWL reads its AWDL address from mon0.
* `mon_mac own`: mon0 gets `02:` plus the station MAC's last five octets, and OWL is given the
  station MAC with `owl -m`.
* `reconnect yes`: run `nmcli device connect` straight after mon0 is deleted. The MT7925's station
  stops receiving 3-165 s after a monitor vif is deleted.
* `mt76_pm yes`: write 0 to debugfs `ieee80211/<phy>/mt76/{runtime-pm,deep-sleep}` for the window,
  and restore the saved values on stop. jedbillyb notes that **mt76 monitor RX delivers nothing
  while runtime PM is on**, with no error reported.
* MT7922 shared mode with the rate fix used `mt7921e shared station yes yes` (shaecodes-wilde).

### 1.3 Shared mode (Wi-Fi stays associated)

1. Find the station: `/etc/omdrop/infra-iface`, or else the first `type managed` netdev that is
   not awdl0, mon0, awdlibss0 or awdlap0. Get the driver from `/sys/class/net/X/device/driver`.
2. Wait up to 15 s for a default route and a successful ping to the gateway through the station.
3. Read the AP frequency from `iw dev $sta link`. It must be 2437 (6), 5220 (44) or 5745 (149);
   anything else exits 3 with "move the router".
4. `iw phy $phy interface add mon0 type monitor` (a plain monitor, not active). Then
   `ip link set mon0 address <station MAC>` (or the `own` variant), then `up`. The monitor rides
   the station's channel context. It cannot retune while associated: cfg80211 returns `EBUSY` when
   not all running interfaces are monitors. On mt7921 the firmware sniffer is bound to the BSS
   channel anyway (§4.6).
5. mt76_pm off if the profile asks.
6. `owl -i mon0 -c <6|44|149> -N -S intersect [-m MAC] [-A] -v`.
   * `-N` means do not switch the interface into monitor mode: the vif is pre-made, and
     jedbillyb says it is **required on MT7921**.
   * `-S intersect` advertises only the slots in which the peer is on our channel.
   * **ACKs to the phone's unicast come from the station vif**, which has the same MAC; the
     firmware ACKs frames addressed to its own MAC.
7. Wait at most 10 s, in 50 ms polls, for `awdl0` to appear. Then:
   * `nmcli device set awdl0 managed no`;
   * `sysctl net.ipv6.conf.awdl0.accept_dad=0`;
   * delete the tentative link-local and re-add it with `nodad`. DAD otherwise cost about 1.2 s.
   * Wait until the link-local is non-tentative.
8. Start the supervisor.

### 1.4 Exclusive mode (Wi-Fi taken down for the window)

1. Save the connection UUID with `nmcli -g GENERAL.CON-UUID device show $sta` while still
   connected. Then `nmcli device set $sta managed no` and `ip link set $sta down`.
2. Pick channels with `tx_channels`. From `iw phy channels`, drop any channel marked
   disabled/No IR/radar. The default order is `44 149 6`; `iwlwifi` gets `6`; `/etc/omdrop/channels`
   overrides.
3. If the phy has IBSS (judged from "Supported interface modes" only):
   * `iw phy $phy interface add awdlibss0 type ibss` and set the station's MAC. If the card refuses
     an IBSS vif beside the station (AX211 gives EOPNOTSUPP), `iw dev $sta set type ibss` and
     record that in `$RUN/converted`.
   * `disable_ipv6=1` on the cell.
   * `iw dev X ibss join omdrop <freq> HT20 fixed-freq 00:25:00:ff:94:73 beacon-interval 10000`.
     That is the AWDL BSSID. The cell **must be HT**: iPhones send unicast at MCS7 (65 Mbit/s), and
     a non-HT cell neither receives nor ACKs it. The beacon interval is 10 s because an iPhone
     restarted next to a cell beaconing every 100 TU did not list us.
   * Wait until the link shows "Joined IBSS".
4. Without IBSS: a hidden **hostapd AP** `awdlap0` (`type __ap`) with the station MAC.
   * `ignore_broadcast_ssid=1`, random SSID and PSK, `ieee80211n=1`, `beacon_int=1000`.
   * The cell and AP exist only so the firmware **ACKs unicast to our MAC** and has a BSSID filter
     that lets AWDL through.
5. mon0 as in shared mode. `owl ... -F -E -S pin` (plus `-A` if aggregation is enabled).
6. Stop: kill hostapd; leave and delete the IBSS; turn a converted station back into managed and
   restore IPv6; `managed yes`. Then wait up to 10 s for NetworkManager to autoconnect, otherwise
   `nmcli -w 10 connection up uuid U ifname S`. That is tried first as the pkexec/sudo user through
   `runuser`, because psk-flags=1 secrets live in the user's agent, and then as root, up to three
   times.

### 1.5 Active-monitor mode (the one for mt7921e: no IBSS)

This is exclusive mode with the cell replaced by an **active monitor**, which ACKs unicast to its
own MAC (from PR #13 by kmod-midori, merged as #24).

```
nmcli device set $sta managed no; ip link set $sta down
iw dev $sta set type monitor; iw dev $sta set monitor none; ip link set $sta up   # station -> passive monitor (holds the chanctx)
iw phy $phy interface add mon0 type monitor flags active
ip link set mon0 address <station MAC>; ip link set mon0 up
for f in $freqs: iw dev $sta set freq $f HT20; iw dev mon0 set freq $f HT20 && break
owl -i mon0 -c $chan -N -S pin -F -E [-A] -v
stop: del mon0; ip link set $sta down; iw dev $sta set type managed; up; managed yes; connection up <uuid>
```

* An active monitor **on its own accepted channel changes but stayed on the old channel**
  (MT7612U, kernel 7.2). Woodinferno saw the same on mt7921 with 7.2.9: "a plain or active monitor
  vif on its own never retunes; only the station plus active-monitor layout hops". So the station
  is turned into a passive monitor first, and **both** interfaces are tuned.
* jedbillyb puts the same idea as "create a plain monitor vif and tune it first, then add the
  active vif alongside it: they share one channel context, so you get hardware ACKs and channel
  hopping at once".
* `active_monitor_problem` requires `mon_mac station` and "Device supports active monitor" in
  `iw phy`.
* Channels: **6 only for mt7921e** (stock driver). mt76x2u tries `149 44 6`. PR #62 adds
  `/etc/omdrop/exclusive-channel <MHz>` so mt7921e can run on 44 with the patched driver.
* Issue #57 (MT7921, 7.2.8): Wi-Fi was naturally down for the whole window (0/86 pings) and back
  after stop (300/300).

### 1.6 Supervision (`__supervise` loop, every ADV_INTERVAL = 3 s)

`stop <reason>` is run (via `exec`) on any of:
* window expiry: `expired`;
* OWL pid gone: `owl-exited`;
* `mon0` gone: `radio-lost`. A driver reload, such as after an iwlwifi firmware crash, kills mon0
  while OWL keeps running and awdl0 keeps its address (#23).
* hostapd dead: `hostapd-exited`.

Otherwise it runs one announcement burst per pass. `start` with a live supervisor only extends
`$RUN/until` (exit 5). A stale `converted` or `monitor` marker from a failed stop triggers a
recovery stop before the next start.

### 1.7 Keeping NetworkManager and udev away (packaging)

* `/usr/lib/NetworkManager/conf.d/99-omdrop-owl-unmanaged.conf` contains:
  `[keyfile] unmanaged-devices=interface-name:awdl0;interface-name:mon0;interface-name:awdlibss0;interface-name:awdlap0`.
  Post-install runs `nmcli general reload conf`, which touches no connection. A managed awdl0 gets a
  **randomised MAC and no IPv6 LL**. A managed mon0 kept the phone from ever being heard, and
  crashed iwlwifi's firmware about a second into the window (#23).
* `/usr/lib/systemd/network/10-omdrop-owl-awdl0.link` contains:
  `[Match] OriginalName=awdl0 / [Link] MACAddressPolicy=none`. udev's `99-default.link`
  (persistent policy) otherwise rewrites awdl0's MAC after the LL is derived (seen on Ubuntu 22.04),
  so omdrop announces an LL that awdl0 no longer has.
* The helper also calls `nmcli device set <if> managed no` at runtime for awdl0, awdlap0 and the
  station.
* NixOS module:
  * `networking.networkmanager.unmanaged` lists the same four interfaces;
  * the firewall opens TCP 8771 on awdl0 only;
  * a udev rule `SUBSYSTEM=="net", KERNEL=="awdl0|mon0|awdlibss0|awdlap0", RUN:="/bin/true"`
    stops NixOS restarting wpa_supplicant (and dropping Wi-Fi) whenever a vif comes or goes;
  * the package is built against the system NetworkManager, because a newer nmcli fails with
    "No such method SetManaged".
* **iwd backend (ours).** None of this is iwd-specific. NM with iwd still honours
  `unmanaged-devices`. iwd itself only manages station-type netdevs it created or adopted. Check
  that iwd does not grab the station after `set type managed`, and that NM brings it back. Note the
  local airdrop-mt7921 fork has a commit "Supporto iwd come backend di NetworkManager".

---------------------------------------------------------------------------------------------------

## 2. The OWL patches (on jedbillyb/owl 832d70f), as AWDL lessons

The jedbillyb base already provides:
* `-S verbatim|widen|intersect|rotate|pin` (how to derive our 16-slot channel sequence from a
  peer's);
* `sync_offset` actually populated from the Sync Params TLV (upstream never assigned it);
* async non-blocking `set_channel`;
* "iOS 26 fixes".

jedbillyb notes that **plain `pin`** (sitting on the peer's channel in all 16 slots) "broke TX to
iOS 26". omdrop-owl's patch 06 changes what `pin` means under `-F`, and uses it in exclusive mode
successfully.

| # | Flag | What and why |
|---|---|---|
| 01 tx-rate | `-R 6..54 \| mcsN[-sgi]` | Data frames use a radiotap RATE field (500 kb/s units) or, for MCS, an MCS field: known = HAVE_BW\|HAVE_MCS\|HAVE_GI, flags = BW_20 (\| SGI), mcs = N. The default stays **legacy 12**. **Action frames are always legacy 12**, as iPhones send theirs. mac80211 turns RATE/MCS into `IEEE80211_TX_CTRL_RATE_INJECT`; "whether the driver uses it, only throughput tells". Field data: 6 Mbit/s fixed lost Discover tails that 12 Mbit/s lost (woodinferno, mt7921 with no antennas). |
| 02 awdl-addr | `-m MAC` | Overrides the AWDL address (and awdl0's MAC) instead of using the mon vif MAC. For chips that send injected frames from the station MAC anyway (MT7925): a monitor vif that carried the station MAC **broke the station when deleted**. |
| 03 fixed-radio | `-F` | The radio sits on `-c` permanently. **(a)** Multicast may go in **any slot**: normally only slots **0 and 10** (Apple's multicast slots), with the multicast guard replaced by the unicast guard. An iPhone advertising `149,149,149,0,0,0,0,0,6,149,149,0,...` is on ch6 only in slot 8, and never heard us until it moved slot 0. **(b)** `awdl_same_channel_as_peer` compares against our master channel in every slot, so unicast is sent whenever the *peer's* sequence says it is on our channel. **(c)** The advertised sequence becomes the union over all valid peers that have a sequence and a sync offset, use the same encoding, and are phase-aligned within ±16 TU (phase taken mod 1024 TU). Bytes are copied only from each peer's own sequence. Following an idle master iPad (ch6 in slots 0 and 8) gave **27 kB/s**; following the transferring phone (`149x8, 6,6,149,6,6,6,149,149`) gave **150 kB/s**. |
| 04 election-metric | `-E` | Upstream ranks masters by counter, then metric. **Apple ranks by metric, then counter**: a master with metric 510 / counter 3214 yielded to one with 522 / 393, and an iPhone left that master for itself as soon as its metric went from 65 to 518. Counter-first kept OWL in an orphaned sync tree, and the phone never sent it a frame. |
| 05 fixed-radio-noack | (with `-F`) | Unicast data gets radiotap `TX_FLAGS = IEEE80211_RADIOTAP_F_TX_NOACK`. Alignment: with RATE (u8) present, pad 1 byte before the u16 TX_FLAGS; with MCS, TX_FLAGS (bit 15) comes before MCS (bit 19). Reason: the AX201 retried a missed unicast **15 times over 75-140 ms**, blocking the queue after the peer had left, so transfers stalled on "Waiting". A lost frame then costs a TCP retransmit instead. In mt76, `IEEE80211_TX_CTL_NO_ACK` maps to `MT_TXD3_NO_ACK`; the default is REM_TX_COUNT=15. |
| 06 fixed-radio-pin | (with `-F -S pin`) | `pin` pins the `-c` channel, not the peer's favourite, in all 16 slots. The intersection trails the phone by a tick while the phone "makes room" for us. |
| 07 intel-reliability | – | **(a)** `pcap_inject` returning EAGAIN or ENOBUFS gives TX_AGAIN: keep the frame and retry in 0.5 ms through the event loop. The slot gate is re-checked, and the Ethernet header is not stripped so the buffer can be resubmitted. Before this, an Intel card dropped **8383 frames** in a 34 MB receive, TCP ACKs among them, and uploads died near the end. **(b)** Peer timeout raised from 2 s to **5 s**. **(c)** **Received data refreshes `peer->last_update`**; before, only action frames did. An iPhone mid-transfer, on our channel one slot in 16, went 2 s without action frames, and OWL dropped it with "Drop frame to non-peer" (1405 times). |
| 08 amsdu-bound | – | An A-MSDU with more than `MAX_NUM_AMPDU` (16) subframes wrote past a stack array. Fix: pass `out_end` and return `RX_TOO_MANY_FRAMES`; free partial conversions. In Rust: bounds and iterators, no fixed array. |
| 09 action-zero-padding | – | **iPhones end some MIFs with zero padding** (8 zeros seen). The TLV loop read them as empty TLVs until fewer than 3 bytes were left, then refused the frame, so the peer was **never validated**. Fix: a trailing tail of fewer than 3 bytes that is all zeros is stripped; a nonzero or truncated TLV is still an error. #38: this allowed **direct ch44 discovery** with an iPad. |
| 10 addba-accept | `-A` | Recognise an ADDBA Request (category 3 BACK, action 0) addressed to us: token = body[2], params = LE16 body[3..5], timeout = LE16 body[5..7], SSN = body[7..9]. Reply with an action frame (FC 0xd0 0x00, DA = peer, SA = self, BSSID = AWDL): `03 01 token 00 00 params(LE) timeout(LE)`. Params: bit0 A-MSDU = 0, bit1 immediate = 1, TID = requested bits 2-5, buffer size = bits 6-15 (0 or >64 becomes 64). Unanswered, an iPhone asked **61 times** and sent all 21,941 frames singly. MT7612U: 30 MB in **14 s vs 47 s**. On by default only for mt76x2u (`/etc/omdrop/aggregation`). Who sends the **Block Ack** itself? The hardware: the active monitor or station with our MAC. PR #59 adds a **reorder buffer** (per peer/TID from the SSN, window ≤64, flush on gap fill, window move, BAR, or 100 ms timeout; mod 4096; DELBA ends it): in-order TCP went from 40% to 87%, ACKs injected from 12,250 to 7,020. PR #62: `-A` cut retries from 5.2% to 0.2% on MT7921. |
| 11 rx-flags-init | – | `uint8_t flags = 0`: frames without a radiotap Flags field were judged on stack garbage, and 0xff reads as "bad FCS". Our own MIFs (rate-only radiotap) have no Flags. In Rust: treat an absent field as 0. |

**Pending upstream proposals worth knowing (not merged):**
* **PR #62 `owl-14-wide -V`:** the HT Capabilities TLV is 9 bytes as an iPad sends it (2SS),
  with a **VHT Capabilities element after it in MIFs**, and 5 GHz tuned to **80 MHz**. The iPad
  then sends VHT MCS9 2SS 80 MHz.
* **PR #62 `owl-15`:** `-R mcsN[-20|-40][-lgi|-sgi]`.
* **#38 / PR #62:** HT Capabilities TLV in **PSFs as well as MIFs** (ARPA stays MIF-only). iPadOS
  27 PSFs contain it; a Galaxy S24 Quick-Share AWDL daemon logged "AF didn't have required TLVs"
  without it.
* **PR #64 (claims, unreviewed):**
  * OWL's self metric is a fixed 60, while Apple devices draw 505-536 when they activate. The PR
    proposes `-M 550` and a counter that increments every 3.14 s. Woodinferno found that iPhone
    and Mac **ignored** us claiming metric 550 / counter 999999.
  * Sync Params `master_channel` and `next_aw_channel` are encoded with the wrong channel
    encoding.
  * Data Path State flags: REALTIME 0x2000, DUALBAND 0x0020, FOLLOW_CHAN_SEQ 0x0080, social-map
    ext 0x0008.
  * pcap buffer of 8 MB instead of 2 MB.
  * The HT capability SGI20 bit (0x0020) and 2SS RX MCS 8-15.

---------------------------------------------------------------------------------------------------

## 3. Channels: why only 6/44/149, and what ch36 means for us

* **AWDL schedule.**
  * An AW is 16 TU. A slot (extended AW) is 64 TU, about 65.5 ms.
  * The channel sequence has **16 slots, a period of 1024 TU ≈ 1.05 s**.
  * Slots **0 and 10** are the multicast slots (MIF/PSF and mDNS).
  * Slot **8 is consistently ch6** (the 2.4 GHz social channel).
  * The 5 GHz social channel is **149**, or **44** where 149 is unavailable.
  * The device's **infrastructure channel** also takes many slots: up to 8 of 16. Slot 0 is the
    infra channel when idle, and becomes 149 when the share sheet is open.
* **Observed sequences:**
  * idle iPhone on 157 infra: `157,157,149,0,0,0,0,157,6,157,149,157,0,0,0,157`;
  * share sheet open: `149,149,149,0,0,0,0,0,6,149,149,0,...`;
  * transferring, infra on 40: `149x4,40x4,6,6,149,6,40x4`;
  * iPad on ch36 Wi-Fi: `44,44,149,44,44,44,36,36,6,44,44,44,44,44,36,36`;
  * device on the same ch44 AP as the PC: `149x4,44x4,6,44,149,44x5` (44 in 10/16).
* **Consequences.**
  * We are only useful in slots where the phone is on our channel.
  * ch6 gets **1-2 of 16 slots** until an exchange is under way, so discovery and TLS stall and
    transfers are 10-27x slower.
  * Shared mode works best when the phone's infra channel equals ours. It then gets about half the
    slots, which is why the MT7922 shared/ch44 results are good.
* **kmod-midori's observations:**
  * discovery and sync worked only on 6 at first (before the zero-padding fix);
  * after negotiation you can hop to the 5 GHz channels the phone advertises (about 3x faster);
  * pinning to the phone's slot-0 channel improved discovery a lot;
  * per-transfer hops 6/20 to 149/80 committed in 10-12 ms on mt7921 (station plus active-monitor
    layout).
* **Our AP on ch36/80 (Huawei B818).**
  * Shared mode as omdrop-owl implements it is impossible on 36: `start` exits 3.
  * The mt7921 firmware's monitor/sniffer channel **is bound to the BSS channel**. With the
    station on 2417, a monitor requested on 5745 still captured 593/593 frames on 2417. So we
    cannot listen on 44 while associated on 36, even though 44 is inside the 80 MHz block.
  * Paths:
    * **(a) Move the AP's primary to 44** (still 36-48/80) or to 149 (13 dBm in IT), then use
      shared mode. This needs the mt76 rate patch for unicast on 5 GHz.
    * **(b) Active-monitor exclusive on 44**: Wi-Fi is down during the window, and the patch is
      needed.
    * **(c) A P2P-GO second channel** (jedbillyb/mt7921-dual-channel, stock kernel):
      * the combination `#{P2P-GO}<=1, #channels<=2` lets the firmware time-slice;
      * create `go0` (`__p2pgo`) and run **hostapd patched to keep the P2P_GO iftype**
        (`HOSTAPD_P2P_GO=1`) on 44 or 149;
      * create mon0 and give it the **GO's MAC**, because `ieee80211_monitor_start_xmit()` picks
        the chanctx of the non-monitor vif whose MAC equals addr2;
      * mt76 runtime-pm off.
      * Measured: RX and TX on 149 while associated on 52, and an iPhone answering ping6. The
        uplink went from about 2.6 ms to about 40 ms median latency (280-590 ms max), with 0% loss.
      * `iw` shows no channel for mon0; only a radiotap capture tells the truth.
    * **(d) Rely on the phone adding 36** to its sequence when it is on the same AP. This was an
      unmeasured local experiment (#38), "not evidence for ch36 support". AWDL discovery still
      needs social-channel presence.
    * Following 36+44 vs pinning 44: equal (#38). Pinning is simpler.
* A phone-side gate (woodinferno, from sharingd logs): the iPhone resolves `_airdrop._tcp`, then its
  own path evaluator says **"unsatisfied (No network route)" on awdl0**, so Discover times out and
  is retried every 5 s. Admission correlated with the time since the phone rebooted, in exclusive
  mode on a channel that is not the phone's infra channel. shaecodes, in **shared mode on the
  phone's infra channel (44)**, had 26 uploads over 25 h with no reboots. This is one more reason to
  prefer being on the phone's infra or primary channel.

---------------------------------------------------------------------------------------------------

## 4. mt7921e specifics

### 4.1 Status in omdrop-owl
* The profile line is
  `mt7921e untested active-monitor station no no`: no IBSS, ch6 only, both directions with an
  iPhone, "not reliably". README "Known issues": "`mt7921e` sends injected unicast at 1 Mbit/s
  whatever rate is asked for, and not at all on 5 GHz, so discovery and the handshake often stall.
  Close and reopen the share sheet."
* At 1 Mbit/s a full 1500-byte frame takes about 12 ms of airtime. That fills most of the single
  65 ms slot in 16 that the phone spends on ch6.

### 4.2 Issue #14 timeline (all mt7921e)
* **verusferro** (MT7921 14c3:7961, CachyOS 7.2.9, regdomain TR):
  * exclusive mode with IBSS gave `-95`;
  * the active-monitor fork worked both ways on ch6: a small image in 13 s, start 2.26 s, stop
    0.18 s.
* **t4t5** (MT7922 0616):
  * shared mode: "the station deauthenticates when the monitor interface comes up, and TCP never
    completes", with either mon_mac;
  * active monitor on ch6 worked 1 time in 4;
  * on ch44, 0/132 unicast frames aired; on ch6, 115/121 aired;
  * multicast airs at 6 Mbit/s, but **iOS drops data sent group-addressed**;
  * he pointed to `mt76_connac2_mac_write_txwi` ignoring RATE_INJECT.
* **cbspears** (MT7921, Ubuntu kernel 7.0, AP on 149, stock driver, shared profile `*`):
  * injection while associated was fine (120/120 pings, about 2,900 action frames);
  * the phone was heard about 5 s and then lost; no unicast;
  * the forced reconnect hit AP auth denial (status 30) and fell back to 2.4 GHz;
  * the station did **not** show the MT7925's post-teardown drop, so `reconnect no` is plausible.
* **Loong0x00** (MT7927, mt7925e DKMS):
  * TXWI tracing: injected unicast **only airs to a peer that went through full association**
    (a real wcid);
  * a reserved wcid (19) or the global wcid (0) gave 0 on air;
  * even broadcast Action frames do not air there;
  * forcing IBSS made the firmware reject BSS_INFO with 0xC00000BB.
  * mt7921 is better: broadcast airs.
* **shaecodes-wilde**: the rate patch (§4.4).
  * MT7922 7922, ch44 active monitor: 3.4 MB in about 6 s.
  * Then shared mode on ch44: 43 MB in 30 s, 117 MB in 96 s.
  * t4t5 confirmed on 0616, in both active-monitor and shared mode.
* **verusferro** confirmed `2a2f085d` unmodified on 14c3:7961, ch44 active monitor with an iPad:
  * iPad to PC: 30.4 MB in 20.8 s (1.7 MB/s);
  * PC to iPad: 8.2 MB in 17.9 s (0.46 MB/s).
  * He then wrote the **HT extension** (§4.5).
* **woodinferno** (MT7921K 14c3:0608 with the fix, no antennas): the phone-side "No network route"
  gate (§3), and that a lone monitor never retunes on 7.2.9.

### 4.3 Where the rate is chosen (mainline mt76)
* `mt7921/pci_mac.c: mt7921e_tx_prepare_skb()` runs these steps in order:
  1. `wcid = wcid ?: &dev->mt76.global_wcid`;
  2. `mt76_token_consume`;
  3. `pid = mt76_tx_status_skb_add(mdev, wcid, skb)`;
  4. `mt76_connac2_mac_write_txwi(...)`;
  5. `mt76_connac_write_hw_txp`.
* `tx.c: mt76_tx_status_skb_add()` starts with `memset(mt76_tx_skb_cb(skb), 0, ...)`. That cb lives
  in `info->status.status_driver_data`, which **aliases `info->control.flags`** in the
  `ieee80211_tx_info` union. **Any RATE_INJECT test placed after this line sees 0.**
* `mt76_connac_mac.c: mt76_connac2_mac_write_txwi_80211()` sets `MT_TXD2_FIX_RATE` only when
  `!ieee80211_is_data(fc) || multicast || info->flags & IEEE80211_TX_CTL_USE_MINRATE`.
  * **Unicast data therefore gets no fixed rate.** The firmware uses the wcid's WTBL rate. For an
    injected frame the wcid is the global or reserved one, with no rate table, so it uses its
    default of **CCK 1M**. That is illegal on 5 GHz, and the frame is dropped by the firmware.
* When FIX_RATE is set, `mt76_connac2_mac_tx_rate_val()` handles connac2 with
  `rateidx = ffs(conf->basic_rates)-1`, i.e. a legacy rate. Then:
  * 5 GHz adds offset 4 into `mt76_rates[]`, so `rateidx<0` becomes 0 + 4 = OFDM 6M. That is why
    multicast airs at 6 Mbit/s.
  * The write sets `MT_TXD6_FIXED_BW | FIELD_PREP(MT_TXD6_TX_RATE, rate)`, `MT_TXD2_HTC_VLD`,
    `MT_TXD3_BA_DISABLE`, and clears `MT_TXD7_HW_AMSDU`.
  * openwrt/mt76 2026-10-06 adds "connac2: fill TXD spe_idx for connac2 fixed-rate frames".
* NO_ACK: `IEEE80211_TX_CTL_NO_ACK` becomes `MT_TXD3_NO_ACK`, so radiotap NOACK works on mt76.
* `MT_TX_RATE` encoding: `MODE` (CCK 0, OFDM 1, HT 2, ...), `IDX`, `NSS`. Legacy `hw_value` in
  `mt76_rates[]` is `(mode<<8)|idx`; for example, OFDM 6M is `(1<<8)|11`.

### 4.4 The legacy-rate fix (shaecodes-wilde `02b6c9a1` + `2a2f085d`, `mt7921/pci_mac.c`)

```c
	bool use_minrate = info->flags & IEEE80211_TX_CTL_USE_MINRATE;
	bool injected_legacy = false;
	u16 injected_rate = 0;
	...
	/* Save the rate before TX status bookkeeping clears control.flags. */
	if ((mt76_chip(mdev) == 0x7961 || is_mt7920(mdev) || is_mt7922(mdev)) &&
	    !(info->flags & IEEE80211_TX_CTL_HW_80211_ENCAP) &&
	    (info->flags & IEEE80211_TX_CTL_INJECTED) &&
	    (info->control.flags & IEEE80211_TX_CTRL_RATE_INJECT) &&
	    !(info->control.rates[0].flags & (IEEE80211_TX_RC_MCS | IEEE80211_TX_RC_VHT_MCS)) &&
	    info->band < NUM_NL80211_BANDS) {
		struct ieee80211_supported_band *sband = mdev->phy.hw->wiphy->bands[info->band];
		int idx = info->control.rates[0].idx;
		if (sband && idx >= 0 && idx < sband->n_bitrates) {
			struct ieee80211_rate *r = &sband->bitrates[idx];
			u16 hw_rate = (info->control.rates[0].flags & IEEE80211_TX_RC_USE_SHORT_PREAMBLE)
				      ? r->hw_value_short : r->hw_value;
			injected_rate = FIELD_PREP(MT_TX_RATE_MODE, hw_rate >> 8) |
					FIELD_PREP(MT_TX_RATE_IDX, hw_rate & 0xff);
			injected_legacy = true;
		}
	}
	pid = mt76_tx_status_skb_add(mdev, wcid, tx_info->skb);
	if (injected_legacy)                       /* force the common FIX_RATE path (BW, HTC, BA_DISABLE) */
		info->flags |= IEEE80211_TX_CTL_USE_MINRATE;
	mt76_connac2_mac_write_txwi(mdev, txwi_ptr, tx_info->skb, wcid, key, pid, qid, 0);
	if (injected_legacy) {
		__le32 *txd = txwi_ptr;
		if (!use_minrate)
			info->flags &= ~IEEE80211_TX_CTL_USE_MINRATE;
		txd[6] &= ~cpu_to_le32(MT_TXD6_TX_RATE);   /* replace basic-rate with the injected one */
		txd[6] |= cpu_to_le32(FIELD_PREP(MT_TXD6_TX_RATE, injected_rate));
	}
```

* Subject: `wifi: mt76: mt7921: honor injected legacy rates on MT7922`, with
  `Fixes: 688088728bd3`. It has not been submitted upstream yet.
* An alternative used in #38: keep RATE_INJECT in a spare mt76 tx-cb flag before the memset, and
  honour it in the common connac2 TXWI path. TXWI tracing showed FIX_RATE and 12M on 2.4 and
  5 GHz.
* For lindrop: build `mt7921e.ko` out of tree (DKMS) from the running kernel's
  `drivers/net/wireless/mediatek/mt76` with this hunk. Only `mt7921e` needs rebuilding.
  `mt7921-common`, `mt792x-lib` and `mt76-connac-lib` stay stock.

### 4.5 HT/MCS extension (verusferro, on top of 2a2f085d, same function)

* The VHT_MCS exclusion stays, but an `IEEE80211_TX_RC_MCS` branch is added. It accepts the frame
  only when all of these hold:
  * 5 GHz;
  * idx 0-15 and HT supported;
  * idx ≥8 only if the antenna mask has 2 or more chains;
  * no flags other than `MCS|40_MHZ_WIDTH|SHORT_GI`;
  * no STBC;
  * a unicast data frame.
* It then builds `hw_rate = MODE(MT_PHY_TYPE_HT) | IDX(idx) | NSS(idx>>3)`, and
  `txd6 = TX_RATE(hw_rate) | BW(!!40MHz) | SGI(!!sgi) | (LDPC if IEEE80211_TX_CTL_LDPC)`.
* After write_txwi it clears `TX_RATE|BW|SGI|LDPC` in txd[6] and ORs in `txd6`.
* Measured PC to iPad, 30.4 MB on ch44: legacy 54 took 19.8 s, mcs7 13.4 s, mcs15-40-sgi 11.5 s.
* Suggested subject: `wifi: mt76: mt7921: honor injected HT rates on MT7920/MT7921/MT7922`.

### 4.6 Other mt7921 facts
* There are **no IBSS** interface modes. `iw ... type ibss` gives -95.
* Monitor RX is **dead while runtime-pm is on**: turn debugfs `mt76/runtime-pm` and `deep-sleep`
  to 0.
* An in-place iftype switch never retunes the radio. A lone (active) monitor does not retune
  either. Use the station as a passive monitor plus the active monitor, and tune both.
* While associated, the firmware sniffer stays on the BSS channel (four gates; the last is in
  firmware, `WIFI_RAM_CODE_MT7961_1.bin` has no sniffer channel manager).
* Only **P2P-GO** gets a second channel context (`#channels<=2`); a plain AP gets
  `#channels<=1`. hostapd forces the AP iftype, which gives EBUSY, hence the one-line hostapd patch.
* PR #55: an opt-in "GO transmit-anchor" mode. A temporary Wi-Fi Direct group on ch6 through
  wpa_supplicant, with passive and active monitors sharing the GO's MAC and `owl -N -S intersect -F -E`.
  It received from a Mac and iPhone on an MT7922.
* The PR #42 hidden-AP fallback (`mt7921e untested exclusive station no yes` plus hostapd) was
  suggested for mt7921e but has no reported result.
* Shared-mode mt7921 data points:
  * MT7922 shared with the fix: `mt7921e shared station yes yes` (reconnect yes, pm off),
    `owl -i mon0 -c 44 -N -S intersect -v`, legacy 12, more than 25 h stable.
  * MT7922 shared stock: the station deauthenticated when mon0 came up (t4t5).

---------------------------------------------------------------------------------------------------

## 5. Performance numbers and limits

| Setup | Result |
|---|---|
| MT7925 shared | PC→phone ~490 kB/s; phone→PC 1.3 MB/s. Injected frames leave **at most one every ~2.5 ms**. |
| MT7612U active monitor, ch149 | 3 MB photo ~5 s; 30 MB upload **14 s with -A** vs 47 s without; ch6: 1.1 MB in 27 s, multi-photo stalls. |
| Intel AX201 exclusive ch6 | 2.4 MB photo 10-17 s (17 s in, 10.5 s out). |
| RTL8822BE exclusive ch6 | 6.4 MB in 110 s. |
| MT7921 stock, active monitor ch6 | small image 13 s; sends 13-20 s; works about 1 time in 4 (MT7922). |
| MT7921 + legacy fix, active monitor ch44 | iPad→PC 1.7 MB/s (59 MB/37 s; 30.4 MB/20.8 s); PC→iPad 0.40-0.46 MB/s. |
| MT7922 + legacy fix, **shared ch44**, legacy 12 | 43.2 MB/30.2 s (1.43 MB/s); 117 MB/96 s (1.22); 92 MB/82 s; small files 0.3-0.9 MB/s. |
| MT7921 + HT fix, `-F -E -S pin -A -V -R mcs15-40-sgi`, ch44 | iPad→PC 30.4 MB in **3.0-3.4 s (9-10 MB/s)**; 71.3 MB 4.96-6.49 s (**11-14 MB/s**; the iPad sends VHT MCS9 2SS 80 MHz); PC→iPad 10.2-11.6 s (2.6-3.0 MB/s). Galaxy S24: 2.6-3.0 MB/s. |
| owl_rs (kmod-midori Rust port), hop 6→149/80 per transfer | ~3x vs fixed ch6; ~2 Mbps on 149 early; later 0.55-0.65 MB/s with no antennas; 7.25 MB in 10-13.5 s. The claim is 1-2 MB/s both ways. |

**What limits throughput, in order of impact:**
1. **The rate of injected unicast.** On stock mt7921 that is 1 Mbit/s CCK on 2.4 GHz and nothing on
   5 GHz. With the fix, legacy 12 to 54, then HT MCS15/40. PC-to-phone is bounded by our injected
   rate, because injected frames get `BA_DISABLE` and no A-MPDU. Phone-to-PC is bounded by the
   phone's rate, and the phone only goes fast if we advertise HT/VHT caps and accept ADDBA.
2. **Slot share.** The number of the phone's 16 slots that land on our channel: ch6 gets 1-2,
   the 5 GHz social channel 3-5, the infra channel about 8-10. Use `-F` (send whenever the peer's
   sequence says so) and advertise the union or pin.
3. **Retries blocking the queue.** Use NOACK under `-F`, and EAGAIN requeue instead of drop.
4. **Peer eviction mid-transfer.** Data must refresh liveness; use a 5 s timeout.
5. **Aggregation.** Accept ADDBA (`-A`) and reorder (#59), or TCP sees fake losses.
6. **Injection pacing in the driver.** The MT7925 manages one frame every 2.5 ms.
7. **Phone-side admission** ("No network route") and TLS identity. omdrop needs a self-signed
   identity set up with `omdrop setup`; the receiver had to close the connection after a completed
   upload, and wait 3 s for a missing closing chunk.

**Lessons for the Rust AWDL implementation:**
* data rate configurable, default 6 or 12 legacy;
* action frames at 12 legacy;
* radiotap TX_FLAGS NOACK with correct alignment;
* election by metric, then counter;
* tolerate MIF zero padding;
* default radiotap Flags to 0;
* bounded A-MSDU parsing;
* ADDBA response plus a reorder buffer;
* union-of-peers sequence under a fixed radio;
* multicast in any slot, repeated across 16 slots;
* HT caps TLV (9 bytes) in PSF and MIF, with a VHT element for 80 MHz;
* AWDL BSSID 00:25:00:ff:94:73;
* awdl0 TAP with MACAddressPolicy=none, NM unmanaged, DAD off.
