//! Native macOS magnification synthesis for continuous thumb-wheel zoom.
//!
//! A wheel flick is a stream, not a series of independent zoom commands. The
//! worker below coalesces deltas into one AppKit-style magnification gesture:
//! `Began`, zero or more `Changed` events, then `Ended` after a short idle gap.
//! That is the event shape emitted by a trackpad and understood consistently by
//! browsers, Preview/PDF viewers, Photos, and other native macOS applications.

/// Feed one fractional magnification delta into the current zoom gesture.
///
/// Positive values zoom in and negative values zoom out. On macOS the deltas
/// are emitted as native magnification gesture events and automatically grouped
/// into a began/changed/ended sequence. Other platforms currently ignore them.
pub fn post_magnification(amount: f64) {
    #[cfg(target_os = "macos")]
    {
        macos::enqueue(amount);
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = amount;
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use core::ptr::NonNull;
    use std::sync::OnceLock;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
    use std::time::Duration;

    use objc2_application_services::AXUIElement;
    use objc2_core_foundation::CFRetained;
    use objc2_core_graphics::{CGEvent, CGEventField, CGEventTapLocation, CGEventType};

    /// A new wheel movement inside this gap continues the same pinch gesture.
    const END_AFTER_IDLE: Duration = Duration::from_millis(80);

    /// `NSEventTypeGesture`.
    const GESTURE_EVENT: CGEventType = CGEventType(29);
    /// `kIOHIDEventTypeZoom`.
    const HID_ZOOM: i64 = 8;
    /// Undocumented CoreGraphics field carrying the IOHID event subtype.
    const FIELD_HID_TYPE: CGEventField = CGEventField(110);
    /// Undocumented CoreGraphics field carrying the magnification delta.
    const FIELD_MAGNIFICATION: CGEventField = CGEventField(113);
    /// Undocumented CoreGraphics field carrying `IOHIDEventPhaseBits`.
    const FIELD_PHASE: CGEventField = CGEventField(132);

    static MAGNIFY_SENDER: OnceLock<Sender<f64>> = OnceLock::new();

    #[derive(Clone, Copy)]
    enum GesturePhase {
        Began,
        Changed,
        Ended,
    }

    impl GesturePhase {
        const fn bits(self) -> i64 {
            match self {
                Self::Began => 1,
                Self::Changed => 2,
                Self::Ended => 4,
            }
        }
    }

    pub(super) fn enqueue(amount: f64) {
        let sender = MAGNIFY_SENDER.get_or_init(start_worker);
        if let Err(error) = sender.send(amount) {
            tracing::warn!(%error, "magnification worker is unavailable");
        }
    }

    fn start_worker() -> Sender<f64> {
        let (sender, receiver) = mpsc::channel();
        if let Err(error) = std::thread::Builder::new()
            .name("openlogi-magnify".into())
            .spawn(move || run_worker(receiver))
        {
            tracing::warn!(%error, "could not start magnification worker");
        }
        sender
    }

    fn run_worker(receiver: Receiver<f64>) {
        let mut active = false;
        let mut target_pid: Option<i32> = None;
        loop {
            if !active {
                match receiver.recv() {
                    Ok(amount) => {
                        target_pid = hovered_pid();
                        post_event(0.0, GesturePhase::Began, target_pid);
                        post_event(amount, GesturePhase::Changed, target_pid);
                        active = true;
                    }
                    Err(_) => break,
                }
                continue;
            }

            match receiver.recv_timeout(END_AFTER_IDLE) {
                Ok(amount) => post_event(amount, GesturePhase::Changed, target_pid),
                Err(RecvTimeoutError::Timeout) => {
                    post_event(0.0, GesturePhase::Ended, target_pid);
                    active = false;
                    target_pid = None;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    post_event(0.0, GesturePhase::Ended, target_pid);
                    break;
                }
            }
        }
    }

    /// Resolve the application under the current pointer without activating it.
    ///
    /// Accessibility hit-testing follows the same screen point the user's mouse
    /// is over and gives us the owning process. The PID is captured once when a
    /// wheel gesture begins so every Began/Changed/Ended event in that gesture
    /// stays on one target even if the pointer moves slightly while zooming.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "macOS screen coordinates are small enough to round-trip through the AX API's f32 coordinates"
    )]
    #[expect(
        unsafe_code,
        reason = "objc2's AXUIElement hit-test methods expose Apple's C out-parameters as unsafe; all pointers are local, valid, and the copied element is immediately wrapped in CFRetained"
    )]
    fn hovered_pid() -> Option<i32> {
        let pointer_event = CGEvent::new(None)?;
        let location = CGEvent::location(Some(&pointer_event));

        // SAFETY: `new_system_wide` returns a retained system accessibility
        // object. The two out-pointers below point to live stack locals for the
        // duration of the calls. `copy_element_at_position` follows Core
        // Foundation's Create/Copy rule; wrapping the returned non-null pointer
        // in `CFRetained` balances that ownership when it drops.
        unsafe {
            let system = AXUIElement::new_system_wide();
            let mut raw_element: *const AXUIElement = core::ptr::null();
            let hit_error = system.copy_element_at_position(
                location.x as f32,
                location.y as f32,
                NonNull::from(&mut raw_element),
            );
            if hit_error.0 != 0 {
                tracing::debug!(error = hit_error.0, "could not hit-test app under pointer for zoom");
                return None;
            }

            let element_ptr = NonNull::new(raw_element.cast_mut())?;
            let element = CFRetained::from_raw(element_ptr);
            let mut pid = 0_i32;
            let pid_error = element.pid(NonNull::from(&mut pid));
            if pid_error.0 == 0 && pid > 0 {
                Some(pid)
            } else {
                tracing::debug!(error = pid_error.0, "could not resolve hovered app PID for zoom");
                None
            }
        }
    }

    fn post_event(amount: f64, phase: GesturePhase, target_pid: Option<i32>) {
        let Some(event) = CGEvent::new(None) else {
            tracing::warn!("CGEvent::new failed for magnification");
            return;
        };

        CGEvent::set_type(Some(&event), GESTURE_EVENT);
        CGEvent::set_integer_value_field(Some(&event), FIELD_HID_TYPE, HID_ZOOM);
        CGEvent::set_integer_value_field(Some(&event), FIELD_PHASE, phase.bits());
        CGEvent::set_double_value_field(Some(&event), FIELD_MAGNIFICATION, amount);

        // A global gesture event is routed to the focused application, unlike a
        // mouse-wheel event, which WindowServer routes by pointer location.
        // Posting directly to the app under the pointer reproduces the wheel's
        // hover-targeting semantics without activating that app or changing the
        // user's focused window. If AX hit-testing is unavailable, preserve the
        // previous focused-app behavior as a fallback.
        if let Some(pid) = target_pid {
            CGEvent::post_to_pid(pid, Some(&event));
        } else {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::GesturePhase;

        #[test]
        fn gesture_phases_use_iohid_bit_values() {
            assert_eq!(GesturePhase::Began.bits(), 1);
            assert_eq!(GesturePhase::Changed.bits(), 2);
            assert_eq!(GesturePhase::Ended.bits(), 4);
        }
    }
}
