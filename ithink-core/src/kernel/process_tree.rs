//! Native-process lifetime cleanup, not a security sandbox.
//!
//! Windows uses a kill-on-close Job Object; assignment happens immediately
//! after spawn, so a small spawn/assignment race remains. Unix starts a new
//! process group. Trusted plugins must not deliberately escape these groups.
use std::io;
use std::process::{Child, Command};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(windows)]
mod platform {
    use super::*;
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    pub(super) struct Tree {
        job: Option<OwnedHandle>,
    }
    impl Tree {
        pub(super) fn prepare(_: &mut Command) -> io::Result<Self> {
            // SAFETY: null attributes/name request a private, non-inherited job.
            let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a successful CreateJobObjectW returns a uniquely owned handle.
            let job = unsafe { OwnedHandle::from_raw_handle(raw) };
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: the buffer is the exact documented structure and size.
            if unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { job: Some(job) })
        }
        pub(super) fn attach(&mut self, child: &Child) -> io::Result<()> {
            let job = self.job.as_ref().expect("job prepared before spawn");
            // SAFETY: both handles remain valid for the duration of this call.
            if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        pub(super) fn terminate(&mut self) {
            if let Some(job) = self.job.take() {
                // SAFETY: this private job belongs only to the plugin tree.
                unsafe {
                    TerminateJobObject(job.as_raw_handle(), 1);
                }
                // Closing is an additional kill-on-close backstop even if terminate failed.
                drop(job);
            }
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            self.terminate();
        }
    }

    pub(super) fn interrupt(thread: &JoinHandle<()>) {
        // SAFETY: JoinHandle owns the live thread handle. A completed thread or
        // no pending synchronous I/O simply makes this best-effort call fail.
        unsafe {
            windows_sys::Win32::System::IO::CancelSynchronousIo(thread.as_raw_handle());
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::process::CommandExt;

    unsafe extern "C" {
        fn kill(pid: std::os::raw::c_int, signal: std::os::raw::c_int) -> std::os::raw::c_int;
    }
    pub(super) struct Tree {
        group: Option<std::os::raw::c_int>,
    }
    impl Tree {
        pub(super) fn prepare(command: &mut Command) -> io::Result<Self> {
            command.process_group(0);
            Ok(Self { group: None })
        }
        pub(super) fn attach(&mut self, child: &Child) -> io::Result<()> {
            let group = std::os::raw::c_int::try_from(child.id())
                .map_err(|_| io::Error::other("child PID exceeds process-group range"))?;
            if group <= 0 {
                return Err(io::Error::other("invalid child process group"));
            }
            self.group = Some(group);
            Ok(())
        }
        pub(super) fn terminate(&mut self) {
            if let Some(group) = self.group.take() {
                // SAFETY: a negative PID targets the private group created by
                // CommandExt::process_group(0); SIGKILL=9 on supported Unix targets.
                unsafe {
                    kill(-group, 9);
                }
            }
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            self.terminate();
        }
    }
    pub(super) fn interrupt(_: &JoinHandle<()>) {}
}

#[cfg(not(any(windows, unix)))]
mod platform {
    use super::*;
    pub(super) struct Tree;
    impl Tree {
        pub(super) fn prepare(_: &mut Command) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "native process-tree cleanup is unsupported on this platform",
            ))
        }
        pub(super) fn attach(&mut self, _: &Child) -> io::Result<()> {
            Ok(())
        }
        pub(super) fn terminate(&mut self) {}
    }
    pub(super) fn interrupt(_: &JoinHandle<()>) {}
}

/// Prepares lifetime containment before spawn and immediately attaches the child.
/// Assignment failure never returns an unmanaged live child to the caller.
pub(crate) struct ProcessTree(platform::Tree);
impl ProcessTree {
    pub(crate) fn prepare(command: &mut Command) -> io::Result<Self> {
        platform::Tree::prepare(command).map(Self)
    }
    pub(crate) fn attach(&mut self, child: &Child) -> io::Result<()> {
        self.0.attach(child)
    }
    fn terminate(&mut self) {
        self.0.terminate();
    }
}

struct Cleanup {
    child: Option<Child>,
    threads: Vec<JoinHandle<()>>,
}
impl Cleanup {
    fn poll(&mut self) -> bool {
        if let Some(child) = &mut self.child {
            if matches!(child.try_wait(), Ok(Some(_))) {
                self.child.take();
            }
        }
        let mut index = 0;
        while index < self.threads.len() {
            if self.threads[index].is_finished() {
                let handle = self.threads.swap_remove(index);
                let _ = handle.join();
            } else {
                platform::interrupt(&self.threads[index]);
                index += 1;
            }
        }
        self.child.is_none() && self.threads.is_empty()
    }
}

// One cleanup service, not a blocking waiter for every retired plugin. Handles
// stay owned until they finish; failed native cleanup never blocks Runtime::pump.
struct Reaper {
    pending: Mutex<Vec<Cleanup>>,
    wake: Condvar,
}
static REAPER: OnceLock<Arc<Reaper>> = OnceLock::new();
static REAPER_START: Mutex<()> = Mutex::new(());

pub(crate) fn ensure_reaper() -> io::Result<()> {
    if REAPER.get().is_some() {
        return Ok(());
    }
    let _start = REAPER_START.lock().unwrap_or_else(|p| p.into_inner());
    if REAPER.get().is_some() {
        return Ok(());
    }
    let reaper = Arc::new(Reaper {
        pending: Mutex::new(Vec::new()),
        wake: Condvar::new(),
    });
    let worker = Arc::clone(&reaper);
    thread::Builder::new()
        .name("ithink-process-reaper".into())
        .spawn(move || {
            let mut work: Vec<Cleanup> = Vec::new();
            loop {
                {
                    let mut pending = worker.pending.lock().unwrap_or_else(|p| p.into_inner());
                    if work.is_empty() {
                        while pending.is_empty() {
                            pending = worker.wake.wait(pending).unwrap_or_else(|p| p.into_inner());
                        }
                    } else if pending.is_empty() {
                        let waited = worker
                            .wake
                            .wait_timeout(pending, Duration::from_millis(5))
                            .unwrap_or_else(|p| p.into_inner());
                        pending = waited.0;
                    }
                    work.append(&mut *pending);
                }
                // Never hold the submission lock while asking the OS about exit.
                work.retain_mut(|cleanup| !cleanup.poll());
            }
        })?;
    // Initialization is serialized; only a successfully started worker is published.
    let _ = REAPER.set(reaper);
    Ok(())
}

/// Terminate the whole managed tree, then reap already-finished resources.
/// Anything still exiting is moved to the shared reaper without blocking pump.
pub(crate) fn stop(mut tree: ProcessTree, mut child: Child, threads: Vec<JoinHandle<()>>) {
    tree.terminate();
    // Also kill the direct child if assignment failed or a platform call failed.
    let _ = child.kill();
    let mut cleanup = Cleanup {
        child: Some(child),
        threads,
    };
    if cleanup.poll() {
        return;
    }
    let reaper = REAPER
        .get()
        .expect("reaper is created before spawning plugins");
    reaper
        .pending
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(cleanup);
    reaper.wake.notify_one();
}
