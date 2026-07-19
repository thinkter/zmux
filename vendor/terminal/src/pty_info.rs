use gpui::{Context, Task};
use parking_lot::{Mutex, RwLock};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[cfg(not(target_os = "macos"))]
use parking_lot::{MappedRwLockReadGuard, RwLockReadGuard};

/// zmux patch: minimum spacing between foreground-process refreshes.
///
/// PTY wakeups arrive per output batch — hundreds per second while a TUI
/// animates a spinner — and upstream refreshes the process table on every
/// one. Pacing the refresh keeps title/process detection responsive while
/// bounding the syscall and allocation load to a few refreshes per second.
const REFRESH_MIN_INTERVAL: Duration = Duration::from_millis(200);

#[cfg(target_os = "windows")]
use windows::Win32::{Foundation::HANDLE, System::Threading::GetProcessId};

use sysinfo::Pid;
#[cfg(not(target_os = "macos"))]
use sysinfo::{Process, ProcessRefreshKind, System, UpdateKind};

use crate::{Event, Terminal};

#[derive(Clone, Copy)]
pub struct ProcessIdGetter {
    handle: i32,
    fallback_pid: u32,
}

impl ProcessIdGetter {
    pub(crate) fn new(handle: i32, fallback_pid: u32) -> ProcessIdGetter {
        ProcessIdGetter {
            handle,
            fallback_pid,
        }
    }

    pub fn fallback_pid(&self) -> Pid {
        Pid::from_u32(self.fallback_pid)
    }
}

#[cfg(unix)]
impl ProcessIdGetter {
    fn pid(&self) -> Option<Pid> {
        // Negative pid means error.
        // Zero pid means no foreground process group is set on the PTY yet.
        // Avoid killing the current process by returning a zero pid.
        let pid = unsafe { libc::tcgetpgrp(self.handle) };
        if pid > 0 {
            return Some(Pid::from_u32(pid as u32));
        }

        if self.fallback_pid > 0 {
            return Some(Pid::from_u32(self.fallback_pid));
        }

        None
    }
}

#[cfg(windows)]
impl ProcessIdGetter {
    fn pid(&self) -> Option<Pid> {
        let pid = unsafe { GetProcessId(HANDLE(self.handle as _)) };
        // the GetProcessId may fail and returns zero, which will lead to a stack overflow issue
        if pid == 0 {
            // in the builder process, there is a small chance, almost negligible,
            // that this value could be zero, which means child_watcher returns None,
            // GetProcessId returns 0.
            if self.fallback_pid == 0 {
                return None;
            }
            return Some(Pid::from_u32(self.fallback_pid));
        }
        Some(Pid::from_u32(pid))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessInfo {
    pub(crate) pid: Pid,
    pub(crate) name: String,
    pub(crate) executable: Option<PathBuf>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) argv: Vec<String>,
    argv_valid: bool,
}

#[cfg(target_os = "macos")]
fn same_cached_identity(
    previous: Option<&ProcessInfo>,
    pid: Pid,
    executable: Option<&std::path::Path>,
) -> bool {
    previous.is_some_and(|previous| {
        previous.pid == pid
            && executable
                .is_none_or(|executable| previous.executable.as_deref() == Some(executable))
    })
}

#[cfg(target_os = "macos")]
fn should_refresh_argv(
    previous: Option<&ProcessInfo>,
    pid: Pid,
    executable: Option<&std::path::Path>,
) -> bool {
    !same_cached_identity(previous, pid, executable)
        || previous.is_none_or(|previous| !previous.argv_valid)
}

#[cfg(unix)]
fn child_signal_pid(pid: Pid) -> Option<libc::pid_t> {
    let pid = pid.as_u32();
    (pid > 0 && pid <= libc::pid_t::MAX as u32).then_some(pid as libc::pid_t)
}

#[cfg(target_os = "macos")]
fn foreground_pid_unchanged(initial: Pid, current: Option<Pid>) -> bool {
    current == Some(initial)
}

#[cfg(target_os = "macos")]
fn merge_macos_process_info(
    previous: Option<&ProcessInfo>,
    pid: Pid,
    cwd: Option<PathBuf>,
    executable: Option<PathBuf>,
    refreshed_argv: Option<Vec<String>>,
) -> ProcessInfo {
    let same_identity = same_cached_identity(previous, pid, executable.as_deref());
    let previous = same_identity.then_some(previous).flatten();
    let executable =
        executable.or_else(|| previous.and_then(|previous| previous.executable.clone()));
    let cwd = cwd.or_else(|| previous.and_then(|previous| previous.cwd.clone()));
    let argv_valid =
        refreshed_argv.is_some() || previous.is_some_and(|previous| previous.argv_valid);
    let argv = refreshed_argv.unwrap_or_else(|| {
        previous
            .map(|previous| previous.argv.clone())
            .unwrap_or_default()
    });
    let name = executable
        .as_deref()
        .and_then(std::path::Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .or_else(|| {
            argv.first().and_then(|argument| {
                std::path::Path::new(argument)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        })
        .unwrap_or_default();

    ProcessInfo {
        pid,
        name,
        executable,
        cwd,
        argv,
        argv_valid,
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::Pid;
    use std::{
        ffi::OsStr,
        mem::{MaybeUninit, size_of, size_of_val},
        os::unix::ffi::OsStrExt,
        path::PathBuf,
        ptr, slice,
    };

    pub(super) fn path_from_bounded_buffer(buffer: &[u8]) -> Option<PathBuf> {
        let end = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        (end > 0).then(|| PathBuf::from(OsStr::from_bytes(&buffer[..end])))
    }

    pub(super) fn process_cwd(pid: Pid) -> Option<PathBuf> {
        let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        let buffer_size = size_of::<libc::proc_vnodepathinfo>();
        let bytes_read = unsafe {
            libc::proc_pidinfo(
                pid.as_u32() as libc::c_int,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr().cast(),
                buffer_size as libc::c_int,
            )
        };
        if bytes_read < buffer_size as libc::c_int {
            return None;
        }

        let info = unsafe { info.assume_init() };
        let path = unsafe {
            slice::from_raw_parts(
                info.pvi_cdir.vip_path.as_ptr().cast::<u8>(),
                size_of_val(&info.pvi_cdir.vip_path),
            )
        };
        path_from_bounded_buffer(path)
    }

    pub(super) fn process_executable(pid: Pid) -> Option<PathBuf> {
        let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let bytes_read = unsafe {
            libc::proc_pidpath(
                pid.as_u32() as libc::c_int,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
            )
        };
        if bytes_read <= 0 {
            return None;
        }

        let bytes_read = (bytes_read as usize).min(buffer.len());
        path_from_bounded_buffer(&buffer[..bytes_read])
    }

    pub(super) fn process_argv(pid: Pid) -> Option<Vec<String>> {
        let mut mib = [
            libc::CTL_KERN,
            libc::KERN_PROCARGS2,
            pid.as_u32() as libc::c_int,
        ];
        let mut buffer_size = 0;
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                ptr::null_mut(),
                &mut buffer_size,
                ptr::null_mut(),
                0,
            )
        } != 0
            || buffer_size == 0
        {
            return None;
        }

        let mut buffer = vec![0_u8; buffer_size];
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                buffer.as_mut_ptr().cast(),
                &mut buffer_size,
                ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        buffer.truncate(buffer_size);
        parse_procargs2(&buffer)
    }

    pub(super) fn parse_procargs2(buffer: &[u8]) -> Option<Vec<String>> {
        let argc_bytes: [u8; size_of::<libc::c_int>()] =
            buffer.get(..size_of::<libc::c_int>())?.try_into().ok()?;
        let argc = libc::c_int::from_ne_bytes(argc_bytes);
        if argc < 0 {
            return None;
        }

        let mut cursor = size_of::<libc::c_int>();
        cursor += buffer.get(cursor..)?.iter().position(|byte| *byte == 0)?;
        while buffer.get(cursor) == Some(&0) {
            cursor += 1;
        }

        let argc = argc as usize;
        if argc > buffer.len().saturating_sub(cursor) {
            return None;
        }
        let mut argv = Vec::with_capacity(argc);
        for _ in 0..argc {
            let remaining = buffer.get(cursor..)?;
            let argument_len = remaining.iter().position(|byte| *byte == 0)?;
            argv.push(String::from_utf8_lossy(&remaining[..argument_len]).into_owned());
            cursor += argument_len + 1;
        }
        Some(argv)
    }
}

/// Fetches Zed-relevant Pseudo-Terminal (PTY) process information
#[derive(Debug, Default, PartialEq, Eq)]
struct ProcessRefreshLatch {
    running: bool,
    pending: bool,
}

impl ProcessRefreshLatch {
    /// Returns true when this wakeup must start a probe immediately.
    fn wakeup(&mut self) -> bool {
        if self.running {
            self.pending = true;
            false
        } else {
            self.running = true;
            true
        }
    }

    /// Completes the active probe and returns whether one trailing probe was
    /// requested. The caller re-enters `wakeup` synchronously for that probe.
    fn completed(&mut self) -> bool {
        debug_assert!(self.running);
        self.running = false;
        std::mem::take(&mut self.pending)
    }
}

#[derive(Default)]
struct ProcessRefreshTaskState {
    latch: ProcessRefreshLatch,
    task: Option<Task<()>>,
}

pub(crate) struct PtyProcessInfo {
    #[cfg(not(target_os = "macos"))]
    system: RwLock<System>,
    #[cfg(not(target_os = "macos"))]
    refresh_kind: ProcessRefreshKind,
    pid_getter: ProcessIdGetter,
    pub(crate) current: RwLock<Option<ProcessInfo>>,
    refresh_state: Mutex<ProcessRefreshTaskState>,
}

impl PtyProcessInfo {
    pub(crate) fn new(pid_getter: ProcessIdGetter) -> PtyProcessInfo {
        #[cfg(not(target_os = "macos"))]
        let process_refresh_kind = ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .with_exe(UpdateKind::Always);
        #[cfg(not(target_os = "macos"))]
        let system = System::new();

        PtyProcessInfo {
            #[cfg(not(target_os = "macos"))]
            system: RwLock::new(system),
            #[cfg(not(target_os = "macos"))]
            refresh_kind: process_refresh_kind,
            pid_getter,
            current: RwLock::new(None),
            refresh_state: Mutex::new(ProcessRefreshTaskState::default()),
        }
    }

    pub(crate) fn pid_getter(&self) -> &ProcessIdGetter {
        &self.pid_getter
    }

    #[cfg(not(target_os = "macos"))]
    fn refresh(&self) -> Option<MappedRwLockReadGuard<'_, Process>> {
        let pid = self.pid_getter.pid()?;
        if self.system.write().refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            true,
            self.refresh_kind,
        ) == 1
        {
            RwLockReadGuard::try_map(self.system.read(), |system| system.process(pid)).ok()
        } else {
            None
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn get_child(&self) -> Option<MappedRwLockReadGuard<'_, Process>> {
        let pid = self.pid_getter.fallback_pid();
        self.system.write().refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            true,
            self.refresh_kind,
        );
        RwLockReadGuard::try_map(self.system.read(), |system| system.process(pid)).ok()
    }

    #[cfg(unix)]
    pub(crate) fn kill_current_process(&self) -> bool {
        let Some(pid) = self.pid_getter.pid() else {
            return false;
        };
        unsafe { libc::killpg(pid.as_u32() as i32, libc::SIGKILL) == 0 }
    }

    #[cfg(not(unix))]
    pub(crate) fn kill_current_process(&self) -> bool {
        self.refresh().is_some_and(|process| process.kill())
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn kill_child_process(&self) -> bool {
        self.get_child().is_some_and(|process| process.kill())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn kill_child_process(&self) -> bool {
        let Some(pid) = child_signal_pid(self.pid_getter.fallback_pid()) else {
            return false;
        };
        unsafe { libc::kill(pid, libc::SIGKILL) == 0 }
    }

    #[cfg(unix)]
    pub(crate) fn terminate_child_process(&self) -> bool {
        let Some(pid) = child_signal_pid(self.pid_getter.fallback_pid()) else {
            return false;
        };
        unsafe { libc::killpg(pid, libc::SIGTERM) == 0 }
    }

    #[cfg(not(unix))]
    pub(crate) fn terminate_child_process(&self) -> bool {
        false
    }

    #[cfg(not(target_os = "macos"))]
    fn load(&self) -> Option<ProcessInfo> {
        let process = self.refresh()?;
        let pid = process.pid();

        let info = ProcessInfo {
            pid,
            name: process.name().to_str()?.to_owned(),
            executable: process.exe().map(PathBuf::from),
            cwd: process.cwd().map(PathBuf::from),
            argv: process
                .cmd()
                .iter()
                .filter_map(|s| s.to_str().map(ToOwned::to_owned))
                .collect(),
            argv_valid: true,
        };
        *self.current.write() = Some(info.clone());
        Some(info)
    }

    #[cfg(target_os = "macos")]
    fn load(&self) -> Option<ProcessInfo> {
        let pid = self.pid_getter.pid()?;
        let previous = self.current.read().clone();
        let cwd = macos::process_cwd(pid);
        let executable = macos::process_executable(pid);
        let refresh_argv = should_refresh_argv(previous.as_ref(), pid, executable.as_deref());
        let argv = refresh_argv.then(|| macos::process_argv(pid)).flatten();

        // The foreground process group can change while the three independent
        // process queries are in flight. Never publish a mixed snapshot.
        if !foreground_pid_unchanged(pid, self.pid_getter.pid()) {
            return previous;
        }

        let info = merge_macos_process_info(previous.as_ref(), pid, cwd, executable, argv);
        *self.current.write() = Some(info.clone());
        Some(info)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn load_for_test(&self) -> Option<ProcessInfo> {
        self.load()
    }

    /// Updates the cached process info, emitting a [`Event::TitleChanged`] event if the Zed-relevant info has changed
    pub(crate) fn emit_title_changed_if_changed(self: &Arc<Self>, cx: &mut Context<'_, Terminal>) {
        let mut refresh_state = self.refresh_state.lock();
        if !refresh_state.latch.wakeup() {
            return;
        }
        let this = self.clone();
        let executor = cx.background_executor().clone();
        let has_changed = cx.background_executor().spawn(async move {
            // zmux patch: coalesce wakeup bursts. The in-flight `task` guard
            // above keeps this to one delayed refresh per interval, and a
            // trailing wakeup still lands after the burst settles.
            executor.timer(REFRESH_MIN_INTERVAL).await;
            let previous = this.current.read().clone();
            let current = this.load();
            let has_changed = match (previous.as_ref(), current.as_ref()) {
                (None, None) => false,
                (Some(prev), Some(now)) => prev != now,
                _ => true,
            };
            if has_changed {
                *this.current.write() = current;
            }
            has_changed
        });
        let this = Arc::downgrade(self);
        refresh_state.task = Some(cx.spawn(async move |term, cx| {
            if has_changed.await {
                term.update(cx, |_, cx| cx.emit(Event::TitleChanged)).ok();
            }
            if let Some(this) = this.upgrade() {
                let trailing = {
                    let mut refresh_state = this.refresh_state.lock();
                    refresh_state.task.take();
                    refresh_state.latch.completed()
                };
                if trailing {
                    term.update(cx, |_, cx| this.emit_title_changed_if_changed(cx))
                        .ok();
                }
            }
        }));
    }

    pub(crate) fn pid(&self) -> Option<Pid> {
        self.pid_getter.pid()
    }
}

#[cfg(test)]
mod refresh_latch_tests {
    use super::*;

    #[test]
    fn in_flight_wakeups_collapse_to_one_trailing_probe_then_settle() {
        let mut latch = ProcessRefreshLatch::default();
        let mut probes = 0;

        if latch.wakeup() {
            probes += 1;
        }
        for _ in 0..32 {
            assert!(!latch.wakeup());
        }

        assert!(
            latch.completed(),
            "the burst must request one trailing probe"
        );
        if latch.wakeup() {
            probes += 1;
        }
        assert!(!latch.completed(), "the trailing probe must settle idle");

        assert_eq!(probes, 2);
        assert_eq!(latch, ProcessRefreshLatch::default());
    }
}

#[cfg(all(test, not(target_os = "macos")))]
mod process_system_tests {
    use super::*;

    #[test]
    fn process_system_starts_empty_and_refreshes_fallback_pid_on_demand() {
        let pid = Pid::from_u32(std::process::id());
        let info = PtyProcessInfo::new(ProcessIdGetter::new(-1, pid.as_u32()));

        assert!(info.system.read().processes().is_empty());

        let child_pid = info.get_child().map(|process| process.pid());

        assert_eq!(child_pid, Some(pid));
        assert_eq!(info.system.read().processes().len(), 1);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::{path::Path, process::Command, thread};

    fn procargs2_fixture(executable: &[u8], arguments: &[&[u8]]) -> Vec<u8> {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&(arguments.len() as libc::c_int).to_ne_bytes());
        buffer.extend_from_slice(executable);
        buffer.extend_from_slice(&[0, 0, 0]);
        for argument in arguments {
            buffer.extend_from_slice(argument);
            buffer.push(0);
        }
        buffer.extend_from_slice(b"KEY=value\0");
        buffer
    }

    fn process_info(pid: u32, executable: Option<&str>, cwd: Option<&str>) -> ProcessInfo {
        ProcessInfo {
            pid: Pid::from_u32(pid),
            name: executable
                .and_then(|path| Path::new(path).file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            executable: executable.map(PathBuf::from),
            cwd: cwd.map(PathBuf::from),
            argv: vec![
                executable.unwrap_or("unknown").to_owned(),
                "--flag".to_owned(),
            ],
            argv_valid: true,
        }
    }

    #[test]
    fn parses_procargs2_argv_without_environment() {
        let fixture = procargs2_fixture(
            b"/usr/bin/python3",
            &[b"python3", b"script.py", b"", b"--verbose"],
        );

        assert_eq!(
            macos::parse_procargs2(&fixture),
            Some(vec![
                "python3".to_owned(),
                "script.py".to_owned(),
                String::new(),
                "--verbose".to_owned(),
            ])
        );
    }

    #[test]
    fn rejects_truncated_or_invalid_procargs2_buffers() {
        assert_eq!(macos::parse_procargs2(&[]), None);

        let mut negative_argc = (-1_i32).to_ne_bytes().to_vec();
        negative_argc.extend_from_slice(b"/bin/test\0test\0");
        assert_eq!(macos::parse_procargs2(&negative_argc), None);

        let mut truncated = (2_i32).to_ne_bytes().to_vec();
        truncated.extend_from_slice(b"/bin/test\0\0test\0missing-terminator");
        assert_eq!(macos::parse_procargs2(&truncated), None);
    }

    #[test]
    fn argv_cache_refreshes_only_when_process_identity_changes() {
        let previous = process_info(41, Some("/bin/sleep"), Some("/tmp"));

        assert!(!should_refresh_argv(
            Some(&previous),
            Pid::from_u32(41),
            Some(Path::new("/bin/sleep")),
        ));
        assert!(!should_refresh_argv(
            Some(&previous),
            Pid::from_u32(41),
            None,
        ));
        assert!(should_refresh_argv(
            Some(&previous),
            Pid::from_u32(42),
            Some(Path::new("/bin/sleep")),
        ));
        assert!(should_refresh_argv(
            Some(&previous),
            Pid::from_u32(41),
            Some(Path::new("/bin/cat")),
        ));
    }

    #[test]
    fn argv_probe_failure_retries_until_a_legitimate_result_is_cached() {
        let mut failed = process_info(41, Some("/bin/sleep"), Some("/tmp"));
        failed.argv.clear();
        failed.argv_valid = false;

        assert!(should_refresh_argv(
            Some(&failed),
            failed.pid,
            failed.executable.as_deref(),
        ));

        let recovered = merge_macos_process_info(
            Some(&failed),
            failed.pid,
            failed.cwd.clone(),
            failed.executable.clone(),
            Some(Vec::new()),
        );
        assert!(recovered.argv_valid);
        assert!(recovered.argv.is_empty(), "an empty argv can be valid");
        assert!(!should_refresh_argv(
            Some(&recovered),
            recovered.pid,
            recovered.executable.as_deref(),
        ));
    }

    #[test]
    fn partial_probe_fields_are_retained_only_for_same_identity() {
        let previous = process_info(41, Some("/bin/sleep"), Some("/old-cwd"));
        let retained =
            merge_macos_process_info(Some(&previous), Pid::from_u32(41), None, None, None);
        assert_eq!(retained.executable, previous.executable);
        assert_eq!(retained.cwd, previous.cwd);
        assert_eq!(retained.argv, previous.argv);

        let replaced = merge_macos_process_info(
            Some(&previous),
            Pid::from_u32(42),
            None,
            Some(PathBuf::from("/bin/cat")),
            None,
        );
        assert_eq!(replaced.executable, Some(PathBuf::from("/bin/cat")));
        assert_eq!(replaced.cwd, None);
        assert!(replaced.argv.is_empty());
    }

    #[test]
    fn foreground_pid_race_rejects_mixed_snapshots() {
        let initial = Pid::from_u32(41);
        assert!(foreground_pid_unchanged(initial, Some(initial)));
        assert!(!foreground_pid_unchanged(initial, Some(Pid::from_u32(42))));
        assert!(!foreground_pid_unchanged(initial, None));
    }

    #[test]
    fn child_signal_pid_rejects_zero_and_out_of_range_values() {
        assert_eq!(child_signal_pid(Pid::from_u32(0)), None);
        assert_eq!(child_signal_pid(Pid::from_u32(1)), Some(1));
        assert_eq!(
            child_signal_pid(Pid::from_u32(i32::MAX as u32)),
            Some(i32::MAX)
        );
        assert_eq!(child_signal_pid(Pid::from_u32(i32::MAX as u32 + 1)), None);
    }

    #[test]
    fn zero_fallback_pid_never_targets_the_calling_process_group() {
        let info = PtyProcessInfo::new(ProcessIdGetter::new(-1, 0));
        assert!(!info.kill_child_process());
        assert!(!info.terminate_child_process());
    }

    #[test]
    fn bounded_process_paths_do_not_require_a_nul_terminator() {
        assert_eq!(
            macos::path_from_bounded_buffer(b"/tmp/process\0ignored"),
            Some(PathBuf::from("/tmp/process"))
        );
        assert_eq!(
            macos::path_from_bounded_buffer(b"/tmp/full-buffer"),
            Some(PathBuf::from("/tmp/full-buffer"))
        );
        assert_eq!(macos::path_from_bounded_buffer(b"\0ignored"), None);
    }

    #[test]
    fn targeted_macos_probe_reads_live_sleep_process() {
        let mut child = Command::new("sleep")
            .arg("2")
            .spawn()
            .expect("spawn sleep process");
        let pid = Pid::from_u32(child.id());
        let mut snapshot = None;
        for _ in 0..50 {
            if let (Some(executable), Some(cwd), Some(argv)) = (
                macos::process_executable(pid),
                macos::process_cwd(pid),
                macos::process_argv(pid),
            ) {
                snapshot = Some((executable, cwd, argv));
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();

        let (executable, cwd, argv) = snapshot.expect("read targeted sleep process fields");
        assert_eq!(executable.file_name(), Some(std::ffi::OsStr::new("sleep")));
        assert_eq!(cwd, std::env::current_dir().unwrap());
        assert!(
            argv.first()
                .is_some_and(|argument| argument.ends_with("sleep"))
        );
        assert_eq!(argv.get(1).map(String::as_str), Some("2"));
    }
}
