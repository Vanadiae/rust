//! Implementation of various bits and pieces of the `panic!` macro and
//! associated runtime pieces.
//!
//! Specifically, this module contains the implementation of:
//!
//! * Panic hooks
//! * Executing a panic up to doing the actual implementation
//! * Shims around "try"

#![deny(unsafe_op_in_unsafe_fn)]

use crate::panic::BacktraceStyle;
use core::panic::{BoxMeUp, Location, PanicInfo};

use crate::any::Any;
use crate::fmt;
use crate::intrinsics;
use crate::mem::{self, ManuallyDrop};
use crate::process;
use crate::sync::atomic::{AtomicBool, Ordering};
use crate::sync::{PoisonError, RwLock};
use crate::sys::stdio::panic_output;
use crate::sys_common::backtrace;
use crate::sys_common::thread_info;
use crate::thread;

#[cfg(not(test))]
use crate::io::set_output_capture;
// make sure to use the stderr output configured
// by libtest in the real copy of std
#[cfg(test)]
use realstd::io::set_output_capture;

// Binary interface to the panic runtime that the standard library depends on.
//
// The standard library is tagged with `#![needs_panic_runtime]` (introduced in
// RFC 1513) to indicate that it requires some other crate tagged with
// `#![panic_runtime]` to exist somewhere. Each panic runtime is intended to
// implement these symbols (with the same signatures) so we can get matched up
// to them.
//
// One day this may look a little less ad-hoc with the compiler helping out to
// hook up these functions, but it is not this day!
#[allow(improper_ctypes)]
extern "C" {
    fn __rust_panic_cleanup(payload: *mut u8) -> *mut (dyn Any + Send + 'static);
}

extern "Rust" {
    /// `BoxMeUp` lazily performs allocation only when needed (this avoids
    /// allocations when using the "abort" panic runtime).
    fn __rust_start_panic(payload: &mut dyn BoxMeUp) -> u32;
}

/// This function is called by the panic runtime if FFI code catches a Rust
/// panic but doesn't rethrow it. We don't support this case since it messes
/// with our panic count.
#[cfg(not(test))]
#[rustc_std_internal_symbol]
extern "C" fn __rust_drop_panic() -> ! {
    rtabort!("Rust panics must be rethrown");
}

/// This function is called by the panic runtime if it catches an exception
/// object which does not correspond to a Rust panic.
#[cfg(not(test))]
#[rustc_std_internal_symbol]
extern "C" fn __rust_foreign_exception() -> ! {
    rtabort!("Rust cannot catch foreign exceptions");
}

enum Hook {
    Default,
    Custom(Box<dyn Fn(&PanicInfo<'_>) + 'static + Sync + Send>),
}

impl Hook {
    #[inline]
    fn into_box(self) -> Box<dyn Fn(&PanicInfo<'_>) + 'static + Sync + Send> {
        match self {
            Hook::Default => Box::new(default_hook),
            Hook::Custom(hook) => hook,
        }
    }
}

impl Default for Hook {
    #[inline]
    fn default() -> Hook {
        Hook::Default
    }
}

static HOOK: RwLock<Hook> = RwLock::new(Hook::Default);

/// Registers a custom panic hook, replacing the previously registered hook.
///
/// The panic hook is invoked when a thread panics, but before the panic runtime
/// is invoked. As such, the hook will run with both the aborting and unwinding
/// runtimes.
///
/// The default hook, which is registered at startup, prints a message to standard error and
/// generates a backtrace if requested. This behavior can be customized using the `set_hook` function.
/// The current hook can be retrieved while reinstating the default hook with the [`take_hook`]
/// function.
///
/// [`take_hook`]: ./fn.take_hook.html
///
/// The hook is provided with a `PanicInfo` struct which contains information
/// about the origin of the panic, including the payload passed to `panic!` and
/// the source code location from which the panic originated.
///
/// The panic hook is a global resource.
///
/// # Panics
///
/// Panics if called from a panicking thread.
///
/// # Examples
///
/// The following will print "Custom panic hook":
///
/// ```should_panic
/// use std::panic;
///
/// panic::set_hook(Box::new(|_| {
///     println!("Custom panic hook");
/// }));
///
/// panic!("Normal panic");
/// ```
#[stable(feature = "panic_hooks", since = "1.10.0")]
pub fn set_hook(hook: Box<dyn Fn(&PanicInfo<'_>) + 'static + Sync + Send>) {
    if thread::panicking() {
        panic!("cannot modify the panic hook from a panicking thread");
    }

    let new = Hook::Custom(hook);
    let mut hook = HOOK.write().unwrap_or_else(PoisonError::into_inner);
    let old = mem::replace(&mut *hook, new);
    drop(hook);
    // Only drop the old hook after releasing the lock to avoid deadlocking
    // if its destructor panics.
    drop(old);
}

/// Unregisters the current panic hook and returns it, registering the default hook
/// in its place.
///
/// *See also the function [`set_hook`].*
///
/// [`set_hook`]: ./fn.set_hook.html
///
/// If the default hook is registered it will be returned, but remain registered.
///
/// # Panics
///
/// Panics if called from a panicking thread.
///
/// # Examples
///
/// The following will print "Normal panic":
///
/// ```should_panic
/// use std::panic;
///
/// panic::set_hook(Box::new(|_| {
///     println!("Custom panic hook");
/// }));
///
/// let _ = panic::take_hook();
///
/// panic!("Normal panic");
/// ```
#[must_use]
#[stable(feature = "panic_hooks", since = "1.10.0")]
pub fn take_hook() -> Box<dyn Fn(&PanicInfo<'_>) + 'static + Sync + Send> {
    if thread::panicking() {
        panic!("cannot modify the panic hook from a panicking thread");
    }

    let mut hook = HOOK.write().unwrap_or_else(PoisonError::into_inner);
    let old_hook = mem::take(&mut *hook);
    drop(hook);

    old_hook.into_box()
}

/// Atomic combination of [`take_hook`] and [`set_hook`]. Use this to replace the panic handler with
/// a new panic handler that does something and then executes the old handler.
///
/// [`take_hook`]: ./fn.take_hook.html
/// [`set_hook`]: ./fn.set_hook.html
///
/// # Panics
///
/// Panics if called from a panicking thread.
///
/// # Examples
///
/// The following will print the custom message, and then the normal output of panic.
///
/// ```should_panic
/// #![feature(panic_update_hook)]
/// use std::panic;
///
/// // Equivalent to
/// // let prev = panic::take_hook();
/// // panic::set_hook(move |info| {
/// //     println!("...");
/// //     prev(info);
/// // );
/// panic::update_hook(move |prev, info| {
///     println!("Print custom message and execute panic handler as usual");
///     prev(info);
/// });
///
/// panic!("Custom and then normal");
/// ```
#[unstable(feature = "panic_update_hook", issue = "92649")]
pub fn update_hook<F>(hook_fn: F)
where
    F: Fn(&(dyn Fn(&PanicInfo<'_>) + Send + Sync + 'static), &PanicInfo<'_>)
        + Sync
        + Send
        + 'static,
{
    if thread::panicking() {
        panic!("cannot modify the panic hook from a panicking thread");
    }

    let mut hook = HOOK.write().unwrap_or_else(PoisonError::into_inner);
    let prev = mem::take(&mut *hook).into_box();
    *hook = Hook::Custom(Box::new(move |info| hook_fn(&prev, info)));
}

/// The default panic handler.
fn default_hook(info: &PanicInfo<'_>) {
    // If this is a double panic, make sure that we print a backtrace
    // for this panic. Otherwise only print it if logging is enabled.
    let backtrace = if info.force_no_backtrace() {
        None
    } else if panic_count::get_count() >= 2 {
        BacktraceStyle::full()
    } else {
        crate::panic::get_backtrace_style()
    };

    // The current implementation always returns `Some`.
    let location = info.location().unwrap();

    let msg = match info.payload().downcast_ref::<&'static str>() {
        Some(s) => *s,
        None => match info.payload().downcast_ref::<String>() {
            Some(s) => &s[..],
            None => "Box<dyn Any>",
        },
    };
    let thread = thread_info::current_thread();
    let name = thread.as_ref().and_then(|t| t.name()).unwrap_or("<unnamed>");

    let write = |err: &mut dyn crate::io::Write| {
        let _ = writeln!(err, "thread '{name}' panicked at {location}:\n{msg}");

        static FIRST_PANIC: AtomicBool = AtomicBool::new(true);

        match backtrace {
            Some(BacktraceStyle::Short) => {
                drop(backtrace::print(err, crate::backtrace_rs::PrintFmt::Short))
            }
            Some(BacktraceStyle::Full) => {
                drop(backtrace::print(err, crate::backtrace_rs::PrintFmt::Full))
            }
            Some(BacktraceStyle::Off) => {
                if FIRST_PANIC.swap(false, Ordering::SeqCst) {
                    let _ = writeln!(
                        err,
                        "note: run with `RUST_BACKTRACE=1` environment variable to display a \
                             backtrace"
                    );
                }
            }
            // If backtraces aren't supported or are forced-off, do nothing.
            None => {}
        }
    };

    if let Some(local) = set_output_capture(None) {
        write(&mut *local.lock().unwrap_or_else(|e| e.into_inner()));
        set_output_capture(Some(local));
    } else if let Some(mut out) = panic_output() {
        write(&mut out);
    }
}

#[cfg(not(test))]
#[doc(hidden)]
#[unstable(feature = "update_panic_count", issue = "none")]
pub mod panic_count {
    use crate::cell::Cell;
    use crate::sync::atomic::{AtomicUsize, Ordering};

    pub const ALWAYS_ABORT_FLAG: usize = 1 << (usize::BITS - 1);

    /// A reason for forcing an immediate abort on panic.
    #[derive(Debug)]
    pub enum MustAbort {
        AlwaysAbort,
        PanicInHook,
    }

    // Panic count for the current thread and whether a panic hook is currently
    // being executed..
    thread_local! {
        static LOCAL_PANIC_COUNT: Cell<(usize, bool)> = const { Cell::new((0, false)) }
    }

    // Sum of panic counts from all threads. The purpose of this is to have
    // a fast path in `count_is_zero` (which is used by `panicking`). In any particular
    // thread, if that thread currently views `GLOBAL_PANIC_COUNT` as being zero,
    // then `LOCAL_PANIC_COUNT` in that thread is zero. This invariant holds before
    // and after increase and decrease, but not necessarily during their execution.
    //
    // Additionally, the top bit of GLOBAL_PANIC_COUNT (GLOBAL_ALWAYS_ABORT_FLAG)
    // records whether panic::always_abort() has been called. This can only be
    // set, never cleared.
    // panic::always_abort() is usually called to prevent memory allocations done by
    // the panic handling in the child created by `libc::fork`.
    // Memory allocations performed in a child created with `libc::fork` are undefined
    // behavior in most operating systems.
    // Accessing LOCAL_PANIC_COUNT in a child created by `libc::fork` would lead to a memory
    // allocation. Only GLOBAL_PANIC_COUNT can be accessed in this situation. This is
    // sufficient because a child process will always have exactly one thread only.
    // See also #85261 for details.
    //
    // This could be viewed as a struct containing a single bit and an n-1-bit
    // value, but if we wrote it like that it would be more than a single word,
    // and even a newtype around usize would be clumsy because we need atomics.
    // But we use such a tuple for the return type of increase().
    //
    // Stealing a bit is fine because it just amounts to assuming that each
    // panicking thread consumes at least 2 bytes of address space.
    static GLOBAL_PANIC_COUNT: AtomicUsize = AtomicUsize::new(0);

    // Increases the global and local panic count, and returns whether an
    // immediate abort is required.
    //
    // This also updates thread-local state to keep track of whether a panic
    // hook is currently executing.
    pub fn increase(run_panic_hook: bool) -> Option<MustAbort> {
        let global_count = GLOBAL_PANIC_COUNT.fetch_add(1, Ordering::Relaxed);
        if global_count & ALWAYS_ABORT_FLAG != 0 {
            return Some(MustAbort::AlwaysAbort);
        }

        LOCAL_PANIC_COUNT.with(|c| {
            let (count, in_panic_hook) = c.get();
            if in_panic_hook {
                return Some(MustAbort::PanicInHook);
            }
            c.set((count + 1, run_panic_hook));
            None
        })
    }

    pub fn finished_panic_hook() {
        LOCAL_PANIC_COUNT.with(|c| {
            let (count, _) = c.get();
            c.set((count, false));
        });
    }

    pub fn decrease() {
        GLOBAL_PANIC_COUNT.fetch_sub(1, Ordering::Relaxed);
        LOCAL_PANIC_COUNT.with(|c| {
            let (count, _) = c.get();
            c.set((count - 1, false));
        });
    }

    pub fn set_always_abort() {
        GLOBAL_PANIC_COUNT.fetch_or(ALWAYS_ABORT_FLAG, Ordering::Relaxed);
    }

    // Disregards ALWAYS_ABORT_FLAG
    #[must_use]
    pub fn get_count() -> usize {
        LOCAL_PANIC_COUNT.with(|c| c.get().0)
    }

    // Disregards ALWAYS_ABORT_FLAG
    #[must_use]
    #[inline]
    pub fn count_is_zero() -> bool {
        if GLOBAL_PANIC_COUNT.load(Ordering::Relaxed) & !ALWAYS_ABORT_FLAG == 0 {
            // Fast path: if `GLOBAL_PANIC_COUNT` is zero, all threads
            // (including the current one) will have `LOCAL_PANIC_COUNT`
            // equal to zero, so TLS access can be avoided.
            //
            // In terms of performance, a relaxed atomic load is similar to a normal
            // aligned memory read (e.g., a mov instruction in x86), but with some
            // compiler optimization restrictions. On the other hand, a TLS access
            // might require calling a non-inlinable function (such as `__tls_get_addr`
            // when using the GD TLS model).
            true
        } else {
            is_zero_slow_path()
        }
    }

    // Slow path is in a separate function to reduce the amount of code
    // inlined from `count_is_zero`.
    #[inline(never)]
    #[cold]
    fn is_zero_slow_path() -> bool {
        LOCAL_PANIC_COUNT.with(|c| c.get().0 == 0)
    }
}

#[cfg(test)]
pub use realstd::rt::panic_count;

/// Invoke a closure, capturing the cause of an unwinding panic if one occurs.
pub unsafe fn r#try<R, F: FnOnce() -> R>(f: F) -> Result<R, Box<dyn Any + Send>> {
    union Data<F, R> {
        f: ManuallyDrop<F>,
        r: ManuallyDrop<R>,
        p: ManuallyDrop<Box<dyn Any + Send>>,
    }

    // We do some sketchy operations with ownership here for the sake of
    // performance. We can only pass pointers down to `do_call` (can't pass
    // objects by value), so we do all the ownership tracking here manually
    // using a union.
    //
    // We go through a transition where:
    //
    // * First, we set the data field `f` to be the argumentless closure that we're going to call.
    // * When we make the function call, the `do_call` function below, we take
    //   ownership of the function pointer. At this point the `data` union is
    //   entirely uninitialized.
    // * If the closure successfully returns, we write the return value into the
    //   data's return slot (field `r`).
    // * If the closure panics (`do_catch` below), we write the panic payload into field `p`.
    // * Finally, when we come back out of the `try` intrinsic we're
    //   in one of two states:
    //
    //      1. The closure didn't panic, in which case the return value was
    //         filled in. We move it out of `data.r` and return it.
    //      2. The closure panicked, in which case the panic payload was
    //         filled in. We move it out of `data.p` and return it.
    //
    // Once we stack all that together we should have the "most efficient'
    // method of calling a catch panic whilst juggling ownership.
    let mut data = Data { f: ManuallyDrop::new(f) };

    let data_ptr = &mut data as *mut _ as *mut u8;
    // SAFETY:
    //
    // Access to the union's fields: this is `std` and we know that the `r#try`
    // intrinsic fills in the `r` or `p` union field based on its return value.
    //
    // The call to `intrinsics::r#try` is made safe by:
    // - `do_call`, the first argument, can be called with the initial `data_ptr`.
    // - `do_catch`, the second argument, can be called with the `data_ptr` as well.
    // See their safety preconditions for more information
    unsafe {
        return if intrinsics::r#try(do_call::<F, R>, data_ptr, do_catch::<F, R>) == 0 {
            Ok(ManuallyDrop::into_inner(data.r))
        } else {
            Err(ManuallyDrop::into_inner(data.p))
        };
    }

    // We consider unwinding to be rare, so mark this function as cold. However,
    // do not mark it no-inline -- that decision is best to leave to the
    // optimizer (in most cases this function is not inlined even as a normal,
    // non-cold function, though, as of the writing of this comment).
    #[cold]
    unsafe fn cleanup(payload: *mut u8) -> Box<dyn Any + Send + 'static> {
        // SAFETY: The whole unsafe block hinges on a correct implementation of
        // the panic handler `__rust_panic_cleanup`. As such we can only
        // assume it returns the correct thing for `Box::from_raw` to work
        // without undefined behavior.
        let obj = unsafe { Box::from_raw(__rust_panic_cleanup(payload)) };
        panic_count::decrease();
        obj
    }

    // SAFETY:
    // data must be non-NUL, correctly aligned, and a pointer to a `Data<F, R>`
    // Its must contains a valid `f` (type: F) value that can be use to fill
    // `data.r`.
    //
    // This function cannot be marked as `unsafe` because `intrinsics::r#try`
    // expects normal function pointers.
    #[inline]
    fn do_call<F: FnOnce() -> R, R>(data: *mut u8) {
        // SAFETY: this is the responsibility of the caller, see above.
        unsafe {
            let data = data as *mut Data<F, R>;
            let data = &mut (*data);
            let f = ManuallyDrop::take(&mut data.f);
            data.r = ManuallyDrop::new(f());
        }
    }

    // We *do* want this part of the catch to be inlined: this allows the
    // compiler to properly track accesses to the Data union and optimize it
    // away most of the time.
    //
    // SAFETY:
    // data must be non-NUL, correctly aligned, and a pointer to a `Data<F, R>`
    // Since this uses `cleanup` it also hinges on a correct implementation of
    // `__rustc_panic_cleanup`.
    //
    // This function cannot be marked as `unsafe` because `intrinsics::r#try`
    // expects normal function pointers.
    #[inline]
    #[rustc_nounwind] // `intrinsic::r#try` requires catch fn to be nounwind
    fn do_catch<F: FnOnce() -> R, R>(data: *mut u8, payload: *mut u8) {
        // SAFETY: this is the responsibility of the caller, see above.
        //
        // When `__rustc_panic_cleaner` is correctly implemented we can rely
        // on `obj` being the correct thing to pass to `data.p` (after wrapping
        // in `ManuallyDrop`).
        unsafe {
            let data = data as *mut Data<F, R>;
            let data = &mut (*data);
            let obj = cleanup(payload);
            data.p = ManuallyDrop::new(obj);
        }
    }
}

/// Determines whether the current thread is unwinding because of panic.
#[inline]
pub fn panicking() -> bool {
    !panic_count::count_is_zero()
}

/// Entry point of panics from the core crate (`panic_impl` lang item).
#[cfg(not(test))]
#[panic_handler]
pub fn begin_panic_handler(info: &PanicInfo<'_>) -> ! {
    struct PanicPayload<'a> {
        inner: &'a fmt::Arguments<'a>,
        string: Option<String>,
    }

    impl<'a> PanicPayload<'a> {
        fn new(inner: &'a fmt::Arguments<'a>) -> PanicPayload<'a> {
            PanicPayload { inner, string: None }
        }

        fn fill(&mut self) -> &mut String {
            use crate::fmt::Write;

            let inner = self.inner;
            // Lazily, the first time this gets called, run the actual string formatting.
            self.string.get_or_insert_with(|| {
                let mut s = String::new();
                let _err = s.write_fmt(*inner);
                s
            })
        }
    }

    unsafe impl<'a> BoxMeUp for PanicPayload<'a> {
        fn take_box(&mut self) -> *mut (dyn Any + Send) {
            // We do two allocations here, unfortunately. But (a) they're required with the current
            // scheme, and (b) we don't handle panic + OOM properly anyway (see comment in
            // begin_panic below).
            let contents = mem::take(self.fill());
            Box::into_raw(Box::new(contents))
        }

        fn get(&mut self) -> &(dyn Any + Send) {
            self.fill()
        }
    }

    struct StrPanicPayload(&'static str);

    unsafe impl BoxMeUp for StrPanicPayload {
        fn take_box(&mut self) -> *mut (dyn Any + Send) {
            Box::into_raw(Box::new(self.0))
        }

        fn get(&mut self) -> &(dyn Any + Send) {
            &self.0
        }
    }

    let loc = info.location().unwrap(); // The current implementation always returns Some
    let msg = info.message().unwrap(); // The current implementation always returns Some
    crate::sys_common::backtrace::__rust_end_short_backtrace(move || {
        // FIXME: can we just pass `info` along rather than taking it apart here, only to have
        // `rust_panic_with_hook` construct a new `PanicInfo`?
        if let Some(msg) = msg.as_str() {
            rust_panic_with_hook(
                &mut StrPanicPayload(msg),
                info.message(),
                loc,
                info.can_unwind(),
                info.force_no_backtrace(),
            );
        } else {
            rust_panic_with_hook(
                &mut PanicPayload::new(msg),
                info.message(),
                loc,
                info.can_unwind(),
                info.force_no_backtrace(),
            );
        }
    })
}

/// This is the entry point of panicking for the non-format-string variants of
/// panic!() and assert!(). In particular, this is the only entry point that supports
/// arbitrary payloads, not just format strings.
#[unstable(feature = "libstd_sys_internals", reason = "used by the panic! macro", issue = "none")]
#[cfg_attr(not(test), lang = "begin_panic")]
// lang item for CTFE panic support
// never inline unless panic_immediate_abort to avoid code
// bloat at the call sites as much as possible
#[cfg_attr(not(feature = "panic_immediate_abort"), inline(never), cold)]
#[cfg_attr(feature = "panic_immediate_abort", inline)]
#[track_caller]
#[rustc_do_not_const_check] // hooked by const-eval
pub const fn begin_panic<M: Any + Send>(msg: M) -> ! {
    if cfg!(feature = "panic_immediate_abort") {
        intrinsics::abort()
    }

    let loc = Location::caller();
    return crate::sys_common::backtrace::__rust_end_short_backtrace(move || {
        rust_panic_with_hook(
            &mut PanicPayload::new(msg),
            None,
            loc,
            /* can_unwind */ true,
            /* force_no_backtrace */ false,
        )
    });

    struct PanicPayload<A> {
        inner: Option<A>,
    }

    impl<A: Send + 'static> PanicPayload<A> {
        fn new(inner: A) -> PanicPayload<A> {
            PanicPayload { inner: Some(inner) }
        }
    }

    unsafe impl<A: Send + 'static> BoxMeUp for PanicPayload<A> {
        fn take_box(&mut self) -> *mut (dyn Any + Send) {
            // Note that this should be the only allocation performed in this code path. Currently
            // this means that panic!() on OOM will invoke this code path, but then again we're not
            // really ready for panic on OOM anyway. If we do start doing this, then we should
            // propagate this allocation to be performed in the parent of this thread instead of the
            // thread that's panicking.
            let data = match self.inner.take() {
                Some(a) => Box::new(a) as Box<dyn Any + Send>,
                None => process::abort(),
            };
            Box::into_raw(data)
        }

        fn get(&mut self) -> &(dyn Any + Send) {
            match self.inner {
                Some(ref a) => a,
                None => process::abort(),
            }
        }
    }
}

/// Central point for dispatching panics.
///
/// Executes the primary logic for a panic, including checking for recursive
/// panics, panic hooks, and finally dispatching to the panic runtime to either
/// abort or unwind.
fn rust_panic_with_hook(
    payload: &mut dyn BoxMeUp,
    message: Option<&fmt::Arguments<'_>>,
    location: &Location<'_>,
    can_unwind: bool,
    force_no_backtrace: bool,
) -> ! {
    let must_abort = panic_count::increase(true);

    // Check if we need to abort immediately.
    if let Some(must_abort) = must_abort {
        match must_abort {
            panic_count::MustAbort::PanicInHook => {
                // Don't try to print the message in this case
                // - perhaps that is causing the recursive panics.
                rtprintpanic!("thread panicked while processing panic. aborting.\n");
            }
            panic_count::MustAbort::AlwaysAbort => {
                // Unfortunately, this does not print a backtrace, because creating
                // a `Backtrace` will allocate, which we must to avoid here.
                let panicinfo = PanicInfo::internal_constructor(
                    message,
                    location,
                    can_unwind,
                    force_no_backtrace,
                );
                rtprintpanic!("{panicinfo}\npanicked after panic::always_abort(), aborting.\n");
            }
        }
        crate::sys::abort_internal();
    }

    let mut info =
        PanicInfo::internal_constructor(message, location, can_unwind, force_no_backtrace);
    let hook = HOOK.read().unwrap_or_else(PoisonError::into_inner);
    match *hook {
        // Some platforms (like wasm) know that printing to stderr won't ever actually
        // print anything, and if that's the case we can skip the default
        // hook. Since string formatting happens lazily when calling `payload`
        // methods, this means we avoid formatting the string at all!
        // (The panic runtime might still call `payload.take_box()` though and trigger
        // formatting.)
        Hook::Default if panic_output().is_none() => {}
        Hook::Default => {
            info.set_payload(payload.get());
            default_hook(&info);
        }
        Hook::Custom(ref hook) => {
            info.set_payload(payload.get());
            hook(&info);
        }
    };
    drop(hook);

    // Indicate that we have finished executing the panic hook. After this point
    // it is fine if there is a panic while executing destructors, as long as it
    // it contained within a `catch_unwind`.
    panic_count::finished_panic_hook();

    if !can_unwind {
        // If a thread panics while running destructors or tries to unwind
        // through a nounwind function (e.g. extern "C") then we cannot continue
        // unwinding and have to abort immediately.
        rtprintpanic!("thread caused non-unwinding panic. aborting.\n");
        crate::sys::abort_internal();
    }

    rust_panic(payload)
}

/// This is the entry point for `resume_unwind`.
/// It just forwards the payload to the panic runtime.
pub fn rust_panic_without_hook(payload: Box<dyn Any + Send>) -> ! {
    panic_count::increase(false);

    struct RewrapBox(Box<dyn Any + Send>);

    unsafe impl BoxMeUp for RewrapBox {
        fn take_box(&mut self) -> *mut (dyn Any + Send) {
            Box::into_raw(mem::replace(&mut self.0, Box::new(())))
        }

        fn get(&mut self) -> &(dyn Any + Send) {
            &*self.0
        }
    }

    rust_panic(&mut RewrapBox(payload))
}

/// An unmangled function (through `rustc_std_internal_symbol`) on which to slap
/// yer breakpoints.
#[inline(never)]
#[cfg_attr(not(test), rustc_std_internal_symbol)]
fn rust_panic(msg: &mut dyn BoxMeUp) -> ! {
    let code = unsafe { __rust_start_panic(msg) };
    rtabort!("failed to initiate panic, error {code}")
}
