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
