//
// Copyright © 2020 Haim Gelfenbeyn
// This code is licensed under MIT license (see LICENSE.txt for details)
//
use ddc_hi::{Display, Handle};

use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;

#[link(name = "CoreDisplay", kind = "framework")]
extern "C" {
    fn CoreDisplay_DisplayCreateInfoDictionary(display: u32) -> CFDictionaryRef;
}

/// The EDID serial number of a display, used to tell identical monitors apart.
///
/// `ddc-hi` only populates `DisplayInfo::serial` when it can parse the raw EDID, which on
/// macOS it reads from the CoreDisplay `IODisplayEDIDOriginal` key. That key is absent on
/// recent macOS versions (verified missing on macOS 27), so fall back to the `DisplaySerialNumber`
/// key of the same dictionary, which carries the serial from the EDID header.
pub fn display_serial(display: &Display) -> Option<u32> {
    if let Some(serial) = display.info.serial {
        return Some(serial);
    }
    let monitor = match display.handle {
        Handle::MacOS(ref monitor) => monitor,
        #[allow(unreachable_patterns)]
        _ => return None,
    };
    let info = unsafe {
        let info = CoreDisplay_DisplayCreateInfoDictionary(monitor.handle().id);
        if info.is_null() {
            return None;
        }
        CFDictionary::<CFString, CFType>::wrap_under_create_rule(info)
    };
    info.find(CFString::from_static_string("DisplaySerialNumber"))?
        .downcast::<CFNumber>()?
        .to_i64()?
        .try_into()
        .ok()
}
