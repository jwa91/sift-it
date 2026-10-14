//! Whether the Mac is on battery, or in Low Power Mode: the indexer's first pass can wait for a charger.

use objc2_core_foundation::{CFRetained, CFString, CFType};
use objc2_foundation::NSProcessInfo;
use std::ffi::c_void;
use std::ptr::NonNull;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    /// A CFTypeRef the caller releases.
    fn IOPSCopyPowerSourcesInfo() -> *mut c_void;
    /// A CFStringRef owned by the snapshot: "AC Power", "Battery Power" or "Off Line".
    fn IOPSGetProvidingPowerSourceType(snapshot: *const c_void) -> *const c_void;
}

pub fn on_battery() -> bool {
    unsafe {
        let Some(snapshot) = NonNull::new(IOPSCopyPowerSourcesInfo()) else { return false };
        let snapshot: CFRetained<CFType> = CFRetained::from_raw(snapshot.cast());
        let source = IOPSGetProvidingPowerSourceType(&*snapshot as *const CFType as *const c_void);
        if source.is_null() {
            return false;
        }
        (*(source as *const CFString)).to_string() == "Battery Power"
    }
}

pub fn low_power_mode() -> bool {
    NSProcessInfo::processInfo().isLowPowerModeEnabled()
}
