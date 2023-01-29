//! Test that our use of #[track_caller] is making the correct line number
//! show up.
use std::panic;
use std::process::abort;
use std::ptr;
use std::sync::Mutex;

use objc2::rc::{Allocated, Id, Shared, __RcTestObject};
use objc2::runtime::{NSObject, Object};
use objc2::{class, declare_class, msg_send, msg_send_id, ClassType};

static EXPECTED_MESSAGE: Mutex<String> = Mutex::new(String::new());
static EXPECTED_LINE: Mutex<u32> = Mutex::new(0);

pub struct PanicChecker(());

impl PanicChecker {
    fn new() -> Self {
        panic::set_hook(Box::new(|info| {
            let expected_message = EXPECTED_MESSAGE.lock().unwrap();
            let expected_line = EXPECTED_LINE.lock().unwrap();

            let payload = info.payload();
            let message = if let Some(payload) = payload.downcast_ref::<&'static str>() {
                payload.to_string()
            } else if let Some(payload) = payload.downcast_ref::<String>() {
                payload.clone()
            } else {
                format!("could not extract message: {payload:?}")
            };
            let location = info.location().expect("location");

            if !message.contains(&*expected_message) {
                eprintln!("expected {expected_message:?}, got: {message:?}");
                abort();
            }
            if location.file() != file!() {
                eprintln!("expected file {:?}, got: {:?}", file!(), location.file());
                abort();
            }
            if location.line() != *expected_line {
                eprintln!("expected line {expected_line}, got: {}", location.line());
                abort();
            }
        }));
        Self(())
    }

    fn assert_panics(&self, message: &str, line: u32, f: impl FnOnce()) {
        *EXPECTED_MESSAGE.lock().unwrap() = message.to_string();
        *EXPECTED_LINE.lock().unwrap() = line;

        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            f();
        }));
        assert!(res.is_err());

        *EXPECTED_MESSAGE.lock().unwrap() = "unknown".to_string();
        *EXPECTED_LINE.lock().unwrap() = 0;
    }
}

impl Drop for PanicChecker {
    fn drop(&mut self) {
        let _ = panic::take_hook();
    }
}

#[test]
fn test_track_caller() {
    let checker = PanicChecker::new();

    #[cfg(debug_assertions)]
    {
        test_nil(&checker);
        test_verify(&checker);
        test_error_methods(&checker);
    }

    test_id_unwrap(&checker);

    #[cfg(feature = "catch-all")]
    test_catch_all(&checker);

    test_unwind(&checker);
}

pub fn test_nil(checker: &PanicChecker) {
    let nil: *mut Object = ptr::null_mut();

    let msg = "messsaging description to nil";
    checker.assert_panics(msg, line!() + 1, || {
        let _: *mut Object = unsafe { msg_send![nil, description] };
    });
    checker.assert_panics(msg, line!() + 1, || {
        let _: *mut Object = unsafe { msg_send![super(nil, NSObject::class()), description] };
    });
    checker.assert_panics(msg, line!() + 1, || {
        let _: Option<Id<Object, Shared>> = unsafe { msg_send_id![nil, description] };
    });
}

pub fn test_verify(checker: &PanicChecker) {
    let obj = NSObject::new();

    let msg = "invalid message send to -[NSObject description]: expected return to have type code '@', but found 'v'";
    checker.assert_panics(msg, line!() + 1, || {
        let _: () = unsafe { msg_send![&obj, description] };
    });

    let msg = "invalid message send to -[NSObject hash]: expected return to have type code 'Q', but found '@'";
    checker.assert_panics(msg, line!() + 1, || {
        let _: Option<Id<Object, Shared>> = unsafe { msg_send_id![&obj, hash] };
    });
}

pub fn test_error_methods(checker: &PanicChecker) {
    let nil: *mut Object = ptr::null_mut();

    let msg = "messsaging someSelectorWithError: to nil";
    checker.assert_panics(msg, line!() + 2, || {
        let _: Result<(), Id<NSObject, Shared>> =
            unsafe { msg_send![nil, someSelectorWithError: _] };
    });
    checker.assert_panics(msg, line!() + 2, || {
        let _: Result<(), Id<NSObject, Shared>> =
            unsafe { msg_send![super(nil, NSObject::class()), someSelectorWithError: _] };
    });
    checker.assert_panics(msg, line!() + 2, || {
        let _: Result<Id<Object, Shared>, Id<NSObject, Shared>> =
            unsafe { msg_send_id![nil, someSelectorWithError: _] };
    });

    let msg = "invalid message send to -[NSObject someSelectorWithError:]: method not found";
    checker.assert_panics(msg, line!() + 3, || {
        let obj = __RcTestObject::new();
        let _: Result<(), Id<NSObject, Shared>> =
            unsafe { msg_send![super(&obj), someSelectorWithError: _] };
    });
}

pub fn test_id_unwrap(checker: &PanicChecker) {
    let cls = __RcTestObject::class();
    let obj = __RcTestObject::new();

    let msg = "failed creating new instance using +[__RcTestObject newReturningNull]";
    checker.assert_panics(msg, line!() + 1, || {
        let _obj: Id<__RcTestObject, Shared> = unsafe { msg_send_id![cls, newReturningNull] };
    });

    let msg = "failed allocating with +[__RcTestObject allocReturningNull]";
    checker.assert_panics(msg, line!() + 1, || {
        let _obj: Allocated<__RcTestObject> = unsafe { msg_send_id![cls, allocReturningNull] };
    });

    let msg = "failed initializing object with -initReturningNull";
    checker.assert_panics(msg, line!() + 2, || {
        let _obj: Id<__RcTestObject, Shared> =
            unsafe { msg_send_id![__RcTestObject::alloc(), initReturningNull] };
    });

    let msg = "failed copying object";
    checker.assert_panics(msg, line!() + 1, || {
        let _obj: Id<__RcTestObject, Shared> = unsafe { msg_send_id![&obj, copyReturningNull] };
    });

    let msg = "unexpected NULL returned from -[__RcTestObject methodReturningNull]";
    checker.assert_panics(msg, line!() + 1, || {
        let _obj: Id<__RcTestObject, Shared> = unsafe { msg_send_id![&obj, methodReturningNull] };
    });
}

pub fn test_catch_all(checker: &PanicChecker) {
    let obj: Id<NSObject, Shared> = unsafe { msg_send_id![class!(NSArray), new] };

    let msg = "NSRangeException";
    checker.assert_panics(msg, line!() + 1, || {
        let _: *mut Object = unsafe { msg_send![&obj, objectAtIndex: 0usize] };
    });

    let msg = "NSRangeException";
    checker.assert_panics(msg, line!() + 1, || {
        let _: Id<Object, Shared> = unsafe { msg_send_id![&obj, objectAtIndex: 0usize] };
    });
}

declare_class!(
    struct PanickingClass;

    unsafe impl ClassType for PanickingClass {
        type Super = NSObject;
        const NAME: &'static str = "PanickingClass";
    }

    unsafe impl PanickingClass {
        #[method(panic)]
        fn _panic() -> *mut Self {
            panic!("panic in PanickingClass")
        }
    }
);

pub fn test_unwind(checker: &PanicChecker) {
    let msg = "panic in PanickingClass";
    let line = line!() - 7;
    checker.assert_panics(msg, line, || {
        let _: *mut Object = unsafe { msg_send![PanickingClass::class(), panic] };
    });
    checker.assert_panics(msg, line, || {
        let _: Id<Object, Shared> = unsafe { msg_send_id![PanickingClass::class(), panic] };
    });
}
