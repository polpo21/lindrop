# lindrop

**AirDrop tra iPhone e Linux, scritto da zero in Rust.**
*AirDrop between iPhone and Linux, written from scratch in Rust. Work in progress.*

> Stato: fase 1. Prima prova con un iPhone vero (iOS 27, 17/10/2026): l'iPhone
> trova il PC, chiede il permesso e inizia a inviare; col driver `mt7921e` di
> serie il collegamento perde troppi pacchetti e il trasferimento si ferma.
> La correzione del driver è in `driver/` (da provare).

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
| BLE | il PC sveglia l'iPhone quando vuole inviargli qualcosa | `ble` ✅ |
| AWDL | il "Wi-Fi diretto" di Apple: crea l'interfaccia `awdl0` tra PC e telefono | fase 1: OWL esterno; fase 2: `awdl` in Rust |
| mDNS | il servizio `_airdrop._tcp` annunciato e cercato su `awdl0` | `mdns` ✅ |
| HTTPS | `/Discover`, `/Ask`, `/Upload` con messaggi plist | `airdrop`, `send` ✅ |
| Archivio | cpio dentro dvzip (iOS 18+) o gzip (vecchi) | `archive`, `dvzip` ✅ |

Dettagli in [docs/ARCHITETTURA.md](docs/ARCHITETTURA.md); quello che abbiamo
imparato dai progetti esistenti è in [docs/ricerca/](docs/ricerca/).

## Installazione

```sh
git clone https://github.com/polpo21/lindrop && cd lindrop
./install.sh
```

Installa `/usr/local/bin/lindrop`, compila OWL (AWDL, fase 1) e attiva il
servizio `lindrop-radio`. Poi:

```sh
lindrop ricevi          # visibile all'iPhone per 10 minuti
lindrop invia foto.jpg  # invia a un iPhone vicino
```

Configurazione facoltativa in `~/.config/lindrop/config.toml`:

```toml
nome = "PC di Paolo"   # come ti vede l'iPhone
cartella = "/home/io/Scaricati"
modalita = "automatica" # o "condivisa" (Wi-Fi acceso) / "esclusiva" (Wi-Fi spento)
canale = 0             # 0 = quello del Wi-Fi, oppure 6, 36, 44, 149
velocita = ""          # "" = mcs7 + aggregazione col nostro driver; "base", "mcs0".."mcs15", "6".."54"
paese = "IT"           # dominio regolatorio mentre il Wi-Fi è spento
chiedi = true          # notifica Accetta/Rifiuta
minuti = 10
```

## Hardware

### Driver MT7921 (consigliato)

Il driver `mt7921e` di serie manda i pacchetti iniettati a 1 Mbit/s CCK, che a
5 GHz il firmware scarta: l'iPhone ci sente ma non riceve le nostre risposte.
`scripts/driver.sh` ricompila solo `mt7921e.ko` con `driver/mt7921e-rate.patch`
per il kernel in uso e lo installa in `/lib/modules/<kernel>/updates/lindrop/`
(`--rimuovi` torna a quello di serie). Dopo un aggiornamento del kernel va
rilanciato. Non funziona se il kernel accetta solo moduli firmati.

Il primo obiettivo è la scheda **MediaTek MT7921** (driver `mt7921e`). Serve una
scheda che supporti il monitor mode con iniezione di pacchetti.

## Limiti che non dipendono da noi

- Sull'iPhone AirDrop deve essere su **"Tutti per 10 minuti"**: "Solo
  contatti" richiede un'identità firmata da Apple.
- Apple non supporta niente di tutto questo: un aggiornamento di iOS può
  romperlo.

## Licenza

GPL-3.0-or-later. Non affiliato ad Apple. AirDrop è un marchio di Apple Inc.
