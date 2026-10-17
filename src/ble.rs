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

/// Type 0x05, length 18: zero prefix, version 1, four zero hashes, trailer.
const ADVERT: [u8; 20] = [0x05, 0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0];

struct Advert;

#[zbus::interface(name = "org.bluez.LEAdvertisement1")]
impl Advert {
    fn release(&self) {
        tracing::debug!("BlueZ ha rilasciato l'annuncio BLE");
    }

    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        "broadcast".into()
    }

    #[zbus(property)]
    fn manufacturer_data(&self) -> HashMap<u16, OwnedValue> {
        let v = OwnedValue::try_from(Value::from(ADVERT.to_vec())).expect("byte array");
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
        conn.object_server().at(PATH, Advert)?;
        let opts: HashMap<&str, Value> = HashMap::new();
        conn.call_method(
            Some("org.bluez"),
            ADAPTER,
            Some("org.bluez.LEAdvertisingManager1"),
            "RegisterAdvertisement",
            &(ObjectPath::try_from(PATH)?, opts),
        )
        .context("BlueZ ha rifiutato l'annuncio BLE")?;
        tracing::info!("annuncio BLE AirDrop attivo");
        Ok(Self { conn, powered_by_us: !powered })
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
        assert_eq!(super::ADVERT[10], 1);
    }
}
