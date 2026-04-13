use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Clone, Debug, Default)]
pub struct ProcessInfo {
    pub pid: i32,
    pub name: String,
    pub exe_path: Option<String>,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub start_time: Option<SystemTime>,
}

impl ProcessInfo {
    /// Is this process a Claude Code CLI invocation? Needed because the CLI
    /// is a Node.js script, so the kernel-reported process name is "node"
    /// even though the user ran `claude`.
    pub fn is_claude_code(&self) -> bool {
        if self.name == "claude" {
            return true;
        }
        if let Some(ref p) = self.exe_path {
            if p.ends_with("/claude") {
                return true;
            }
        }
        self.argv.iter().any(|a| {
            a.ends_with("/claude")
                || a == "claude"
                || a.contains("claude-code")
                || a.contains(".claude/local")
        })
    }

    pub fn is_ssh(&self) -> bool {
        if self.name == "ssh" {
            return true;
        }
        if let Some(ref p) = self.exe_path {
            if p.ends_with("/ssh") {
                return true;
            }
        }
        false
    }

    /// User-facing command name; collapses node-wrapped claude invocations.
    pub fn display_name(&self) -> &str {
        if self.is_claude_code() {
            "claude"
        } else {
            &self.name
        }
    }
}

#[cfg(target_os = "macos")]
pub fn inspect(pid: i32) -> Option<ProcessInfo> {
    let name = libproc::proc_pid::name(pid).ok()?;
    let exe_path = libproc::proc_pid::pidpath(pid).ok();
    let argv = macos::argv_of(pid).unwrap_or_default();
    let cwd = macos::cwd_of(pid);
    let start_time = libproc::proc_pid::pidinfo::<libproc::bsd_info::BSDInfo>(pid, 0)
        .ok()
        .map(|info| {
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(info.pbi_start_tvsec)
        });
    Some(ProcessInfo {
        pid,
        name,
        exe_path,
        argv,
        cwd,
        start_time,
    })
}

#[cfg(not(target_os = "macos"))]
pub fn inspect(_pid: i32) -> Option<ProcessInfo> {
    None
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CStr;
    use std::path::PathBuf;

    // PROC_PIDVNODEPATHINFO = 9 (bsd/sys/proc_info.h)
    const PROC_PIDVNODEPATHINFO: libc::c_int = 9;
    const MAXPATHLEN: usize = 1024;

    // Layout mirrors XNU's bsd/sys/proc_info.h. Stable for many macOS versions.
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct VinfoStat {
        vst_dev: u32,
        vst_mode: u16,
        vst_nlink: u16,
        vst_ino: u64,
        vst_uid: u32,
        vst_gid: u32,
        vst_atime: i64,
        vst_atimensec: i64,
        vst_mtime: i64,
        vst_mtimensec: i64,
        vst_ctime: i64,
        vst_ctimensec: i64,
        vst_birthtime: i64,
        vst_birthtimensec: i64,
        vst_size: i64,
        vst_blocks: i64,
        vst_blksize: i32,
        vst_flags: u32,
        vst_gen: u32,
        vst_rdev: u32,
        vst_qspare: [i64; 2],
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct Fsid {
        val: [i32; 2],
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct VnodeInfo {
        vi_stat: VinfoStat,
        vi_type: i32,
        vi_pad: i32,
        vi_fsid: Fsid,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct VnodeInfoPath {
        vip_vi: VnodeInfo,
        vip_path: [libc::c_char; MAXPATHLEN],
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct ProcVnodePathInfo {
        pvi_cdir: VnodeInfoPath,
        pvi_rdir: VnodeInfoPath,
    }

    unsafe extern "C" {
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    /// Fetch a process's argv via `sysctl(KERN_PROCARGS2)`.
    ///
    /// Layout of the returned buffer is documented in Darwin's ps(1) source:
    ///   [i32 argc][exec_path NUL][NUL padding][argv[0] NUL][argv[1] NUL]...[env...]
    pub fn argv_of(pid: i32) -> Option<Vec<String>> {
        const CTL_KERN: libc::c_int = 1;
        const KERN_PROCARGS2: libc::c_int = 49;

        let mut mib: [libc::c_int; 3] = [CTL_KERN, KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 16 * 1024;
        let mut buf: Vec<u8> = vec![0u8; size];
        let ret = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if ret != 0 {
            return None;
        }
        buf.truncate(size);
        if buf.len() < 4 {
            return None;
        }

        let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]).max(0) as usize;
        let mut pos = 4;

        // Skip exec_path (null-terminated) then any padding nulls.
        while pos < buf.len() && buf[pos] != 0 {
            pos += 1;
        }
        while pos < buf.len() && buf[pos] == 0 {
            pos += 1;
        }

        let mut args = Vec::with_capacity(argc);
        for _ in 0..argc {
            if pos >= buf.len() {
                break;
            }
            let start = pos;
            while pos < buf.len() && buf[pos] != 0 {
                pos += 1;
            }
            if let Ok(s) = std::str::from_utf8(&buf[start..pos]) {
                args.push(s.to_string());
            }
            pos += 1; // step past terminator
        }
        Some(args)
    }

    pub fn cwd_of(pid: i32) -> Option<PathBuf> {
        let mut info: ProcVnodePathInfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<ProcVnodePathInfo>() as libc::c_int;
        let ret = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDVNODEPATHINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if ret <= 0 {
            return None;
        }
        let cstr = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr()) };
        let s = cstr.to_str().ok()?;
        if s.is_empty() {
            None
        } else {
            Some(PathBuf::from(s))
        }
    }
}
