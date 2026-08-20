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
        loop {
            if !active {
                match receiver.recv() {
                    Ok(amount) => {
                        post_event(0.0, GesturePhase::Began);
                        post_event(amount, GesturePhase::Changed);
                        active = true;
                    }
                    Err(_) => break,
                }
                continue;
            }

            match receiver.recv_timeout(END_AFTER_IDLE) {
                Ok(amount) => post_event(amount, GesturePhase::Changed),
                Err(RecvTimeoutError::Timeout) => {
                    post_event(0.0, GesturePhase::Ended);
                    active = false;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    post_event(0.0, GesturePhase::Ended);
                    break;
                }
            }
        }
    }

    fn post_event(amount: f64, phase: GesturePhase) {
        let Some(event) = CGEvent::new(None) else {
            tracing::warn!("CGEvent::new failed for magnification");
            return;
        };

        CGEvent::set_type(Some(&event), GESTURE_EVENT);
        CGEvent::set_integer_value_field(Some(&event), FIELD_HID_TYPE, HID_ZOOM);
        CGEvent::set_integer_value_field(Some(&event), FIELD_PHASE, phase.bits());
        CGEvent::set_double_value_field(Some(&event), FIELD_MAGNIFICATION, amount);
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
