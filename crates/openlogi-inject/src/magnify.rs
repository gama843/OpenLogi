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
    use std::sync::OnceLock;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
    use std::time::Duration;

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
        loop {
            if !active {
                match receiver.recv() {
                    Ok(amount) => {
                        location = pointer_location();
                        post_gesture_boundary(true, location);
                        post_magnify(0.0, GesturePhase::Began, location);
                        post_magnify(amount, GesturePhase::Changed, location);
                        active = true;
                    }
                    Err(_) => break,
                }
                continue;
            }

            match receiver.recv_timeout(END_AFTER_IDLE) {
                Ok(amount) => post_magnify(amount, GesturePhase::Changed, location),
                Err(RecvTimeoutError::Timeout) => {
                    post_magnify(0.0, GesturePhase::Ended, location);
                    post_gesture_boundary(false, location);
                    active = false;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    post_magnify(0.0, GesturePhase::Ended, location);
                    post_gesture_boundary(false, location);
                    break;
                }
            }
        }
    }

    /// Snapshot the pointer position when the wheel gesture begins. WindowServer
    /// can then route the magnification like a mouse-wheel event: to the window
    /// under this point, without activating it or changing keyboard focus.
    fn pointer_location() -> CGPoint {
        CGEvent::new(None).map_or(CGPoint { x: 0.0, y: 0.0 }, |event| {
            CGEvent::location(Some(&event))
        })
    }

    fn post_gesture_boundary(begin: bool, location: CGPoint) {
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
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }

    fn post_magnify(amount: f64, phase: GesturePhase, location: CGPoint) {
        let Some(event) = CGEvent::new(None) else {
            tracing::warn!("CGEvent::new failed for magnification");
            return;
        };

        // Type 30 is the AppKit magnify event that carries a screen location.
        // Keeping the event global (rather than `post_to_pid`) lets WindowServer
        // perform normal pointer-based routing while the focused app stays put.
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
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
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
