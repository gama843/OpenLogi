//! Native macOS magnification synthesis for continuous thumb-wheel zoom.
//!
//! A wheel flick is a stream, not a series of independent zoom commands. The
//! worker below coalesces deltas into one AppKit-style magnification gesture:
//! `Began`, zero or more `Changed` events, then `Ended` after a short idle gap.
//! On macOS, ordinary scroll-wheel events are routed by WindowServer to the
//! window under the pointer. Gesture events are different: a synthetic pinch is
//! normally delivered to the active AppKit application. For a background window
//! we therefore use SkyLight's focus-without-raise path to make only its AppKit
//! input route active, post the magnification directly to that process, and then
//! restore the user's foreground application.

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
    use std::ffi::c_void;
    use std::sync::OnceLock;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
    use std::time::Duration;

    use objc2_core_graphics::{CGEvent, CGEventField, CGEventTapLocation, CGEventType};
    use objc2_foundation::NSPoint as CGPoint;

    /// A new wheel movement inside this gap continues the same pinch gesture.
    const END_AFTER_IDLE: Duration = Duration::from_millis(80);
    /// Give AppKit a short moment to consume the private activation records.
    const BACKGROUND_ACTIVATION_SETTLE: Duration = Duration::from_millis(50);
    /// Let the target consume the `Ended` event before restoring the prior app.
    const BACKGROUND_RESTORE_SETTLE: Duration = Duration::from_millis(12);

    /// The compact gesture event that is known to behave like native pinch on
    /// Preview, browsers, Photos, and other AppKit applications.
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

    enum Route {
        /// Known-good path for the already active application.
        Focused,
        /// A different visible window is under the pointer. SkyLight has made
        /// its AppKit input route active without raising/restacking the window.
        Background {
            target: skylight::HoveredWindow,
            activation: skylight::ActivationLease,
        },
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
        let mut route = Route::Focused;

        loop {
            if !active {
                match receiver.recv() {
                    Ok(amount) => {
                        route = route_for_pointer();
                        post_event(0.0, GesturePhase::Began, &route);
                        post_event(amount, GesturePhase::Changed, &route);
                        active = true;
                    }
                    Err(_) => break,
                }
                continue;
            }

            match receiver.recv_timeout(END_AFTER_IDLE) {
                Ok(amount) => post_event(amount, GesturePhase::Changed, &route),
                Err(RecvTimeoutError::Timeout) => {
                    post_event(0.0, GesturePhase::Ended, &route);
                    finish_route(&route);
                    active = false;
                    route = Route::Focused;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    post_event(0.0, GesturePhase::Ended, &route);
                    finish_route(&route);
                    break;
                }
            }
        }
    }

    fn route_for_pointer() -> Route {
        let location = pointer_location();
        let Some(target) = skylight::window_under_pointer(location) else {
            return Route::Focused;
        };

        match skylight::activate_if_background(target) {
            skylight::ActivationResult::AlreadyFrontmost => Route::Focused,
            skylight::ActivationResult::Activated(activation) => {
                std::thread::sleep(BACKGROUND_ACTIVATION_SETTLE);
                Route::Background { target, activation }
            }
            skylight::ActivationResult::Unavailable => {
                tracing::debug!(
                    pid = target.pid,
                    wid = target.window_id,
                    "background zoom routing unavailable; keeping focused-app behavior"
                );
                Route::Focused
            }
        }
    }

    fn finish_route(route: &Route) {
        if let Route::Background { activation, .. } = route {
            std::thread::sleep(BACKGROUND_RESTORE_SETTLE);
            activation.restore();
        }
    }

    fn pointer_location() -> CGPoint {
        CGEvent::new(None).map_or(CGPoint { x: 0.0, y: 0.0 }, |event| {
            CGEvent::location(Some(&event))
        })
    }

    fn post_event(amount: f64, phase: GesturePhase, route: &Route) {
        let Some(event) = CGEvent::new(None) else {
            tracing::warn!("CGEvent::new failed for magnification");
            return;
        };

        CGEvent::set_type(Some(&event), GESTURE_EVENT);
        CGEvent::set_integer_value_field(Some(&event), FIELD_HID_TYPE, HID_ZOOM);
        CGEvent::set_integer_value_field(Some(&event), FIELD_PHASE, phase.bits());
        CGEvent::set_double_value_field(Some(&event), FIELD_MAGNIFICATION, amount);

        match route {
            Route::Focused => {
                CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
            }
            Route::Background { target, .. } => {
                // Preserve both the screen point and WindowServer's window-local
                // hit-test point. The latter is a private field setter used by
                // macOS background-input implementations; it matters when one
                // process owns more than one window.
                CGEvent::set_location(Some(&event), target.screen_point);
                skylight::set_window_location(&event, target.window_point);

                // The public CGEventPostToPid path is ignored by several native
                // applications while inactive. SLEventPostToPid travels through
                // SkyLight's WindowServer delivery path. If that private symbol
                // ever disappears, the target has already been made AppKit-active,
                // so a global HID post remains a useful compatibility fallback.
                if !skylight::post_to_pid(target.pid, &event) {
                    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
                }
            }
        }
    }

    /// Private WindowServer routing used only for hover-targeted background
    /// magnification. Every symbol is resolved dynamically, so a future macOS
    /// release can fail closed back to the ordinary focused zoom path instead of
    /// preventing OpenLogi from launching.
    #[expect(
        unsafe_code,
        reason = "macOS exposes the required focus-without-raise and SkyLight event-posting primitives only as private C SPIs resolved with dlopen/dlsym"
    )]
    mod skylight {
        use super::{CGEvent, CGPoint, NonNull, c_void};
        use libc::{c_char, c_int, pid_t};
        use std::sync::OnceLock;

        type MainConnectionFn = unsafe extern "C" fn() -> c_int;
        type FindWindowFn = unsafe extern "C" fn(
            c_int,
            c_int,
            c_int,
            c_int,
            *mut CGPoint,
            *mut CGPoint,
            *mut u32,
            *mut c_int,
        ) -> c_int;
        type ConnectionPidFn = unsafe extern "C" fn(c_int, *mut pid_t) -> c_int;
        type GetFrontProcessFn = unsafe extern "C" fn(*mut c_void) -> c_int;
        type GetConnectionPsnFn = unsafe extern "C" fn(c_int, *mut c_void) -> c_int;
        type PostEventRecordFn = unsafe extern "C" fn(*const c_void, *const u8) -> c_int;
        type SetFrontProcessFn = unsafe extern "C" fn(*const c_void, u32, u32) -> c_int;
        type PostToPidFn = unsafe extern "C" fn(pid_t, *mut c_void);
        type SetWindowLocationFn = unsafe extern "C" fn(*mut c_void, f64, f64);

        const EVENT_RECORD_LEN: usize = 0xF8;
        const FOCUS_MARKER_OFFSET: usize = 0x8A;
        const WINDOW_ID_OFFSET: usize = 0x3C;
        const K_CPS_NO_WINDOWS: u32 = 0x400;

        #[derive(Clone, Copy, Debug)]
        pub(super) struct HoveredWindow {
            pub pid: i32,
            pub window_id: u32,
            connection_id: i32,
            pub screen_point: CGPoint,
            pub window_point: CGPoint,
        }

        pub(super) enum ActivationResult {
            AlreadyFrontmost,
            Activated(ActivationLease),
            Unavailable,
        }

        pub(super) struct ActivationLease {
            previous_psn: [u8; 8],
            target_psn: [u8; 8],
            target_window_id: u32,
        }

        impl ActivationLease {
            pub(super) fn restore(&self) {
                // Deactivate the temporary target-side AppKit route first, then
                // explicitly restore the process that was frontmost before the
                // wheel gesture. `kCPSNoWindows` avoids raising/restacking its
                // windows; visually it was already in front the whole time.
                if let Some(post_record) = post_event_record_fn() {
                    let mut record = event_record(self.target_window_id);
                    record[FOCUS_MARKER_OFFSET] = 0x02;
                    unsafe {
                        post_record(self.target_psn.as_ptr().cast(), record.as_ptr());
                    }
                }
                if let Some(set_front) = set_front_process_fn() {
                    unsafe {
                        set_front(
                            self.previous_psn.as_ptr().cast(),
                            0,
                            K_CPS_NO_WINDOWS,
                        );
                    }
                }
            }
        }

        pub(super) fn window_under_pointer(point: CGPoint) -> Option<HoveredWindow> {
            let connection = unsafe { main_connection_fn()?() };
            let find_window = find_window_fn()?;
            let get_pid = connection_pid_fn()?;

            let mut screen_point = point;
            let mut window_point = CGPoint { x: 0.0, y: 0.0 };
            let mut window_id = 0_u32;
            let mut owner_connection = 0_i32;

            let mut status = unsafe {
                find_window(
                    connection,
                    0,
                    1,
                    0,
                    &raw mut screen_point,
                    &raw mut window_point,
                    &raw mut window_id,
                    &raw mut owner_connection,
                )
            };

            // WindowServer can first hand back a helper window owned by our own
            // connection. Ask for the next window below it, mirroring yabai's
            // pointer hit-test behavior.
            if status == 0 && owner_connection == connection && window_id != 0 {
                status = unsafe {
                    find_window(
                        connection,
                        window_id.cast_signed(),
                        -1,
                        0,
                        &raw mut screen_point,
                        &raw mut window_point,
                        &raw mut window_id,
                        &raw mut owner_connection,
                    )
                };
            }

            if status != 0 || window_id == 0 || owner_connection == 0 {
                return None;
            }

            let mut pid: pid_t = 0;
            if unsafe { get_pid(owner_connection, &raw mut pid) } != 0 || pid <= 0 {
                return None;
            }

            Some(HoveredWindow {
                pid,
                window_id,
                connection_id: owner_connection,
                screen_point: point,
                window_point,
            })
        }

        pub(super) fn activate_if_background(target: HoveredWindow) -> ActivationResult {
            let Some(get_front) = get_front_process_fn() else {
                return ActivationResult::Unavailable;
            };
            let Some(get_psn) = get_connection_psn_fn() else {
                return ActivationResult::Unavailable;
            };
            let Some(post_record) = post_event_record_fn() else {
                return ActivationResult::Unavailable;
            };
            // We require a restoration primitive before changing any AppKit
            // activation state. If it is absent, leave the user's focus alone.
            if set_front_process_fn().is_none() {
                return ActivationResult::Unavailable;
            }

            let mut previous_psn = [0_u8; 8];
            if unsafe { get_front(previous_psn.as_mut_ptr().cast()) } != 0 {
                return ActivationResult::Unavailable;
            }

            let mut target_psn = [0_u8; 8];
            if unsafe {
                get_psn(
                    target.connection_id,
                    target_psn.as_mut_ptr().cast::<c_void>(),
                )
            } != 0
            {
                return ActivationResult::Unavailable;
            }

            if previous_psn == target_psn {
                return ActivationResult::AlreadyFrontmost;
            }

            let mut record = event_record(target.window_id);
            record[FOCUS_MARKER_OFFSET] = 0x02;
            let defocused = unsafe {
                post_record(previous_psn.as_ptr().cast(), record.as_ptr()) == 0
            };
            record[FOCUS_MARKER_OFFSET] = 0x01;
            let focused = unsafe { post_record(target_psn.as_ptr().cast(), record.as_ptr()) == 0 };

            if !(defocused && focused) {
                if let Some(set_front) = set_front_process_fn() {
                    unsafe {
                        set_front(previous_psn.as_ptr().cast(), 0, K_CPS_NO_WINDOWS);
                    }
                }
                return ActivationResult::Unavailable;
            }

            ActivationResult::Activated(ActivationLease {
                previous_psn,
                target_psn,
                target_window_id: target.window_id,
            })
        }

        pub(super) fn post_to_pid(pid: i32, event: &CGEvent) -> bool {
            let Some(post) = post_to_pid_fn() else {
                return false;
            };
            let event_ptr = NonNull::from(event).as_ptr().cast::<c_void>();
            unsafe {
                post(pid, event_ptr);
            }
            true
        }

        pub(super) fn set_window_location(event: &CGEvent, point: CGPoint) {
            let Some(set_location) = set_window_location_fn() else {
                return;
            };
            let event_ptr = NonNull::from(event).as_ptr().cast::<c_void>();
            unsafe {
                set_location(event_ptr, point.x, point.y);
            }
        }

        fn event_record(window_id: u32) -> [u8; EVENT_RECORD_LEN] {
            let mut record = [0_u8; EVENT_RECORD_LEN];
            record[0x04] = 0xF8;
            record[0x08] = 0x0D;
            record[WINDOW_ID_OFFSET..WINDOW_ID_OFFSET + 4]
                .copy_from_slice(&window_id.to_le_bytes());
            record
        }

        fn skylight_handle() -> Option<usize> {
            static HANDLE: OnceLock<Option<usize>> = OnceLock::new();
            *HANDLE.get_or_init(|| {
                let path = b"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight\0";
                let handle = unsafe {
                    libc::dlopen(
                        path.as_ptr().cast::<c_char>(),
                        libc::RTLD_LAZY | libc::RTLD_GLOBAL,
                    )
                };
                (!handle.is_null()).then_some(handle as usize)
            })
        }

        fn find_symbol(name: &[u8]) -> Option<*mut c_void> {
            let _handle = skylight_handle()?;
            let raw = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr().cast::<c_char>()) };
            (!raw.is_null()).then_some(raw)
        }

        unsafe fn as_fn<T: Copy>(raw: *mut c_void) -> T {
            unsafe { std::mem::transmute_copy::<*mut c_void, T>(&raw) }
        }

        fn resolve<T: Copy>(name: &[u8]) -> Option<T> {
            find_symbol(name).map(|raw| unsafe { as_fn(raw) })
        }

        fn main_connection_fn() -> Option<MainConnectionFn> {
            static SYMBOL: OnceLock<Option<MainConnectionFn>> = OnceLock::new();
            *SYMBOL.get_or_init(|| {
                resolve(b"SLSMainConnectionID\0").or_else(|| resolve(b"CGSMainConnectionID\0"))
            })
        }

        fn find_window_fn() -> Option<FindWindowFn> {
            static SYMBOL: OnceLock<Option<FindWindowFn>> = OnceLock::new();
            *SYMBOL.get_or_init(|| resolve(b"SLSFindWindowAndOwner\0"))
        }

        fn connection_pid_fn() -> Option<ConnectionPidFn> {
            static SYMBOL: OnceLock<Option<ConnectionPidFn>> = OnceLock::new();
            *SYMBOL.get_or_init(|| resolve(b"SLSConnectionGetPID\0"))
        }

        fn get_front_process_fn() -> Option<GetFrontProcessFn> {
            static SYMBOL: OnceLock<Option<GetFrontProcessFn>> = OnceLock::new();
            *SYMBOL.get_or_init(|| resolve(b"_SLPSGetFrontProcess\0"))
        }

        fn get_connection_psn_fn() -> Option<GetConnectionPsnFn> {
            static SYMBOL: OnceLock<Option<GetConnectionPsnFn>> = OnceLock::new();
            *SYMBOL.get_or_init(|| resolve(b"SLSGetConnectionPSN\0"))
        }

        fn post_event_record_fn() -> Option<PostEventRecordFn> {
            static SYMBOL: OnceLock<Option<PostEventRecordFn>> = OnceLock::new();
            *SYMBOL.get_or_init(|| resolve(b"SLPSPostEventRecordTo\0"))
        }

        fn set_front_process_fn() -> Option<SetFrontProcessFn> {
            static SYMBOL: OnceLock<Option<SetFrontProcessFn>> = OnceLock::new();
            *SYMBOL.get_or_init(|| {
                resolve(b"SLPSSetFrontProcessWithOptions\0")
                    .or_else(|| resolve(b"_SLPSSetFrontProcessWithOptions\0"))
            })
        }

        fn post_to_pid_fn() -> Option<PostToPidFn> {
            static SYMBOL: OnceLock<Option<PostToPidFn>> = OnceLock::new();
            *SYMBOL.get_or_init(|| resolve(b"SLEventPostToPid\0"))
        }

        fn set_window_location_fn() -> Option<SetWindowLocationFn> {
            static SYMBOL: OnceLock<Option<SetWindowLocationFn>> = OnceLock::new();
            *SYMBOL.get_or_init(|| resolve(b"CGEventSetWindowLocation\0"))
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
