//! Pure USB configuration-descriptor parsing for HID interfaces.
//!
//! WHY THIS IS A SEPARATE, DEPENDENCY-FREE MODULE. `xhci.rs` used to stop at the
//! *first* HID interface with an interrupt-IN endpoint and configure only that
//! one. A real combo keyboard (e.g. the field R82HE) exposes several HID
//! interfaces — a boot keyboard on interface 0 and consumer/system controls on
//! later ones — and a driver that binds only the first still works for typing,
//! but it is one parser mistake away from binding a report-protocol vendor
//! collection as if it were a boot keyboard. This module walks the entire
//! configuration descriptor and returns every HID interface, with the endpoint
//! data the xHCI layer needs; the policy of which interfaces to bind lives in
//! `usb_hid_proto`.
//!
//! It is `alloc`-only and self-contained, so the host property tests include the
//! exact source the kernel compiles (`host-tests/src/lib.rs`) and can feed it
//! malformed/truncated descriptors that no real device would send.

use alloc::vec::Vec;

/// One HID interface and its first interrupt-IN endpoint.
///
/// Only the default alternate setting (`bAlternateSetting == 0`) is returned:
/// the driver never selects an alternate setting, so collecting alt-1 endpoints
/// would configure endpoints the device is not currently exposing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HidIface {
    /// `bConfigurationValue` from the configuration descriptor (what
    /// SET_CONFIGURATION takes).
    pub config_value: u8,
    /// `bInterfaceNumber`.
    pub interface: u8,
    /// `bInterfaceSubClass` (1 = boot interface).
    pub subclass: u8,
    /// `bInterfaceProtocol` (0 = none, 1 = keyboard, 2 = mouse).
    pub protocol: u8,
    /// Endpoint address; bit 7 set means IN.
    pub ep_addr: u8,
    /// `wMaxPacketSize` exactly as the endpoint descriptor carries it (bits
    /// 11-12 of a high-speed value are the multiplier, not the byte size).
    pub max_packet: u16,
    /// `bInterval` (polling interval; xHCI interprets it by speed).
    pub interval: u8,
}

/// Descriptor type values used here (USB 2.0 §9.6).
const DESC_INTERFACE: u8 = 4;
const DESC_ENDPOINT: u8 = 5;

/// Walk the configuration descriptor and return every HID (class 3) interface
/// that has an interrupt-IN endpoint.
///
/// The parser is deliberately total: a truncated or lying descriptor can never
/// panic or loop (every step is bounded by the slice), it only yields fewer
/// interfaces. A HID interface whose endpoint is not interrupt-IN is skipped —
/// a HID interrupt-OUT-only interface cannot produce input.
pub fn parse_hid_interfaces(conf: &[u8]) -> Vec<HidIface> {
    let mut out = Vec::new();
    let Some(&config_value) = conf.get(5) else {
        return out;
    };
    let mut i = 0usize;
    // (bInterfaceNumber, bInterfaceSubClass, bInterfaceProtocol) of the HID
    // interface the following endpoint descriptor belongs to, when it is a HID
    // interface in its default alternate setting.
    let mut current: Option<(u8, u8, u8)> = None;
    // Only the first interrupt-IN endpoint of an interface is used; a second
    // one belongs to a layout this driver does not poll.
    let mut have_ep = false;
    while i + 2 <= conf.len() {
        let len = conf[i] as usize;
        if len < 2 || i + len > conf.len() {
            break;
        }
        let kind = conf[i + 1];
        if kind == DESC_INTERFACE && len >= 9 {
            let number = conf[i + 2];
            let alternate = conf[i + 3];
            let class = conf[i + 5];
            let subclass = conf[i + 6];
            let protocol = conf[i + 7];
            current = if class == 3 && alternate == 0 {
                Some((number, subclass, protocol))
            } else {
                None
            };
            have_ep = false;
        } else if kind == DESC_ENDPOINT && len >= 7 {
            if let Some((number, subclass, protocol)) = current {
                let addr = conf[i + 2];
                let attrs = conf[i + 3];
                // Transfer type bits 0-1: 3 = interrupt; bit 7 = IN direction.
                if !have_ep && attrs & 0x03 == 3 && addr & 0x80 != 0 {
                    have_ep = true;
                    out.push(HidIface {
                        config_value,
                        interface: number,
                        subclass,
                        protocol,
                        ep_addr: addr,
                        max_packet: u16::from_le_bytes([conf[i + 4], conf[i + 5]]),
                        interval: conf[i + 6],
                    });
                }
            }
        }
        // Unknown/HID class descriptors are skipped by their own bLength, which
        // is what keeps the walk aligned.
        i += len;
    }
    out
}
