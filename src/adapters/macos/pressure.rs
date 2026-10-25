//! Memory pressure, as macOS reports it: a dispatch source that fires when the system's level changes (normal,
//! warning, critical), read where it was when it started from the kernel's own record of it.

use std::ffi::c_void;
use std::sync::Once;
use std::sync::atomic::{AtomicU8, Ordering};

/// How short of memory the Mac is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pressure {
    #[default]
    Normal,
    /// Memory is getting short: what can be let go should be.
    Warning,
    /// The Mac is about to kill processes for memory.
    Critical,
}

/// DISPATCH_MEMORYPRESSURE_* (dispatch/source.h), which are also kern.memorystatus_vm_pressure_level's values.
const NORMAL: usize = 0x1;
const WARN: usize = 0x2;
const CRITICAL: usize = 0x4;

static LEVEL: AtomicU8 = AtomicU8::new(0);
static WATCH: Once = Once::new();

#[allow(non_upper_case_globals)]
unsafe extern "C" {
    static _dispatch_source_type_memorypressure: c_void;
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut c_void;
    fn dispatch_source_create(kind: *const c_void, handle: usize, mask: usize, queue: *mut c_void) -> *mut c_void;
    fn dispatch_set_context(object: *mut c_void, context: *mut c_void);
    fn dispatch_source_set_event_handler_f(source: *mut c_void, handler: extern "C" fn(*mut c_void));
    fn dispatch_source_get_data(source: *mut c_void) -> usize;
    fn dispatch_resume(object: *mut c_void);
}

fn store(mask: usize) {
    let level = if mask & CRITICAL != 0 {
        Pressure::Critical
    } else if mask & WARN != 0 {
        Pressure::Warning
    } else {
        Pressure::Normal
    };
    LEVEL.store(level as u8, Ordering::Relaxed);
}

/// The source's handler, on a dispatch queue's thread: its context is the source.
extern "C" fn changed(source: *mut c_void) {
    store(unsafe { dispatch_source_get_data(source) });
}

/// The level now. The first call starts watching it.
pub fn level() -> Pressure {
    WATCH.call_once(|| {
        let mut mask = 0u32;
        let mut size = std::mem::size_of::<u32>();
        let read = unsafe {
            libc::sysctlbyname(
                c"kern.memorystatus_vm_pressure_level".as_ptr(),
                &mut mask as *mut u32 as *mut c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if read == 0 {
            store(mask as usize);
        }
        unsafe {
            let queue = dispatch_get_global_queue(0, 0);
            let kind = &raw const _dispatch_source_type_memorypressure;
            let source = dispatch_source_create(kind, 0, NORMAL | WARN | CRITICAL, queue);
            if source.is_null() {
                return;
            }
            // It lives as long as the process does.
            dispatch_set_context(source, source);
            dispatch_source_set_event_handler_f(source, changed);
            dispatch_resume(source);
        }
    });
    match LEVEL.load(Ordering::Relaxed) {
        2 => Pressure::Critical,
        1 => Pressure::Warning,
        _ => Pressure::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_level_reads_and_follows_the_masks() {
        // On a Mac with memory to spare.
        assert!(level() < Pressure::Critical);
        store(WARN);
        assert_eq!(LEVEL.load(Ordering::Relaxed), Pressure::Warning as u8);
        store(CRITICAL | WARN);
        assert_eq!(LEVEL.load(Ordering::Relaxed), Pressure::Critical as u8);
        store(NORMAL);
        assert_eq!(level(), Pressure::Normal);
    }
}
