//! Power source and battery charge from IOKit's power-source API.

use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSCopyPowerSourcesList(blob: CFTypeRef) -> CFArrayRef;
    fn IOPSGetPowerSourceDescription(blob: CFTypeRef, ps: CFTypeRef) -> CFDictionaryRef;
    fn IOPSGetProvidingPowerSourceType(blob: CFTypeRef) -> CFStringRef;
    fn IOPSCopyExternalPowerAdapterDetails() -> CFDictionaryRef;
}

/// The power adapter's rating in watts, when one is connected.
pub fn adapter_rating() -> Option<u32> {
    unsafe {
        let d = IOPSCopyExternalPowerAdapterDetails();
        if d.is_null() {
            return None;
        }
        let d: CFDictionary<CFString, CFType> = CFDictionary::wrap_under_create_rule(d);
        d.find(CFString::new("Watts"))
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64())
            .map(|w| w as u32)
    }
}

/// On AC, the battery supplying more than this many watts means the adapter is not keeping up...
const DRAIN_W: f64 = 1.0;
/// ...and charging faster than this leaves room for more work.
const SPARE_W: f64 = 3.0;
/// The duty cap's steps each period: down fast, up slowly, as the fan rules do.
const CUT: f64 = 0.05;
const RISE: f64 = 0.02;

/// On AC, keeps the battery from falling below a minimum when the adapter cannot keep up with
/// the load, by finding the most work it can sustain: a cap on the duty that falls while the
/// battery, at or below the minimum, supplies power, and rises while it charges with room to
/// spare or is above the minimum. Once the cap is back at the ceiling it lets go.
#[derive(Debug, Default)]
pub struct AcGuard {
    pub cap: Option<f64>,
}

impl AcGuard {
    /// One period. `battery_w` is the power the battery supplies, smoothed (negative while it
    /// charges); `duty` the work's duty now, where a new cap starts. True when it starts or
    /// stops limiting.
    pub fn update(
        &mut self,
        min: Option<u32>,
        power: Power,
        battery_w: Option<f64>,
        duty: f64,
        ceiling: f64,
        floor: f64,
    ) -> bool {
        let was = self.cap.is_some();
        self.cap = match (min, power.charge, battery_w) {
            (Some(min), Some(charge), Some(w)) if !power.on_battery => {
                let draining = charge <= min && w > DRAIN_W;
                let cap = match self.cap {
                    None if draining => Some(duty.min(ceiling) - CUT),
                    None => None,
                    Some(c) if draining => Some(c - CUT),
                    Some(c) if charge > min || w < -SPARE_W => Some(c + RISE),
                    c => c,
                };
                cap.filter(|&c| c < ceiling).map(|c| c.max(floor))
            }
            _ => None,
        };
        was != self.cap.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Power {
    pub on_battery: bool,
    /// Percent; None when there is no battery.
    pub charge: Option<u32>,
}

impl Power {
    pub fn current() -> Power {
        unsafe {
            let raw = IOPSCopyPowerSourcesInfo();
            if raw.is_null() {
                return Power::default();
            }
            let blob = CFType::wrap_under_create_rule(raw);
            let blob_ref = blob.as_CFTypeRef();
            let providing = IOPSGetProvidingPowerSourceType(blob_ref);
            let on_battery =
                !providing.is_null() && CFString::wrap_under_get_rule(providing) == "Battery Power";
            let mut charge = None;
            let list = IOPSCopyPowerSourcesList(blob_ref);
            if !list.is_null() {
                let list: CFArray<CFType> = CFArray::wrap_under_create_rule(list);
                for source in list.iter() {
                    let d = IOPSGetPowerSourceDescription(blob_ref, source.as_CFTypeRef());
                    if d.is_null() {
                        continue;
                    }
                    let d: CFDictionary<CFString, CFType> = CFDictionary::wrap_under_get_rule(d);
                    let get = |k: &str| {
                        d.find(CFString::new(k))
                            .and_then(|v| v.downcast::<CFNumber>())
                            .and_then(|n| n.to_i64())
                    };
                    if let (Some(cur), Some(max)) = (get("Current Capacity"), get("Max Capacity"))
                        && max > 0
                    {
                        charge = Some((cur * 100 / max) as u32);
                    }
                }
            }
            Power { on_battery, charge }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ac(charge: u32) -> Power {
        Power {
            on_battery: false,
            charge: Some(charge),
        }
    }

    #[test]
    fn the_cap_falls_while_draining_at_the_minimum_and_rises_back_to_nothing() {
        let mut g = AcGuard::default();
        // Above the minimum, draining: nothing yet.
        assert!(!g.update(Some(50), ac(60), Some(20.0), 0.8, 1.0, 0.2));
        assert_eq!(g.cap, None);
        // At it and draining: starts just below the duty now, then keeps cutting.
        assert!(g.update(Some(50), ac(50), Some(20.0), 0.8, 1.0, 0.2));
        assert!((g.cap.unwrap() - 0.75).abs() < 1e-9);
        g.update(Some(50), ac(50), Some(5.0), 0.75, 1.0, 0.2);
        assert!((g.cap.unwrap() - 0.70).abs() < 1e-9);
        // Balanced (charging a little): holds.
        g.update(Some(50), ac(50), Some(-1.0), 0.7, 1.0, 0.2);
        assert!((g.cap.unwrap() - 0.70).abs() < 1e-9);
        // Never below the floor.
        for _ in 0..30 {
            g.update(Some(50), ac(50), Some(20.0), 0.5, 1.0, 0.2);
        }
        assert!((g.cap.unwrap() - 0.2).abs() < 1e-9);
        // Charging with room to spare, or above the minimum: rises, and lets go at the ceiling.
        let mut released = false;
        for _ in 0..60 {
            released |= g.update(Some(50), ac(51), Some(10.0), 0.5, 1.0, 0.2);
        }
        assert!(released && g.cap.is_none());
    }

    #[test]
    fn it_lets_go_on_battery_or_without_a_minimum_or_a_reading() {
        let mut g = AcGuard::default();
        g.update(Some(50), ac(40), Some(20.0), 1.0, 1.0, 0.2);
        assert!(g.cap.is_some());
        let batt = Power {
            on_battery: true,
            charge: Some(40),
        };
        assert!(g.update(Some(50), batt, Some(20.0), 1.0, 1.0, 0.2));
        g.update(Some(50), ac(40), Some(20.0), 1.0, 1.0, 0.2);
        assert!(g.update(None, ac(40), Some(20.0), 1.0, 1.0, 0.2));
        g.update(Some(50), ac(40), Some(20.0), 1.0, 1.0, 0.2);
        assert!(g.update(Some(50), ac(40), None, 1.0, 1.0, 0.2));
    }
}
