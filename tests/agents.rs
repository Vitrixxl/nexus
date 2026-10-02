//! Run inside dbus-run-session. No access to the machine's Wi-Fi or Bluetooth.
use nexus_control::backend::{BluetoothAgent, Prompts, WifiAgent};
use std::{collections::HashMap, sync::Arc, time::Duration};
use zbus::{
    blocking::{Connection, Proxy, connection::Builder},
    zvariant::{ObjectPath, OwnedValue, Value},
};

fn prompt(prompts: &Prompts) -> nexus_control::Prompt {
    for _ in 0..100 {
        if let Some(p) = prompts.current() {
            return p;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("Agent did not publish a prompt")
}
#[test]
#[ignore = "requires a private D-Bus: dbus-run-session -- cargo test --test agents -- --ignored"]
fn wifi_and_bluetooth_prompts_roundtrip() {
    let prompts = Arc::new(Prompts::default());
    let server = Builder::session()
        .unwrap()
        .name("io.github.vitrixxl.NexusTest")
        .unwrap()
        .serve_at("/wifi", WifiAgent(prompts.clone()))
        .unwrap()
        .serve_at("/bluetooth", BluetoothAgent(prompts.clone()))
        .unwrap()
        .build()
        .unwrap();
    let client = Connection::session().unwrap();
    let c = client.clone();
    let pending = std::thread::spawn(move || {
        let proxy = Proxy::new(
            &c,
            "io.github.vitrixxl.NexusTest",
            "/wifi",
            "org.freedesktop.NetworkManager.SecretAgent",
        )
        .unwrap();
        let connection = HashMap::from([
            (
                "802-11-wireless",
                HashMap::from([("ssid", Value::from(b"Home".to_vec()))]),
            ),
            (
                "802-11-wireless-security",
                HashMap::from([("key-mgmt", Value::from("wpa-psk"))]),
            ),
        ]);
        proxy.call::<_, _, HashMap<String, HashMap<String, OwnedValue>>>(
            "GetSecrets",
            &(
                connection,
                ObjectPath::try_from("/org/freedesktop/NetworkManager/Settings/1").unwrap(),
                "802-11-wireless-security",
                Vec::<String>::new(),
                // ALLOW_INTERACTION | REQUEST_NEW
                3u32,
            ),
        )
    });
    let p = prompt(&prompts);
    assert_eq!(p.fields, vec!["Passphrase"]);
    assert!(p.detail.contains("“Home” was not accepted"));
    assert!(prompts.answer(p.id + 1, HashMap::new(), true).is_err());
    prompts
        .answer(
            p.id,
            HashMap::from([("Passphrase".into(), "test-secret".into())]),
            true,
        )
        .unwrap();
    let result = pending.join().unwrap().unwrap();
    assert_eq!(
        <&str>::try_from(&result["802-11-wireless-security"]["psk"]).unwrap(),
        "test-secret"
    );
    assert!(prompts.current().is_none());
    for accepted in [false, true] {
        let c = client.clone();
        let pending = std::thread::spawn(move || {
            Proxy::new(
                &c,
                "io.github.vitrixxl.NexusTest",
                "/bluetooth",
                "org.bluez.Agent1",
            )
            .unwrap()
            .call::<_, _, ()>(
                "RequestConfirmation",
                &(
                    ObjectPath::try_from("/org/bluez/hci0/dev_TEST").unwrap(),
                    123456u32,
                ),
            )
        });
        let p = prompt(&prompts);
        assert!(p.detail.contains("123456"));
        prompts.answer(p.id, HashMap::new(), accepted).unwrap();
        let result = pending.join().unwrap();
        assert_eq!(result.is_ok(), accepted);
        if !accepted {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("org.bluez.Error.Rejected")
            );
        }
    }
    let proxy = Proxy::new(
        &client,
        "io.github.vitrixxl.NexusTest",
        "/bluetooth",
        "org.bluez.Agent1",
    )
    .unwrap();
    proxy
        .call::<_, _, ()>(
            "DisplayPasskey",
            &(
                ObjectPath::try_from("/org/bluez/hci0/dev_TEST").unwrap(),
                42u32,
                2u16,
            ),
        )
        .unwrap();
    let p = prompt(&prompts);
    assert!(p.display_only);
    assert!(p.detail.contains("000042"));
    prompts.answer(p.id, HashMap::new(), false).unwrap();
    assert!(prompts.current().is_none());
    drop(server);
}
