//! Windows single-instance plugin with hang-safe secondary launch.
//!
//! Derived from `tauri-plugin-single-instance` (Apache-2.0 / MIT), replacing
//! blocking `SendMessageW` with `SendMessageTimeoutW(SMTO_ABORTIFHUNG)` and
//! taking over when the primary is hung so relaunch does not leave zombie processes.
//!
//! ## Zombie primary (WndProc alive, main loop dead)
//!
//! A stuck primary can still answer `SendMessage` on the SI window while its
//! Tauri main thread no longer runs — global shortcuts and tray clicks die, but
//! secondary launches used to exit after a successful notify. We therefore also
//! require a **main-thread ACK** on a named event before treating the primary as
//! healthy. ACK is signaled only from `run_on_main_thread`; if it never arrives,
//! the secondary takes over.

#![cfg(target_os = "windows")]

use std::ffi::CStr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{
    plugin::{self, TauriPlugin},
    AppHandle, Manager, RunEvent, Runtime,
};
use tracing::{info, warn};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GetLastError, BOOL, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, LRESULT,
        WAIT_ABANDONED, WAIT_OBJECT_0, WPARAM,
    },
    System::{
        DataExchange::COPYDATASTRUCT,
        LibraryLoader::GetModuleHandleW,
        Threading::{
            CreateEventW, CreateMutexW, OpenEventW, OpenProcess, QueryFullProcessImageNameW,
            ReleaseMutex, ResetEvent, SetEvent, TerminateProcess, WaitForSingleObject,
            EVENT_MODIFY_STATE, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            PROCESS_TERMINATE, SYNCHRONIZATION_SYNCHRONIZE,
        },
    },
    UI::WindowsAndMessaging::{
        self as w32wm, CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowW,
        GetWindowThreadProcessId, RegisterClassExW, SendMessageTimeoutW, CREATESTRUCTW,
        GWLP_USERDATA, GWL_STYLE, SMTO_ABORTIFHUNG, WINDOW_LONG_PTR_INDEX, WM_COPYDATA, WM_CREATE,
        WM_DESTROY, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TRANSPARENT, WS_OVERLAPPED, WS_POPUP, WS_VISIBLE,
    },
};

const WMCOPYDATA_SINGLE_INSTANCE_DATA: usize = 1542;
/// Max wait for a healthy primary's SI WndProc to accept the notify.
const NOTIFY_TIMEOUT_MS: u32 = 5000;
/// Max wait for the primary **main thread** to ACK after WndProc queued work.
/// ACK is signaled before `on_second_instance` work; this covers main-queue delay only.
const MAIN_ACK_TIMEOUT_MS: u32 = 5000;
/// Max wait for the secondary-notify gate (serializes ResetEvent + Wait ACK).
const NOTIFY_GATE_TIMEOUT_MS: u32 = 8000;
/// Max wait to own the mutex after terminating a hung primary.
const MUTEX_ACQUIRE_TIMEOUT_MS: u32 = 8000;

type SingleInstanceCallback<R> = dyn FnMut(&AppHandle<R>, Vec<String>, String) + Send + 'static;

struct MutexHandle(isize);
struct TargetWindowHandle(isize);
struct AckEventHandle(isize);

struct UserData<R: Runtime> {
    app: AppHandle<R>,
    callback: Arc<Mutex<Box<SingleInstanceCallback<R>>>>,
    ack_event: isize,
}

impl<R: Runtime> UserData<R> {
    unsafe fn from_hwnd_raw(hwnd: HWND) -> *mut Self {
        GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Self
    }

    unsafe fn from_hwnd<'a>(hwnd: HWND) -> &'a mut Self {
        &mut *Self::from_hwnd_raw(hwnd)
    }

    /// Queue the callback off the SI window procedure so `SendMessageTimeout` can
    /// return and Tauri window ops are not nested inside the WndProc (avoids
    /// reentrancy deadlocks when the primary is healthy).
    ///
    /// Signals the ACK event from the main thread **before** user work so a busy
    /// `on_second_instance` does not cause false takeover.
    fn run_callback_deferred(&self, args: Vec<String>, cwd: String) {
        let app = self.app.clone();
        let callback = Arc::clone(&self.callback);
        // isize is Send; raw HANDLE (*mut c_void) is not.
        let ack = self.ack_event;
        std::thread::spawn(move || {
            let app_for_main = app.clone();
            let queued = app.run_on_main_thread(move || {
                if ack != 0 {
                    unsafe {
                        SetEvent(ack as HANDLE);
                    }
                }
                if let Ok(mut guard) = callback.lock() {
                    (guard)(&app_for_main, args, cwd);
                }
            });
            if queued.is_err() {
                warn!("single-instance: run_on_main_thread failed; ACK will not be signaled");
            }
        });
    }
}

/// Whether the secondary should exit because the primary is considered alive.
///
/// - `wndproc_ok`: SI window accepted `SendMessageTimeout`.
/// - `main_ack`: `Some(true/false)` when an ACK event exists; `None` means the
///   event is missing (treat as unhealthy — cannot verify main-thread liveness).
fn secondary_should_exit(wndproc_ok: bool, main_ack: Option<bool>) -> bool {
    if !wndproc_ok {
        return false;
    }
    main_ack.unwrap_or(false)
}

/// Install MarkerOn's Windows single-instance behaviour.
pub fn init<R: Runtime, F>(callback: F) -> TauriPlugin<R>
where
    F: FnMut(&AppHandle<R>, Vec<String>, String) + Send + 'static,
{
    let callback: Arc<Mutex<Box<SingleInstanceCallback<R>>>> =
        Arc::new(Mutex::new(Box::new(callback)));

    plugin::Builder::new("single-instance")
        .setup(move |app, _api| {
            let id = app.config().identifier.clone();
            let class_name = encode_wide(format!("{id}-sic"));
            let window_name = encode_wide(format!("{id}-siw"));
            let mutex_name = encode_wide(format!("{id}-sim"));
            let ack_name = encode_wide(format!("{id}-sia"));
            // Serializes secondary notify so concurrent ResetEvent cannot clear another
            // secondary's ACK (false takeover of a healthy primary).
            let gate_name = encode_wide(format!("{id}-sin"));

            let hmutex =
                unsafe { CreateMutexW(std::ptr::null(), 1i32, mutex_name.as_ptr()) };

            if hmutex.is_null() {
                warn!("single-instance: CreateMutexW failed ({})", unsafe {
                    GetLastError()
                });
                return Ok(());
            }

            let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;

            if already_exists {
                let hwnd = unsafe { FindWindowW(class_name.as_ptr(), window_name.as_ptr()) };

                if !hwnd.is_null() {
                    if notify_existing_instance(hwnd, &ack_name, &gate_name) {
                        unsafe {
                            CloseHandle(hmutex);
                        }
                        app.cleanup_before_exit();
                        std::process::exit(0);
                    }

                    info!(
                        "single-instance: primary unresponsive (WndProc and/or main ACK); taking over"
                    );
                    terminate_si_owner_if_ours(hwnd);
                } else {
                    warn!(
                        "single-instance: mutex held but SI window missing; attempting takeover"
                    );
                }

                let wait = unsafe { WaitForSingleObject(hmutex, MUTEX_ACQUIRE_TIMEOUT_MS) };
                if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
                    warn!(
                        "single-instance: could not acquire mutex after takeover attempt ({wait}); exiting"
                    );
                    unsafe {
                        CloseHandle(hmutex);
                    }
                    app.cleanup_before_exit();
                    std::process::exit(0);
                }
            }

            // Manual-reset ACK event: secondary ResetEvent → notify → Wait;
            // primary SetEvent from main thread. Create before the SI window so a
            // visible SI window always has a matching ACK channel on this build.
            let hack = unsafe {
                CreateEventW(std::ptr::null(), 1i32, 0i32, ack_name.as_ptr())
            };
            if hack.is_null() {
                warn!("single-instance: CreateEventW(ACK) failed ({})", unsafe {
                    GetLastError()
                });
            }

            app.manage(MutexHandle(hmutex as isize));
            app.manage(AckEventHandle(hack as isize));

            let userdata = UserData {
                app: app.clone(),
                callback: Arc::clone(&callback),
                ack_event: hack as isize,
            };
            let userdata = Box::into_raw(Box::new(userdata));
            let hwnd = create_event_target_window::<R>(&class_name, &window_name, userdata);
            app.manage(TargetWindowHandle(hwnd as isize));

            Ok(())
        })
        .on_event(|app, event| {
            if let RunEvent::Exit = event {
                destroy(app);
            }
        })
        .build()
}

fn destroy<R: Runtime, M: Manager<R>>(manager: &M) {
    if let Some(hmutex) = manager.try_state::<MutexHandle>() {
        unsafe {
            ReleaseMutex(hmutex.0 as HANDLE);
            CloseHandle(hmutex.0 as HANDLE);
        }
    }
    if let Some(hwnd) = manager.try_state::<TargetWindowHandle>() {
        unsafe {
            DestroyWindow(hwnd.0 as HWND);
        }
    }
    if let Some(ack) = manager.try_state::<AckEventHandle>() {
        if ack.0 != 0 {
            unsafe {
                CloseHandle(ack.0 as HANDLE);
            }
        }
    }
}

/// Returns true when the primary is alive (WndProc + main-thread ACK).
fn notify_existing_instance(hwnd: HWND, ack_name: &[u16], gate_name: &[u16]) -> bool {
    // Gate must wrap Reset→Send→Wait so two secondaries cannot interleave ResetEvent
    // and clear each other's ACK.
    let gate = unsafe { CreateMutexW(std::ptr::null(), 0i32, gate_name.as_ptr()) };
    if gate.is_null() {
        warn!(
            "single-instance: CreateMutexW(notify gate) failed ({})",
            unsafe { GetLastError() }
        );
        return false;
    }
    let gate_wait = unsafe { WaitForSingleObject(gate, NOTIFY_GATE_TIMEOUT_MS) };
    if gate_wait != WAIT_OBJECT_0 && gate_wait != WAIT_ABANDONED {
        warn!("single-instance: notify gate timed out ({gate_wait})");
        unsafe {
            CloseHandle(gate);
        }
        return false;
    }

    let alive = notify_existing_instance_locked(hwnd, ack_name);

    unsafe {
        ReleaseMutex(gate);
        CloseHandle(gate);
    }
    alive
}

fn notify_existing_instance_locked(hwnd: HWND, ack_name: &[u16]) -> bool {
    let access = EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE;
    let hack = unsafe { OpenEventW(access, 0, ack_name.as_ptr()) };
    if hack.is_null() {
        // Cannot verify main-thread liveness (old build or broken primary).
        // WndProc-only success is not enough — that is exactly the zombie case.
        warn!(
            "single-instance: ACK event missing (err {}); treating primary as unhealthy",
            unsafe { GetLastError() }
        );
        return secondary_should_exit(true, None);
    }

    unsafe {
        ResetEvent(hack);
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let cwd = cwd.to_str().unwrap_or_default();
    let args = std::env::args().collect::<Vec<String>>().join("|");
    let data = format!("{cwd}|{args}\0");
    let bytes = data.as_bytes();
    let cds = COPYDATASTRUCT {
        dwData: WMCOPYDATA_SINGLE_INSTANCE_DATA,
        cbData: bytes.len() as u32,
        lpData: bytes.as_ptr() as *mut _,
    };

    let mut result: usize = 0;
    let wndproc_ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_COPYDATA,
            0,
            &cds as *const COPYDATASTRUCT as LPARAM,
            SMTO_ABORTIFHUNG,
            NOTIFY_TIMEOUT_MS,
            &mut result,
        )
    } != 0;

    if !wndproc_ok {
        unsafe {
            CloseHandle(hack);
        }
        return secondary_should_exit(false, Some(false));
    }

    let wait = unsafe { WaitForSingleObject(hack, MAIN_ACK_TIMEOUT_MS) };
    unsafe {
        CloseHandle(hack);
    }

    let acked = wait == WAIT_OBJECT_0;
    if !acked {
        warn!(
            "single-instance: main-thread ACK timed out after {}ms (zombie primary?)",
            MAIN_ACK_TIMEOUT_MS
        );
    }
    secondary_should_exit(true, Some(acked))
}

fn terminate_si_owner_if_ours(hwnd: HWND) {
    let mut pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    if pid == 0 {
        return;
    }
    let self_pid = std::process::id();
    if pid == self_pid {
        return;
    }

    let Some(self_name) = current_exe_file_name() else {
        return;
    };
    let Some(other_path) = process_image_path(pid) else {
        warn!("single-instance: could not resolve image for pid {pid}");
        return;
    };
    let other_name = Path::new(&other_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !other_name.eq_ignore_ascii_case(&self_name) {
        warn!("single-instance: refusing to terminate pid {pid} ({other_name} != {self_name})");
        return;
    }

    let access = PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        warn!("single-instance: OpenProcess({pid}) failed ({})", unsafe {
            GetLastError()
        });
        return;
    }

    info!("single-instance: terminating hung/zombie primary pid {pid}");
    let terminated = unsafe { TerminateProcess(handle, 1) };
    if terminated == 0 {
        warn!(
            "single-instance: TerminateProcess({pid}) failed ({})",
            unsafe { GetLastError() }
        );
        unsafe {
            CloseHandle(handle);
        }
        return;
    }

    let _ = unsafe { WaitForSingleObject(handle, MUTEX_ACQUIRE_TIMEOUT_MS) };
    unsafe {
        CloseHandle(handle);
    }
    // Brief pause so the SI HWND / mutex ownership can settle.
    std::thread::sleep(Duration::from_millis(50));
}

fn current_exe_file_name() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    exe.file_name()?.to_str().map(|s| s.to_string())
}

fn process_image_path(pid: u32) -> Option<String> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }

    let mut buf = [0u16; 1024];
    let mut size = buf.len() as u32;
    let ok: BOOL = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size) };
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 || size == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..size as usize]))
}

unsafe extern "system" fn single_instance_window_proc<R: Runtime>(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let create_struct = &*(lparam as *const CREATESTRUCTW);
            let userdata = create_struct.lpCreateParams as *const UserData<R>;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, userdata as isize);
            0
        }

        WM_COPYDATA => {
            let cds_ptr = lparam as *const COPYDATASTRUCT;
            if (*cds_ptr).dwData == WMCOPYDATA_SINGLE_INSTANCE_DATA {
                let userdata = UserData::<R>::from_hwnd(hwnd);
                let data = CStr::from_ptr((*cds_ptr).lpData as *const i8).to_string_lossy();
                let mut s = data.split('|');
                let cwd = s.next().unwrap_or_default();
                let args = s.map(|s| s.to_string()).collect();
                userdata.run_callback_deferred(args, cwd.to_string());
            }
            1
        }

        WM_DESTROY => {
            let userdata = UserData::<R>::from_hwnd_raw(hwnd);
            if !userdata.is_null() {
                drop(Box::from_raw(userdata));
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn create_event_target_window<R: Runtime>(
    class_name: &[u16],
    window_name: &[u16],
    userdata: *const UserData<R>,
) -> HWND {
    unsafe {
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(single_instance_window_proc::<R>),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: GetModuleHandleW(std::ptr::null()),
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };

        RegisterClassExW(&class);

        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED | WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            window_name.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            userdata as *const _,
        );
        SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_VISIBLE | WS_POPUP) as isize);
        hwnd
    }
}

fn encode_wide(string: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    std::os::windows::prelude::OsStrExt::encode_wide(string.as_ref())
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(target_pointer_width = "32")]
#[allow(non_snake_case)]
unsafe fn SetWindowLongPtrW(hwnd: HWND, index: WINDOW_LONG_PTR_INDEX, value: isize) -> isize {
    w32wm::SetWindowLongW(hwnd, index, value as i32) as isize
}

#[cfg(target_pointer_width = "64")]
#[allow(non_snake_case)]
unsafe fn SetWindowLongPtrW(hwnd: HWND, index: WINDOW_LONG_PTR_INDEX, value: isize) -> isize {
    w32wm::SetWindowLongPtrW(hwnd, index, value)
}

#[cfg(target_pointer_width = "32")]
#[allow(non_snake_case)]
unsafe fn GetWindowLongPtrW(hwnd: HWND, index: WINDOW_LONG_PTR_INDEX) -> isize {
    w32wm::GetWindowLongW(hwnd, index) as isize
}

#[cfg(target_pointer_width = "64")]
#[allow(non_snake_case)]
unsafe fn GetWindowLongPtrW(hwnd: HWND, index: WINDOW_LONG_PTR_INDEX) -> isize {
    w32wm::GetWindowLongPtrW(hwnd, index)
}

#[cfg(test)]
mod tests {
    use super::secondary_should_exit;

    #[test]
    fn exit_when_wndproc_and_main_ack() {
        assert!(secondary_should_exit(true, Some(true)));
    }

    #[test]
    fn takeover_when_wndproc_hung() {
        assert!(!secondary_should_exit(false, Some(true)));
        assert!(!secondary_should_exit(false, None));
    }

    #[test]
    fn takeover_when_main_ack_times_out() {
        // Zombie: SI WndProc answers SendMessage, main loop never ACKs.
        assert!(!secondary_should_exit(true, Some(false)));
    }

    #[test]
    fn takeover_when_ack_event_missing() {
        assert!(!secondary_should_exit(true, None));
    }
}
