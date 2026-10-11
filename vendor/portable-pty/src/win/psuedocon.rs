use super::WinChild;
use crate::cmdbuilder::CommandBuilder;
use crate::win::procthreadattr::ProcThreadAttributeList;
use anyhow::{bail, ensure, Error};
use filedescriptor::{FileDescriptor, OwnedHandle};
use shared_library::shared_library;
use std::ffi::OsString;
use std::io::Error as IoError;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::{mem, ptr};
use winapi::shared::minwindef::DWORD;
use winapi::shared::winerror::{HRESULT, S_OK};
use winapi::um::handleapi::*;
use winapi::um::processthreadsapi::*;
use winapi::um::winbase::{
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};
use winapi::um::wincon::COORD;
use winapi::um::winnt::HANDLE;

pub type HPCON = HANDLE;

pub const PSUEDOCONSOLE_INHERIT_CURSOR: DWORD = 0x1;
pub const PSEUDOCONSOLE_RESIZE_QUIRK: DWORD = 0x2;
pub const PSEUDOCONSOLE_WIN32_INPUT_MODE: DWORD = 0x4;
#[allow(dead_code)]
pub const PSEUDOCONSOLE_PASSTHROUGH_MODE: DWORD = 0x8;

shared_library!(ConPtyFuncs,
    pub fn CreatePseudoConsole(
        size: COORD,
        hInput: HANDLE,
        hOutput: HANDLE,
        flags: DWORD,
        hpc: *mut HPCON
    ) -> HRESULT,
    pub fn ResizePseudoConsole(hpc: HPCON, size: COORD) -> HRESULT,
    pub fn ClosePseudoConsole(hpc: HPCON),
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConptyBackend {
    Bundled,
    System,
}

#[derive(Clone, Debug)]
pub struct ConptyStatus {
    pub active_bundled: usize,
    pub active_system: usize,
    pub last_used: Option<ConptyBackend>,
    pub fallback_reason: Option<String>,
}

struct ConptyState {
    system: ConPtyFuncs,
    bundled: Option<ConPtyFuncs>,
    bundled_disabled: AtomicBool,
    active_bundled: AtomicUsize,
    active_system: AtomicUsize,
    last_used: AtomicU8,
    fallback_reason: Mutex<Option<String>>,
}

static BUNDLED_PATH: OnceLock<PathBuf> = OnceLock::new();
static CONPTY: OnceLock<ConptyState> = OnceLock::new();

pub fn configure_bundled_conpty(path: PathBuf) -> Result<(), &'static str> {
    if !path.is_absolute() {
        return Err("bundled ConPTY path must be absolute");
    }
    if CONPTY.get().is_some() {
        return Err("ConPTY was already initialized");
    }
    BUNDLED_PATH
        .set(path)
        .map_err(|_| "bundled ConPTY path was already configured")
}

pub fn conpty_status() -> ConptyStatus {
    let Some(state) = CONPTY.get() else {
        return ConptyStatus {
            active_bundled: 0,
            active_system: 0,
            last_used: None,
            fallback_reason: None,
        };
    };
    let last_used = match state.last_used.load(Ordering::Acquire) {
        1 => Some(ConptyBackend::Bundled),
        2 => Some(ConptyBackend::System),
        _ => None,
    };
    ConptyStatus {
        active_bundled: state.active_bundled.load(Ordering::Acquire),
        active_system: state.active_system.load(Ordering::Acquire),
        last_used,
        fallback_reason: state.fallback_reason.lock().unwrap().clone(),
    }
}

fn load_conpty() -> ConptyState {
    let system = ConPtyFuncs::open(Path::new("kernel32.dll")).expect(
        "this system does not support conpty.  Windows 10 October 2018 or newer is required",
    );
    let (bundled, fallback_reason) = match BUNDLED_PATH.get() {
        Some(path) => {
            let host_arches: &[&str] = match std::env::consts::ARCH {
                "x86_64" => &["x64", "arm64"],
                "aarch64" => &["arm64"],
                "x86" => &["x86", "x64", "arm64"],
                _ => &[],
            };
            let missing_host = host_arches.iter().find_map(|arch| {
                let host = path.parent()?.join(arch).join("OpenConsole.exe");
                (!host.is_file()).then_some(host)
            });
            if let Some(host) = missing_host {
                (
                    None,
                    Some(format!(
                        "Bundled ConPTY host is missing: {}",
                        host.display()
                    )),
                )
            } else {
                match ConPtyFuncs::open(path) {
                    Ok(funcs) => (Some(funcs), None),
                    Err(error) => (None, Some(format!("Bundled ConPTY load failed: {error:?}"))),
                }
            }
        }
        None => (None, None),
    };
    ConptyState {
        system,
        bundled,
        bundled_disabled: AtomicBool::new(false),
        active_bundled: AtomicUsize::new(0),
        active_system: AtomicUsize::new(0),
        last_used: AtomicU8::new(0),
        fallback_reason: Mutex::new(fallback_reason),
    }
}

pub struct PsuedoCon {
    con: HPCON,
    backend: ConptyBackend,
}

unsafe impl Send for PsuedoCon {}
unsafe impl Sync for PsuedoCon {}

impl Drop for PsuedoCon {
    fn drop(&mut self) {
        let state = CONPTY
            .get()
            .expect("ConPTY initialized before PTY creation");
        let funcs = state.funcs(self.backend);
        unsafe { (funcs.ClosePseudoConsole)(self.con) };
        state.active(self.backend).fetch_sub(1, Ordering::AcqRel);
    }
}

impl ConptyState {
    fn funcs(&self, backend: ConptyBackend) -> &ConPtyFuncs {
        match backend {
            ConptyBackend::Bundled => self.bundled.as_ref().expect("bundled ConPTY is loaded"),
            ConptyBackend::System => &self.system,
        }
    }

    fn active(&self, backend: ConptyBackend) -> &AtomicUsize {
        match backend {
            ConptyBackend::Bundled => &self.active_bundled,
            ConptyBackend::System => &self.active_system,
        }
    }
}

impl PsuedoCon {
    pub fn new(size: COORD, input: FileDescriptor, output: FileDescriptor) -> Result<Self, Error> {
        let state = CONPTY.get_or_init(load_conpty);
        if state.bundled.is_some() && !state.bundled_disabled.load(Ordering::Acquire) {
            match Self::create(state.funcs(ConptyBackend::Bundled), size, &input, &output) {
                Ok(con) => return Ok(Self::record(con, ConptyBackend::Bundled, state)),
                Err(error) => {
                    state.bundled_disabled.store(true, Ordering::Release);
                    *state.fallback_reason.lock().unwrap() =
                        Some(format!("Bundled ConPTY creation failed: {error:#}"));
                }
            }
        }
        let con = Self::create(&state.system, size, &input, &output)?;
        Ok(Self::record(con, ConptyBackend::System, state))
    }

    fn record(con: HPCON, backend: ConptyBackend, state: &ConptyState) -> Self {
        state.active(backend).fetch_add(1, Ordering::AcqRel);
        state.last_used.store(
            match backend {
                ConptyBackend::Bundled => 1,
                ConptyBackend::System => 2,
            },
            Ordering::Release,
        );
        Self { con, backend }
    }

    fn create(
        funcs: &ConPtyFuncs,
        size: COORD,
        input: &FileDescriptor,
        output: &FileDescriptor,
    ) -> Result<HPCON, Error> {
        let mut con: HPCON = INVALID_HANDLE_VALUE;
        let result = unsafe {
            (funcs.CreatePseudoConsole)(
                size,
                input.as_raw_handle() as _,
                output.as_raw_handle() as _,
                PSUEDOCONSOLE_INHERIT_CURSOR
                    | PSEUDOCONSOLE_RESIZE_QUIRK
                    | PSEUDOCONSOLE_WIN32_INPUT_MODE,
                &mut con,
            )
        };
        ensure!(
            result == S_OK,
            "failed to create psuedo console: HRESULT {}",
            result
        );
        Ok(con)
    }

    pub fn resize(&self, size: COORD) -> Result<(), Error> {
        let state = CONPTY
            .get()
            .expect("ConPTY initialized before PTY creation");
        let result = unsafe { (state.funcs(self.backend).ResizePseudoConsole)(self.con, size) };
        ensure!(
            result == S_OK,
            "failed to resize console to {}x{}: HRESULT: {}",
            size.X,
            size.Y,
            result
        );
        Ok(())
    }

    pub fn spawn_command(&self, cmd: CommandBuilder) -> anyhow::Result<WinChild> {
        let mut si: STARTUPINFOEXW = unsafe { mem::zeroed() };
        si.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
        // Explicitly set the stdio handles as invalid handles otherwise
        // we can end up with a weird state where the spawned process can
        // inherit the explicitly redirected output handles from its parent.
        // For example, when daemonizing wezterm-mux-server, the stdio handles
        // are redirected to a log file and the spawned process would end up
        // writing its output there instead of to the pty we just created.
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;

        let mut attrs = ProcThreadAttributeList::with_capacity(1)?;
        attrs.set_pty(self.con)?;
        si.lpAttributeList = attrs.as_mut_ptr();

        let mut pi: PROCESS_INFORMATION = unsafe { mem::zeroed() };

        let (mut exe, mut cmdline) = cmd.cmdline()?;
        let cmd_os = OsString::from_wide(&cmdline);

        let cwd = cmd.current_directory();

        let res = unsafe {
            CreateProcessW(
                exe.as_mut_slice().as_mut_ptr(),
                cmdline.as_mut_slice().as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                cmd.environment_block().as_mut_slice().as_mut_ptr() as *mut _,
                cwd.as_ref()
                    .map(|c| c.as_slice().as_ptr())
                    .unwrap_or(ptr::null()),
                &mut si.StartupInfo,
                &mut pi,
            )
        };
        if res == 0 {
            let err = IoError::last_os_error();
            let msg = format!(
                "CreateProcessW `{:?}` in cwd `{:?}` failed: {}",
                cmd_os,
                cwd.as_ref().map(|c| OsString::from_wide(c)),
                err
            );
            log::error!("{}", msg);
            bail!("{}", msg);
        }

        // Make sure we close out the thread handle so we don't leak it;
        // we do this simply by making it owned
        let _main_thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread as _) };
        let proc = unsafe { OwnedHandle::from_raw_handle(pi.hProcess as _) };

        Ok(WinChild {
            proc: Mutex::new(proc),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filedescriptor::Pipe;

    unsafe extern "C" fn fail_create(
        _size: COORD,
        _input: HANDLE,
        _output: HANDLE,
        _flags: DWORD,
        _hpc: *mut HPCON,
    ) -> HRESULT {
        0x8000_4005_u32 as HRESULT
    }

    #[test]
    fn bundled_creation_failure_uses_system_functions() {
        let system = ConPtyFuncs::open(Path::new("kernel32.dll")).unwrap();
        let mut bundled = ConPtyFuncs::open(Path::new("kernel32.dll")).unwrap();
        bundled.CreatePseudoConsole = fail_create;
        assert!(CONPTY
            .set(ConptyState {
                system,
                bundled: Some(bundled),
                bundled_disabled: AtomicBool::new(false),
                active_bundled: AtomicUsize::new(0),
                active_system: AtomicUsize::new(0),
                last_used: AtomicU8::new(0),
                fallback_reason: Mutex::new(None),
            })
            .is_ok());

        let input = Pipe::new().unwrap();
        let output = Pipe::new().unwrap();
        let pty = PsuedoCon::new(COORD { X: 80, Y: 24 }, input.read, output.write).unwrap();
        let status = conpty_status();
        assert_eq!(status.last_used, Some(ConptyBackend::System));
        assert_eq!(status.active_system, 1);
        assert!(status.fallback_reason.unwrap().contains("creation failed"));
        drop(pty);
        assert_eq!(conpty_status().active_system, 0);
    }
}
