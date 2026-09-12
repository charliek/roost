//! Binary-fidelity guard for `tab.write.data`. The acceptance
//! criterion in the refactor plan calls out a `0x00..0xff` round-trip
//! test; this is it. If base64 codec or framing ever silently
//! mangles a byte, this catches it.

use roost_ipc::messages::TabWriteParams;

#[test]
fn full_byte_range_round_trips_through_base64() {
    let mut data = Vec::with_capacity(256);
    for b in 0u8..=255u8 {
        data.push(b);
    }
    let p = TabWriteParams {
        tab_id: 1,
        data: data.clone(),
    };
    let json = serde_json::to_string(&p).expect("serialize");
    let back: TabWriteParams = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.data, data, "byte fidelity broke");
}

#[test]
fn nul_bytes_round_trip() {
    let data = vec![0u8; 1024];
    let p = TabWriteParams { tab_id: 1, data };
    let json = serde_json::to_string(&p).unwrap();
    let back: TabWriteParams = serde_json::from_str(&json).unwrap();
    assert_eq!(back.data.len(), 1024);
    assert!(back.data.iter().all(|&b| b == 0));
}

#[test]
fn high_bit_bytes_round_trip() {
    let data = (0..16384).map(|i| (i % 256) as u8).collect::<Vec<u8>>();
    let p = TabWriteParams {
        tab_id: 1,
        data: data.clone(),
    };
    let json = serde_json::to_string(&p).unwrap();
    let back: TabWriteParams = serde_json::from_str(&json).unwrap();
    assert_eq!(back.data, data);
}
