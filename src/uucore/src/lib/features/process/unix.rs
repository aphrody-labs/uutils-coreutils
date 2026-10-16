// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

// spell-checker:ignore sigwait KTIME timeval itimerval setitimer itimer timerid
// spell-checker:ignore sigevent sigev sigval itimerspec signo clockid sevp

#[cfg(not(any(target_os = "fuchsia", target_os = "haiku")))]
use libc::pid_t;
#[cfg(not(any(target_os = "fuchsia", target_os = "haiku")))]
use nix::sys::signal::{self as nix_signal, SigHandler};
use nix::sys::signal::{SigSet, Signal};
#[cfg(not(any(target_os = "fuchsia", target_os = "haiku")))]
use nix::unistd::Pid;
use rustix::process::Signal as RixSignal;
use std::io;
#[cfg(not(any(target_os = "fuchsia", target_os = "haiku")))]
use std::process::Child;
#[cfg(not(any(target_os = "fuchsia", target_os = "haiku")))]
use std::time::{Duration, Instant};
#[cfg(not(any(target_os = "fuchsia", target_os = "haiku")))]
use timer::Timer;

#[cfg(not(any(target_os = "fuchsia", target_os = "haiku")))]
use super::{ChildExt, TimeoutRet};

#[cfg(not(any(target_os = "fuchsia", target_os = "haiku")))]
impl ChildExt for Child {
    fn send_signal(&mut self, signal: usize) -> io::Result<()> {
        let pid = Pid::from_raw(self.id() as pid_t);
        let result = if signal == 0 {
            nix_signal::kill(pid, None)
        } else {
            let signal = Signal::try_from(signal as i32)
                .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
            nix_signal::kill(pid, Some(signal))
        };
        result.map_err(|e| io::Error::from_raw_os_error(e as i32))
    }

    fn send_signal_group(&mut self, signal: usize) -> io::Result<()> {
        // Send signal to our process group (group 0 = caller's group).
        // This matches GNU coreutils behavior: if the child has remained in our
        // process group, it will receive this signal along with all other processes
        // in the group. If the child has created its own process group (via setpgid),
        // it won't receive this group signal, but will have received the direct signal.

        // Signal 0 is special - it just checks if process exists, doesn't send anything.
        // No need to manipulate signal handlers for it.
        if signal == 0 {
            return nix_signal::kill(Pid::from_raw(0), None)
                .map_err(|e| io::Error::from_raw_os_error(e as i32));
        }

        let signal = Signal::try_from(signal as i32)
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;

        // Ignore the signal temporarily so we don't receive it ourselves.
        let old_handler = unsafe { nix_signal::signal(signal, SigHandler::SigIgn) }
            .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
        let result = nix_signal::kill(Pid::from_raw(0), Some(signal));
        // Restore the old handler
        let _ = unsafe { nix_signal::signal(signal, old_handler) };
        result.map_err(|e| io::Error::from_raw_os_error(e as i32))
    }

    fn wait_or_timeout(&mut self, timeout: Duration, ignore_term: bool) -> io::Result<TimeoutRet> {
        if timeout == Duration::from_micros(0) {
            return self.wait().map(TimeoutRet::Exited);
        }
        // .try_wait() doesn't drop stdin, so we do it manually
        drop(self.stdin.take());

        // Waits continuously whenever we receive an external SIGCHLD or
        // we SIGTERM when we are ignoring them.
        let start = Instant::now();
        let mut remaining = timeout;
        let mut timer = Timer::new()?;
        loop {
            match timer.timed_sigwait(remaining) {
                Ok(None) => break Ok(TimeoutRet::TimedOut),
                Ok(Some(Signal::SIGCHLD)) => {
                    if let Some(status) = self.try_wait()? {
                        break Ok(TimeoutRet::Exited(status));
                    } // otherwise waits again
                }
                Ok(Some(Signal::SIGTERM)) if ignore_term => {} // waits again
                Ok(Some(signal)) => break Ok(TimeoutRet::Interrupted(signal as usize)),
                Err(e) => break Err(e),
            }
            remaining = timeout.saturating_sub(start.elapsed());
        }
    }
}

/// These signals must be blocked before calling [`ChildExt::wait_or_timeout`].
/// Consider unblocking them in the child's pre-exec hook.
pub fn timeout_signal_set() -> SigSet {
    let mut set = SigSet::empty();
    set.add(Signal::SIGALRM);
    set.add(Signal::SIGINT);
    set.add(Signal::SIGQUIT);
    set.add(Signal::SIGHUP);
    set.add(Signal::SIGTERM);
    set.add(Signal::SIGPIPE);
    set.add(Signal::SIGUSR1);
    set.add(Signal::SIGUSR2);
    set.add(Signal::SIGCHLD);
    set
}

/// Unblocks a signal from the current thread.
pub fn unblock_signal(signal: RixSignal) -> io::Result<()> {
    let mut set = SigSet::empty();
    // SAFETY: rustix's Signal is also a valid nix Signal.
    set.add(unsafe { Signal::try_from(signal.as_raw()).unwrap_unchecked() });

    set.thread_unblock().map_err(Into::into)
}

/// Ensures there is no overflow on time_t operations. Some BSDs (notably XNU)
/// will return EINVAL otherwise; POSIX only defines it up to 10e8, so we cap
/// it on all targets we do not trust to support the full integer range.
#[cfg(not(any(target_os = "fuchsia", target_os = "haiku")))]
const MAX_KTIME_T: Duration = if cfg!(target_os = "linux") {
    Duration::from_secs(9_223_372_036)
} else {
    Duration::from_secs(100_000_000)
};

/// Sets up a timer on SIGALRM for platforms that support POSIX.1-2008 realtime
/// clock extensions. Notably, both Android and Redox do not support the latter
/// fallback since it was removed in that same spec.
#[cfg(not(any(
    target_vendor = "apple",
    target_os = "fuchsia",
    target_os = "haiku",
    target_os = "openbsd",
    windows
)))]
mod timer {
    use super::MAX_KTIME_T;
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    use kernel_timer as timer_api;
    #[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
    use libc as timer_api;
    use std::io;
    use std::ptr::null_mut;
    use std::time::Duration;
    #[cfg(any(target_os = "redox", target_os = "android"))]
    use timer_sys as libc; // Complements their libc bindings.

    pub(super) struct Timer(timer_api::timer_t);

    impl Timer {
        pub(super) fn new() -> io::Result<Self> {
            use std::mem::MaybeUninit;

            // We must zero the reserved, private bits and other fields.
            // We cannot use nix or rustix because they don't support it in Redox.
            let mut sev = MaybeUninit::<libc::sigevent>::zeroed();

            unsafe {
                (*sev.as_mut_ptr()).sigev_notify = libc::SIGEV_SIGNAL;
                (*sev.as_mut_ptr()).sigev_signo = libc::SIGALRM;
            }

            // On cygwin, it's a u64; otherwise, a ptr with exposed provenance.
            let mut timer_id = MaybeUninit::zeroed();
            // SAFETY: All values are properly initialized.
            if unsafe {
                timer_api::timer_create(
                    libc::CLOCK_MONOTONIC,
                    sev.as_mut_ptr(),
                    timer_id.as_mut_ptr(),
                )
            } == -1
            {
                return Err(io::Error::last_os_error());
            }

            // SAFETY: `timer_create` returned success and initialized timer_id.
            Ok(Self(unsafe { timer_id.assume_init() }))
        }

        pub(super) fn arm(&mut self, timeout: Duration) -> Result<(), io::Error> {
            let timeout = timeout.min(MAX_KTIME_T).max(Duration::from_micros(1));
            // `timespec` has private padding members on time64 targets, so its
            // fields cannot be listed in a struct literal; start from the
            // zeroed default and fill in the ones we care about.
            let mut time = libc::itimerspec {
                it_interval: libc::timespec::default(),
                it_value: libc::timespec::default(),
            };
            time.it_value.tv_sec = timeout.as_secs() as _;
            time.it_value.tv_nsec = timeout.subsec_nanos() as _;

            // SAFETY: All values are properly initialized.
            if unsafe { timer_api::timer_settime(self.0, 0, &raw const time, null_mut()) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            unsafe { timer_api::timer_delete(self.0) };
        }
    }

    // Only this private SIGEV_SIGNAL timer uses kernel IDs. glibc's public timer IDs
    // also support SIGEV_THREAD and must not be mixed with these integer handles.
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    #[allow(non_camel_case_types)]
    mod kernel_timer {
        pub(super) type timer_t = libc::c_int;

        /// # Safety
        /// `event` must contain initialized SIGEV_SIGNAL data and `timer` must be writable.
        pub(super) unsafe fn timer_create(
            clock: libc::clockid_t,
            event: *mut libc::sigevent,
            timer: *mut timer_t,
        ) -> libc::c_int {
            // SAFETY: the caller provides initialized SIGEV_SIGNAL data and a writable kernel ID.
            unsafe { libc::syscall(libc::SYS_timer_create, clock, event, timer) as libc::c_int }
        }

        /// # Safety
        /// `time` must point to an initialized Linux64 timespec; `previous` is writable or null.
        pub(super) unsafe fn timer_settime(
            timer: timer_t,
            flags: libc::c_int,
            time: *const libc::itimerspec,
            previous: *mut libc::itimerspec,
        ) -> libc::c_int {
            // SAFETY: Linux64 uses this timespec layout; both pointers obey timer_settime's contract.
            unsafe {
                libc::syscall(libc::SYS_timer_settime, timer, flags, time, previous) as libc::c_int
            }
        }

        /// # Safety
        /// The caller must own this timer ID, so it cannot delete another owner's timer.
        pub(super) unsafe fn timer_delete(timer: timer_t) -> libc::c_int {
            // SAFETY: the kernel validates the integer ID and reports invalid handles through errno.
            unsafe { libc::syscall(libc::SYS_timer_delete, timer) as libc::c_int }
        }
    }

    #[cfg(all(test, target_os = "linux", target_pointer_width = "64"))]
    mod tests {
        use super::super::{ChildExt, TimeoutRet, timeout_signal_set};
        use super::Timer;
        use nix::sys::signal::SigSet;
        use std::io::Read;
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        struct RestoreMask(SigSet);
        impl Drop for RestoreMask {
            fn drop(&mut self) {
                let _ = self.0.thread_set_mask();
            }
        }

        fn timer_count() -> usize {
            std::fs::read_to_string("/proc/self/timers")
                .unwrap()
                .lines()
                .filter(|line| line.starts_with("ID:"))
                .count()
        }

        #[test]
        fn linux_kernel_timer_lifecycle() {
            let mode = std::env::var_os("UUTILS_KERNEL_TIMER_TEST");
            if mode.is_none() {
                let mut command = Command::new(std::env::current_exe().unwrap());
                let name = concat!(module_path!(), "::linux_kernel_timer_lifecycle")
                    .split_once("::")
                    .unwrap()
                    .1;
                command
                    .args(["--exact", name, "--nocapture", "--test-threads=1"])
                    .env("UUTILS_KERNEL_TIMER_TEST", "probe")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let set = timeout_signal_set();
                // SAFETY: sigprocmask is async-signal-safe; all test threads inherit the blocked set.
                unsafe {
                    command.pre_exec(move || {
                        if libc::sigprocmask(libc::SIG_BLOCK, set.as_ref(), std::ptr::null_mut())
                            == 0
                        {
                            Ok(())
                        } else {
                            Err(std::io::Error::last_os_error())
                        }
                    });
                }
                let mut child = command.spawn().unwrap();
                let mut stdout = child.stdout.take().unwrap();
                let mut stderr = child.stderr.take().unwrap();
                let (send, receive) = std::sync::mpsc::channel();
                let reader = std::thread::spawn(move || {
                    let mut output = String::new();
                    stdout.read_to_string(&mut output).unwrap();
                    stderr.read_to_string(&mut output).unwrap();
                    let _ = send.send(output);
                });
                let output = receive.recv_timeout(Duration::from_secs(5));
                if output.is_err() {
                    // This Child remains unreaped here, so only our owned process can be terminated.
                    let _ = child.kill();
                }
                let status = child.wait().unwrap();
                reader.join().unwrap();
                assert!(
                    output.is_ok(),
                    "isolated kernel timer gate exceeded its deadline"
                );
                let output = output.unwrap();
                assert!(output.contains("KERNEL_TIMER_LIFECYCLE_OK"), "{output}");
                assert!(status.success(), "{output}");
                return;
            }
            let original = SigSet::thread_get_mask().unwrap();
            let restore = RestoreMask(original);
            let mut changed = SigSet::empty();
            changed.add(nix::sys::signal::Signal::SIGWINCH);
            if original.contains(nix::sys::signal::Signal::SIGWINCH) {
                changed.thread_unblock().unwrap();
            } else {
                changed.thread_block().unwrap();
            }
            assert_ne!(
                SigSet::thread_get_mask()
                    .unwrap()
                    .contains(nix::sys::signal::Signal::SIGWINCH),
                original.contains(nix::sys::signal::Signal::SIGWINCH)
            );
            timeout_signal_set().thread_block().unwrap();
            let baseline = timer_count();
            let mut timer = Timer::new().unwrap();
            let id = timer.0;
            assert_eq!(timer_count(), baseline + 1);
            assert!(
                timer
                    .timed_sigwait(Duration::from_millis(1))
                    .unwrap()
                    .is_none()
            );
            timer.arm(Duration::from_secs(30)).unwrap();
            drop(timer);
            assert_eq!(timer_count(), baseline);
            assert_eq!(unsafe { libc::syscall(libc::SYS_timer_delete, id) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EINVAL)
            );
            let mut child = Command::new(std::env::current_exe().unwrap())
                .arg("--help")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let result = child.wait_or_timeout(Duration::from_secs(2), false);
            if !matches!(result, Ok(TimeoutRet::Exited(_))) {
                let _ = child.kill();
            }
            let status = child.wait().unwrap();
            assert!(matches!(result, Ok(TimeoutRet::Exited(status)) if status.success()));
            assert!(status.success());
            assert_eq!(timer_count(), baseline);
            drop(restore);
            let restored = SigSet::thread_get_mask().unwrap();
            for signal in &timeout_signal_set() {
                assert_eq!(restored.contains(signal), original.contains(signal));
            }
            assert_eq!(
                restored.contains(nix::sys::signal::Signal::SIGWINCH),
                original.contains(nix::sys::signal::Signal::SIGWINCH)
            );
            println!("KERNEL_TIMER_LIFECYCLE_OK");
        }
    }

    /// Complements the libc bindings of Redox and Android with missing items.
    #[cfg(any(target_os = "redox", target_os = "android"))]
    #[allow(non_camel_case_types)]
    mod timer_sys {
        pub(super) use libc::{CLOCK_MONOTONIC, SIGALRM, timespec};
        #[cfg(not(target_os = "redox"))]
        pub(super) use libc::{SIGEV_SIGNAL, sigevent};

        pub(super) type timer_t = *mut core::ffi::c_void;

        unsafe extern "C" {
            pub(super) fn timer_settime(
                timerid: timer_t,
                flags: core::ffi::c_int,
                new_value: *const itimerspec,
                old_value: *mut itimerspec,
            ) -> core::ffi::c_int;

            pub(super) fn timer_create(
                clockid: libc::clockid_t,
                sevp: *mut sigevent,
                timerid: *mut timer_t,
            ) -> core::ffi::c_int;

            pub(super) fn timer_delete(timerid: timer_t) -> core::ffi::c_int;
        }

        #[repr(C)]
        #[derive(Clone, Copy, Debug)]
        pub(super) struct itimerspec {
            pub(super) it_interval: timespec,
            pub(super) it_value: timespec,
        }

        #[cfg(target_os = "redox")]
        pub(super) const SIGEV_SIGNAL: core::ffi::c_int = 0;

        #[repr(C)]
        #[derive(Clone, Copy, Debug)]
        #[cfg(target_os = "redox")]
        pub(super) struct sigevent {
            pub(super) sigev_value: libc::sigval,
            pub(super) sigev_signo: core::ffi::c_int,
            pub(super) sigev_notify: core::ffi::c_int,
            pub(super) sigev_notify_thread_id: core::ffi::c_int,
            #[cfg(target_pointer_width = "64")]
            __unused1: std::mem::MaybeUninit<[core::ffi::c_int; 11]>,
            #[cfg(target_pointer_width = "32")]
            __unused1: std::mem::MaybeUninit<[core::ffi::c_int; 12]>,
        }
    }
}

/// Sets up a timer on SIGALRM for platforms that do not support POSIX.1-2008
/// realtime clock extensions. Notably, Darwin, OpenBSD, and Windows.
#[cfg(any(target_vendor = "apple", target_os = "openbsd", windows))]
mod timer {
    use super::MAX_KTIME_T;
    use nix::errno::Errno;
    use std::io;
    use std::ptr::null_mut;
    use std::time::Duration;

    pub(super) struct Timer;

    impl Timer {
        #[allow(clippy::unnecessary_wraps)]
        pub(super) fn new() -> io::Result<Self> {
            Ok(Self)
        }

        pub(super) fn arm(&mut self, timeout: Duration) -> io::Result<()> {
            let timeout = timeout.min(MAX_KTIME_T).max(Duration::from_micros(1));
            let time = libc::itimerval {
                it_interval: libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
                it_value: libc::timeval {
                    tv_sec: timeout.as_secs() as _,
                    tv_usec: timeout.subsec_micros() as _,
                },
            };

            // SAFETY: All values are properly initialized.
            Errno::result(unsafe {
                libc::setitimer(libc::ITIMER_REAL, &raw const time, null_mut())
            })?;
            Ok(())
        }
    }
}

#[cfg(not(any(target_os = "fuchsia", target_os = "haiku")))]
impl Timer {
    fn timed_sigwait(&mut self, timeout: Duration) -> io::Result<Option<Signal>> {
        self.arm(timeout)?;

        let set = timeout_signal_set();
        let mut sig = 0;
        // SAFETY: All values are properly initialized.
        let res = unsafe { libc::sigwait(set.as_ref(), &raw mut sig) };

        if res != 0 {
            Err(io::Error::from_raw_os_error(res))
        } else if sig == libc::SIGALRM {
            Ok(None)
        } else {
            Ok(Some(Signal::try_from(sig)?))
        }
    }
}
