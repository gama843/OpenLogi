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

    use objc2_app_kit::NSWorkspace;
    use objc2_application_services::AXUIElement;
    use objc2_core_foundation::CFRetained;
    use objc2_core_graphics::{CGEvent, CGEventField, CGEventTapLocation, CGEventType};
    use objc2_foundation::NSPoint as CGPoint;

    /// A new wheel movement inside this gap continues the same pinch gesture.
    const END_AFTER_IDLE: Duration = Duration::from_millis(80);

    /// AppKit event types used by a real gesture stream.
    const BEGIN_GESTURE_EVENT: CGEventType = CGEventType(19);
    const END_GESTURE_EVENT: CGEventType = CGEventType(20);
    const MAGNIFY_EVENT: CGEventType = CGEventType(30);

    /// Private event payload copied from real AppKit gesture events.
    const FIELD_EVENT_FAMILY: CGEventField = CGEventField(55);
    const FIELD_GESTURE_FLAGS: CGEventField = CGEventField(59);
    const FIELD_GESTURE_MAGIC: CGEventField = CGEventField(87);
    const FIELD_GESTURE_KIND: CGEventField = CGEventField(101);
    const FIELD_GESTURE_KIND_2: CGEventField = CGEventField(107);
    const FIELD_HID_TYPE: CGEventField = CGEventField(110);
    const FIELD_MAGNIFICATION: CGEventField = CGEventField(113);
    const FIELD_MAGNIFICATION_2: CGEventField = CGEventField(114);
    const FIELD_MAGNIFICATION_3: CGEventField = CGEventField(116);
    const FIELD_MAGNIFICATION_4: CGEventField = CGEventField(118);
    const FIELD_GESTURE_SUBTYPE_1: CGEventField = CGEventField(115);
    const FIELD_GESTURE_SUBTYPE_2: CGEventField = CGEventField(117);
    const FIELD_PHASE: CGEventField = CGEventField(132);

    const NSEVENT_GESTURE_FAMILY: i64 = 29;
    const HID_ZOOM: i64 = 8;
    const HID_GESTURE_BEGIN: i64 = 61;
    const HID_GESTURE_END: i64 = 62;
    const GESTURE_MAGIC: i64 = 4_294_970_300;

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
        let mut location = CGPoint { x: 0.0, y: 0.0 };
        let mut target_pid: Option<i32> = None;
        loop {
            if !active {
                match receiver.recv() {
                    Ok(amount) => {
                        location = pointer_location();
                        target_pid = unfocused_hovered_pid(location);
                        post_gesture_boundary(true, location, target_pid);
                        post_magnify(0.0, GesturePhase::Began, location, target_pid);
                        post_magnify(amount, GesturePhase::Changed, location, target_pid);
                        active = true;
                    }
                    Err(_) => break,
                }
                continue;
            }

            match receiver.recv_timeout(END_AFTER_IDLE) {
                Ok(amount) => post_magnify(amount, GesturePhase::Changed, location, target_pid),
                Err(RecvTimeoutError::Timeout) => {
                    post_magnify(0.0, GesturePhase::Ended, location, target_pid);
                    post_gesture_boundary(false, location, target_pid);
                    active = false;
                    target_pid = None;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    post_magnify(0.0, GesturePhase::Ended, location, target_pid);
                    post_gesture_boundary(false, location, target_pid);
                    break;
                }
            }
        }
    }

    /// Snapshot the pointer position when the wheel gesture begins.
    fn pointer_location() -> CGPoint {
        CGEvent::new(None).map_or(CGPoint { x: 0.0, y: 0.0 }, |event| {
            CGEvent::location(Some(&event))
        })
    }

    /// If the pointer is over a different application than the frontmost one,
    /// return that application's PID. If it is already over the focused app,
    /// return `None` so we keep the known-good global HID posting path.
    fn unfocused_hovered_pid(location: CGPoint) -> Option<i32> {
        let hovered = hovered_pid(location)?;
        match frontmost_pid() {
            Some(frontmost) if frontmost == hovered => None,
            _ => Some(hovered),
        }
    }

    fn frontmost_pid() -> Option<i32> {
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|app| app.processIdentifier())
    }

    /// Accessibility hit-test at the pointer location without activating the
    /// window. We only need its owning PID; focus remains unchanged.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "macOS screen coordinates are small enough for the AX API's f32 coordinates"
    )]
    #[expect(
        unsafe_code,
        reason = "objc2's AX hit-test methods expose C out-parameters; the pointers below reference live stack locals and copied CF objects are retained immediately"
    )]
    fn hovered_pid(location: CGPoint) -> Option<i32> {
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

    fn post_gesture_boundary(begin: bool, location: CGPoint, target_pid: Option<i32>) {
        let Some(event) = CGEvent::new(None) else {
            tracing::warn!("CGEvent::new failed for gesture boundary");
            return;
        };

        CGEvent::set_type(
            Some(&event),
            if begin {
                BEGIN_GESTURE_EVENT
            } else {
                END_GESTURE_EVENT
            },
        );
        CGEvent::set_location(Some(&event), location);
        CGEvent::set_integer_value_field(Some(&event), FIELD_EVENT_FAMILY, NSEVENT_GESTURE_FAMILY);
        CGEvent::set_integer_value_field(
            Some(&event),
            FIELD_HID_TYPE,
            if begin {
                HID_GESTURE_BEGIN
            } else {
                HID_GESTURE_END
            },
        );
        CGEvent::set_integer_value_field(Some(&event), FIELD_GESTURE_SUBTYPE_1, 5);
        CGEvent::set_integer_value_field(Some(&event), FIELD_GESTURE_SUBTYPE_2, 5);
        CGEvent::set_integer_value_field(Some(&event), FIELD_GESTURE_MAGIC, GESTURE_MAGIC);
        post_event(&event, target_pid);
    }

    fn post_magnify(
        amount: f64,
        phase: GesturePhase,
        location: CGPoint,
        target_pid: Option<i32>,
    ) {
        let Some(event) = CGEvent::new(None) else {
            tracing::warn!("CGEvent::new failed for magnification");
            return;
        };

        CGEvent::set_type(Some(&event), MAGNIFY_EVENT);
        CGEvent::set_location(Some(&event), location);
        CGEvent::set_integer_value_field(Some(&event), FIELD_EVENT_FAMILY, NSEVENT_GESTURE_FAMILY);
        CGEvent::set_integer_value_field(Some(&event), FIELD_GESTURE_FLAGS, 256);
        CGEvent::set_integer_value_field(Some(&event), FIELD_GESTURE_MAGIC, GESTURE_MAGIC);
        CGEvent::set_integer_value_field(Some(&event), FIELD_GESTURE_KIND, 4);
        CGEvent::set_integer_value_field(Some(&event), FIELD_GESTURE_KIND_2, 4);
        CGEvent::set_integer_value_field(Some(&event), FIELD_HID_TYPE, HID_ZOOM);
        CGEvent::set_integer_value_field(Some(&event), FIELD_PHASE, phase.bits());
        for field in [
            FIELD_MAGNIFICATION,
            FIELD_MAGNIFICATION_2,
            FIELD_MAGNIFICATION_3,
            FIELD_MAGNIFICATION_4,
        ] {
            CGEvent::set_double_value_field(Some(&event), field, amount);
        }
        post_event(&event, target_pid);
    }

    /// A global gesture is handled by the focused app. For an unfocused app we
    /// instead inject the complete AppKit-style gesture stream directly into
    /// that process. `CGEventPostToPid` does not activate the application.
    fn post_event(event: &CGEvent, target_pid: Option<i32>) {
        if let Some(pid) = target_pid {
            CGEvent::post_to_pid(pid, Some(event));
        } else {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(event));
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
