//! Keep the Mac awake while the app is frontmost (IOKit power assertion, like `caffeinate -d`).
//! Released as soon as the window loses focus.

use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        level: u32,
        name: CFStringRef,
        id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

const ASSERTION_LEVEL_ON: u32 = 255;

#[derive(Default)]
pub struct StayAwake {
    id: Option<u32>,
}

impl StayAwake {
    pub fn set(&mut self, active: bool) {
        match (active, self.id) {
            (true, None) => {
                // Prevents idle display sleep, which also prevents idle system sleep.
                let kind = CFString::new("PreventUserIdleDisplaySleep");
                let name = CFString::new("juan-terminal is in the foreground");
                let mut id = 0u32;
                let ok = unsafe { IOPMAssertionCreateWithName(kind.as_concrete_TypeRef(), ASSERTION_LEVEL_ON, name.as_concrete_TypeRef(), &mut id) };
                if ok == 0 {
                    self.id = Some(id);
                }
            }
            (false, Some(id)) => {
                unsafe { IOPMAssertionRelease(id) };
                self.id = None;
            }
            _ => {}
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_active(&self) -> bool {
        self.id.is_some()
    }
}

impl Drop for StayAwake {
    fn drop(&mut self) {
        self.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assertion_roundtrip() {
        let mut s = StayAwake::default();
        s.set(true);
        assert!(s.is_active());
        let out = std::process::Command::new("pmset").args(["-g", "assertions"]).output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("juan-terminal is in the foreground"));
        s.set(false);
        assert!(!s.is_active());
    }
}
