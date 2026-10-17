# lindrop

**AirDrop tra iPhone e Linux, scritto da zero in Rust.**
*AirDrop between iPhone and Linux, written from scratch in Rust. Work in progress.*

> Stato: in costruzione. Ancora niente funziona end-to-end.

## Perché

Esistono già progetti che fanno parlare un PC Linux con l'AirDrop di un iPhone
([OpenDrop](https://github.com/seemoo-lab/opendrop) +
[OWL](https://github.com/seemoo-lab/owl),
[airdrop-mt7921](https://github.com/jedbillyb/airdrop-mt7921),
[omdrop-owl](https://github.com/t4t5/omdrop-owl)), ma sono un insieme di
script shell, Python con 17 patch sopra e un demone C. lindrop prende le loro
scoperte e le rifà come **un solo programma**, pensato per stare bene su un
desktop Linux normale:

- un solo binario Rust, niente Python, niente patch da riapplicare;
- niente `sudo` senza password: la parte che tocca la radio gira come servizio
  di sistema e parla con il resto tramite un socket;
- funziona con NetworkManager sia con `wpa_supplicant` sia con `iwd`;
- notifiche del desktop per accettare o rifiutare i file.

## Come funziona AirDrop (in breve)

| Strato | Cosa fa | In lindrop |
|---|---|---|
| BLE | l'iPhone annuncia che vuole condividere; il PC lo sveglia quando vuole inviare | `ble` (da fare) |
| AWDL | il "Wi-Fi diretto" di Apple: crea l'interfaccia `awdl0` tra PC e telefono | fase 1: OWL esterno; fase 2: `awdl` in Rust |
| mDNS | il servizio `_airdrop._tcp` annunciato e cercato su `awdl0` | `mdns` (da fare) |
| HTTPS | `/Discover`, `/Ask`, `/Upload` con messaggi plist | `airdrop` (da fare) |
| Archivio | i file viaggiano come cpio compresso con gzip | `archive` ✅ |

Dettagli in [docs/ARCHITETTURA.md](docs/ARCHITETTURA.md).

## Hardware

Il primo obiettivo è la scheda **MediaTek MT7921** (driver `mt7921e`). Serve una
scheda che supporti il monitor mode con iniezione di pacchetti.

## Limiti che non dipendono da noi

- Sull'iPhone AirDrop deve essere su **"Tutti per 10 minuti"**: "Solo
  contatti" richiede un'identità firmata da Apple.
- Apple non supporta niente di tutto questo: un aggiornamento di iOS può
  romperlo.

## Licenza

GPL-3.0-or-later. Non affiliato ad Apple. AirDrop è un marchio di Apple Inc.
