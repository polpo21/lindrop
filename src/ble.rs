//! The BLE "doorbell" that makes an iPhone start AWDL and announce its
//! AirDrop receiver, so we can send to it.
//!
//! An Apple Continuity advert of type 0x05 (AirDrop): an 8-byte zero prefix,
//! version 1, then four 2-byte prefixes of SHA-256 contact hashes. All zero
//! is what a sender with no Apple ID looks like; a phone set to "Everyone"
//! wakes for it. Registered through BlueZ over D-Bus, as the user; it lives
//! as long as the returned [`Wake`].

use std::collections::HashMap;

use anyhow::{Context, Result};
use zbus::blocking::Connection;
use zvariant::{ObjectPath, OwnedValue, Value};

const APPLE: u16 = 0x004c;
const PATH: &str = "/org/lindrop/airdrop";
const ADAPTER: &str = "/org/bluez/hci0";

/// Type 0x05, length 18: an 8-byte prefix, the version, four 2-byte contact
/// hashes (zero: no Apple ID), a trailer. iOS 26 senders used version 1 and
/// a zero prefix; an iOS 27 iPhone sends version 3 (seen 17/10/2026:
/// `05 12 40252267 00000000 03 <hashes> 00`).
const ADVERT: [u8; 20] = [0x05, 0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// The advert bytes: `LINDROP_BLE` (hex, starting with the 0x05 TLV; more
/// Continuity TLVs such as Nearby Info 0x10 may follow) for experiments, else
/// [`ADVERT`].
fn advert() -> Vec<u8> {
    let custom = std::env::var("LINDROP_BLE").ok().and_then(|h| {
        let b: Option<Vec<u8>> =
            (0..h.len()).step_by(2).map(|i| h.get(i..i + 2).and_then(|x| u8::from_str_radix(x, 16).ok())).collect();
        b.filter(|b| (20..=27).contains(&b.len()) && b[0] == 0x05 && b[1] == 0x12)
    });
    custom.unwrap_or_else(|| ADVERT.to_vec())
}

fn interval_ms() -> u32 {
    std::env::var("LINDROP_BLE_MS").ok().and_then(|v| v.parse().ok()).filter(|v| (20..=10240).contains(v)).unwrap_or(30)
}

struct Advert;

#[zbus::interface(name = "org.bluez.LEAdvertisement1")]
impl Advert {
    fn release(&self) {
        tracing::debug!("BlueZ ha rilasciato l'annuncio BLE");
    }

    /// Connectable: an iOS 27 iPhone on its home screen ignores a
    /// non-connectable ("broadcast") AirDrop advert, and only answers one
    /// while its share sheet is open (17/10/2026). `LINDROP_BLE_TYPE=broadcast`
    /// to compare.
    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        match std::env::var("LINDROP_BLE_TYPE").as_deref() {
            Ok("broadcast") => "broadcast".into(),
            _ => "peripheral".into(),
        }
    }

    /// Advertising interval in ms (`LINDROP_BLE_MS`, default 30). BlueZ only
    /// honours it with `Experimental = true` in main.conf; otherwise ignored.
    #[zbus(property)]
    fn min_interval(&self) -> u32 {
        interval_ms()
    }

    #[zbus(property)]
    fn max_interval(&self) -> u32 {
        interval_ms()
    }

    #[zbus(property)]
    fn manufacturer_data(&self) -> HashMap<u16, OwnedValue> {
        let v = OwnedValue::try_from(Value::from(advert())).expect("byte array");
        HashMap::from([(APPLE, v)])
    }
}

pub struct Wake {
    conn: Connection,
    /// We switched Bluetooth on, so we switch it off again.
    powered_by_us: bool,
}

impl Wake {
    pub fn start() -> Result<Self> {
        let conn = Connection::system().context("D-Bus di sistema")?;
        let props = zbus::blocking::fdo::PropertiesProxy::builder(&conn)
            .destination("org.bluez")?
            .path(ADAPTER)?
            .build()?;
        let iface = zbus::names::InterfaceName::try_from("org.bluez.Adapter1")?;
        let powered: bool = props
            .get(iface.clone(), "Powered")
            .context("nessun adattatore Bluetooth (BlueZ)")?
            .try_into()?;
        if !powered {
            props.set(iface, "Powered", Value::from(true)).context("accendo il Bluetooth")?;
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        // From here on, Drop puts Bluetooth back as it was, even on error.
        let wake = Self { conn, powered_by_us: !powered };
        wake.conn.object_server().at(PATH, Advert)?;
        let opts: HashMap<&str, Value> = HashMap::new();
        wake.conn
            .call_method(
                Some("org.bluez"),
                ADAPTER,
                Some("org.bluez.LEAdvertisingManager1"),
                "RegisterAdvertisement",
                &(ObjectPath::try_from(PATH)?, opts),
            )
            .context("BlueZ ha rifiutato l'annuncio BLE")?;
        tracing::info!("annuncio BLE AirDrop attivo: {}", crate::ident::hex(&advert()));
        Ok(wake)
    }
}

impl Drop for Wake {
    fn drop(&mut self) {
        let _ = self.conn.call_method(
            Some("org.bluez"),
            ADAPTER,
            Some("org.bluez.LEAdvertisingManager1"),
            "UnregisterAdvertisement",
            &(ObjectPath::try_from(PATH).expect("static path"),),
        );
        if self.powered_by_us {
            let props = zbus::blocking::fdo::PropertiesProxy::builder(&self.conn)
                .destination("org.bluez")
                .and_then(|b| b.path(ADAPTER))
                .and_then(|b| b.build());
            if let (Ok(p), Ok(i)) = (props, zbus::names::InterfaceName::try_from("org.bluez.Adapter1")) {
                let _ = p.set(i, "Powered", Value::from(false));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn advert_layout() {
        // 0x05 AirDrop, 18 bytes after the length byte.
        assert_eq!(super::ADVERT[1] as usize, super::ADVERT.len() - 2);
        assert_eq!(super::ADVERT[10], 3);
    }
}
