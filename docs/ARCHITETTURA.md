# Architettura

## Due processi, un solo binario

```
 utente                                   root (servizio di sistema)
┌──────────────────────────────┐  socket  ┌─────────────────────────────┐
│ lindrop                      │ ───────▶ │ lindrop radio               │
│  · mDNS su awdl0             │  /run/   │  · NetworkManager / iwd     │
│  · server/client HTTPS       │  lindrop │  · interfacce monitor       │
│  · archivi cpio+gzip         │  /radio  │  · debugfs mt76, reg. domain│
│  · notifiche, ~/Scaricati    │  .sock   │  · AWDL (OWL, poi Rust)     │
│  · BLE (BlueZ, D-Bus)        │          │  · ripristino del Wi-Fi     │
└──────────────────────────────┘          └─────────────────────────────┘
```

- **`lindrop radio`** è l'unica parte con privilegi. Gira come
  `lindrop-radio.service`, attivato dal socket `/run/lindrop/radio.sock`
  (permessi solo per l'utente). Accetta pochi comandi fissi (`su`, `giu`,
  `stato`) in JSON, una riga per messaggio: niente argomenti liberi, niente
  `sudo`. Il servizio ha solo `CAP_NET_ADMIN` e `CAP_NET_RAW` (più l'accesso a
  debugfs) e se il client sparisce rimette a posto il Wi-Fi da solo.
- **`lindrop`** (utente) fa tutto il resto: annuncia il servizio, riceve e invia
  i file, chiede conferma con una notifica, salva in `~/Scaricati`.

## Modalità della radio

| Modalità | Wi-Fi durante AirDrop | Come |
|---|---|---|
| `esclusiva` | spento, torna dopo | NM/iwd fermati, coppia di interfacce monitor (normale + `active`) sul canale scelto, OWL `-S verbatim` |
| `condivisa` | acceso | monitor con il MAC della scheda sul canale dell'AP (6/36/44/149): la scheda stessa conferma l'unicast dell'iPhone; OWL `-S intersect` |
| `automatica` (predefinita) | dipende | condivisa se il Wi-Fi è collegato su un canale AWDL, altrimenti esclusiva |

Con il driver di `driver/` caricato il servizio radio aggiunge a OWL `-R mcs7`
(dati a 65 Mbit/s) e `-A` (risposta ADDBA, così l'iPhone aggrega i pacchetti).
Le patch di OWL sono in `patches/`.

## Fasi

1. **Fase 1**: protocollo AirDrop in Rust (mDNS, HTTPS, archivi, notifiche) e
   servizio radio in Rust; AWDL affidato a OWL, avviato come processo esterno.
2. **Fase 2**: AWDL in Rust (`awdl`), OWL rimosso.
3. **Dopo**: velocità sulla MT7921 (il driver manda i pacchetti iniettati a
   1 Mbit/s), Wi-Fi sempre acceso, invio dal file manager.

## Moduli

| Modulo | Cosa |
|---|---|
| `archive` | cpio odc in scrittura, odc + newc in lettura, estrazione sicura |
| `ident` | nome, ID del servizio, certificato TLS autofirmato |
| `http` | HTTP/1.1 minimo (chunked, `100-continue`, keep-alive) |
| `airdrop` | messaggi plist e gestione di `/Discover`, `/Ask`, `/Upload` |
| `mdns` | responder e browser DNS-SD su `awdl0` |
| `radio` | servizio di sistema e protocollo del socket |
| `ble` | annunci BLE per svegliare l'iPhone quando inviamo |
