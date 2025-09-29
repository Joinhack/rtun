#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
use std::ffi::c_void;
use std::marker::{PhantomData, PhantomPinned};
use std::ptr;
use std::sync::mpsc::channel;

use log::info;

const kIOMessageSystemWillSleep: u32 = 0xe0000280;

type natural_t = u32;
type mach_port_t = natural_t;
type io_object_t = mach_port_t;
type io_connect_t = io_object_t;
type io_service_t = io_object_t;

#[repr(C)]
struct IONotificationPort {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}
type IONotificationPortRef = *mut IONotificationPort;

type IOServiceInterestCallback = unsafe extern "C" fn(
    refcon: *mut c_void,
    service: io_service_t,
    message_type: u32,
    message_argument: *mut c_void,
);

#[cfg_attr(target_os = "macos", link(name = "IOKit", kind = "framework"))]
unsafe extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        port_ref: *mut IONotificationPortRef,
        callback: IOServiceInterestCallback,
        notifier: *mut io_object_t,
    ) -> io_connect_t;
    fn IONotificationPortGetRunLoopSource(notify: IONotificationPortRef) -> CFRunLoopSourceRef;
}

#[repr(C)]
struct __CFString(c_void);
type CFStringRef = *const __CFString;

#[repr(C)]
struct __CFRunLoop {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

unsafe impl Send for __CFRunLoop {}
type CFRunLoopRef = *mut __CFRunLoop;

#[repr(C)]
struct __CFRunLoopSource {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

type CFRunLoopSourceRef = *mut __CFRunLoopSource;

type CFRunLoopMode = CFStringRef;

type CFTypeRef = *const c_void;

#[cfg_attr(target_os = "macos", link(name = "CoreFoundation", kind = "framework"))]
unsafe extern "C" {
    static kCFRunLoopCommonModes: CFStringRef;

    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFRunLoopMode);
    fn CFRunLoopRemoveSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFRunLoopMode);
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRun();
    fn CFRunLoopStop(rl: CFRunLoopRef);

    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

unsafe extern "C" fn power_callback<C>(
    ref_p: *mut c_void,
    _service: io_service_t,
    message_type: u32,
    _message_argument: *mut c_void,
) where
    C: Fn() + Send + 'static,
{
    let callback: *mut C = ref_p as *mut C;

    match message_type {
        kIOMessageSystemWillSleep => unsafe {
            info!("kIOMessageSystemWillSleep");
            (*callback)();
        },
        _ => (),
    }
}

struct CFRunLoop(CFRunLoopRef);

unsafe impl Send for CFRunLoop {}

pub struct PowerNotify {
    run_loop: CFRunLoop,
}

impl PowerNotify {
    pub fn new<C>(callback: C) -> Self
    where
        C: Fn() + Send + 'static,
    {
        let (tx, rx) = channel();
        let _ = std::thread::spawn(move || unsafe {
            let callback: Box<C> = Box::new(callback);
            let callback = Box::into_raw(callback) as *mut C;
            let mut notifier: io_object_t = 0;
            let mut run_loop_source: IONotificationPortRef = ptr::null_mut();
            let run_loop = {
                let run_loop = CFRunLoopGetCurrent();
                CFRetain(run_loop as *const c_void);
                run_loop
            };

            let port = IORegisterForSystemPower(
                callback as _,
                &mut run_loop_source,
                power_callback::<C>,
                &mut notifier,
            );

            if port == 0 {
                CFRelease(run_loop as *const c_void);
                return;
            }
            tx.send(CFRunLoop(run_loop)).unwrap();
            CFRunLoopAddSource(
                run_loop,
                IONotificationPortGetRunLoopSource(run_loop_source),
                kCFRunLoopCommonModes,
            );
            CFRunLoopRun();
            //release the callback from raw
            let _callback = Box::from_raw(callback);
            CFRunLoopRemoveSource(
                run_loop,
                IONotificationPortGetRunLoopSource(run_loop_source),
                kCFRunLoopCommonModes,
            );
        });
        let run_loop = rx.recv().unwrap();
        Self { run_loop }
    }
}

impl Drop for PowerNotify {
    fn drop(&mut self) {
        unsafe {
            CFRunLoopStop(self.run_loop.0);
        }
    }
}
