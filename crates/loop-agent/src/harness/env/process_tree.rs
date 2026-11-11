//! Identify command descendants by an inherited Seatbelt policy fingerprint.
//!
//! Session/process-group IDs and environment variables can be changed by a
//! child. The two policy probes survive setsid, reparenting and exec with an
//! empty environment. Birth times prevent signalling a recycled PID.

use std::ffi::CString;
use std::path::Path;

#[link(name = "sandbox")]
unsafe extern "C" {
    fn sandbox_check(
        pid: libc::pid_t,
        operation: *const libc::c_char,
        kind: libc::c_int,
        ...
    ) -> libc::c_int;
    static SANDBOX_CHECK_NO_REPORT: libc::c_int;
}

pub(crate) struct Descendants {
    deny: CString,
    allow: CString,
}

#[derive(Clone, Copy)]
struct Process {
    pid: i32,
    birth: (u64, u64),
}

fn info(pid: i32) -> Option<libc::proc_bsdinfo> {
    // SAFETY: this POD buffer has the size expected by PROC_PIDTBSDINFO.
    unsafe {
        let mut info = std::mem::zeroed::<libc::proc_bsdinfo>();
        let size = std::mem::size_of_val(&info) as i32;
        (libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        ) == size)
            .then_some(info)
    }
}

impl Process {
    fn signal(self, signal: i32) {
        if info(self.pid).is_some_and(|i| (i.pbi_start_tvsec, i.pbi_start_tvusec) == self.birth) {
            // SAFETY: a positive PID, checked against its birth identity.
            unsafe {
                libc::kill(self.pid, signal);
            }
        }
    }
}

impl Descendants {
    pub(crate) fn new(root: &Path) -> std::io::Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        let cpath = |name| {
            CString::new(root.join(name).as_os_str().as_bytes()).map_err(std::io::Error::other)
        };
        let scope = Self {
            deny: cpath("deny")?,
            allow: cpath("allow")?,
        };
        // Fail preflight rather than silently fall back to process groups on
        // a system where these private Seatbelt queries are unavailable.
        // SAFETY: the fixed operation and path are NUL-terminated and the
        // variadic filter takes exactly this one path argument.
        unsafe {
            let kind = 1 | SANDBOX_CHECK_NO_REPORT;
            if sandbox_check(
                libc::getpid(),
                c"file-read-data".as_ptr(),
                kind,
                scope.allow.as_ptr(),
            ) != 0
            {
                return Err(std::io::Error::other(
                    "Seatbelt process identity queries are unavailable",
                ));
            }
        }
        Ok(scope)
    }

    fn marked(&self, pid: i32) -> bool {
        // SAFETY: sandbox_check is variadic; SANDBOX_FILTER_PATH (1) takes
        // one NUL-terminated path. NO_REPORT prevents logging probe denials.
        unsafe {
            let kind = 1 | SANDBOX_CHECK_NO_REPORT;
            sandbox_check(pid, c"file-read-data".as_ptr(), kind, self.deny.as_ptr()) == 1
                && sandbox_check(pid, c"file-read-data".as_ptr(), kind, self.allow.as_ptr()) == 0
        }
    }

    fn processes(&self) -> std::io::Result<Vec<Process>> {
        // SAFETY: proc_listpids with a null buffer returns the byte capacity;
        // the second call writes at most the provided buffer length.
        let pids = unsafe {
            let uid = libc::geteuid();
            let bytes = libc::proc_listpids(4, uid, std::ptr::null_mut(), 0);
            if bytes <= 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut pids = vec![0i32; bytes as usize / std::mem::size_of::<i32>() + 256];
            let used = libc::proc_listpids(
                4,
                uid,
                pids.as_mut_ptr().cast(),
                std::mem::size_of_val(pids.as_slice()) as i32,
            );
            if used < 0 {
                return Err(std::io::Error::last_os_error());
            }
            pids.truncate(used as usize / std::mem::size_of::<i32>());
            pids
        };
        Ok(pids
            .into_iter()
            .filter_map(|pid| {
                let i = info(pid)?;
                (pid > 1 && self.marked(pid)).then_some(Process {
                    pid,
                    birth: (i.pbi_start_tvsec, i.pbi_start_tvusec),
                })
            })
            .collect())
    }

    pub(crate) fn kill(&self) -> std::io::Result<()> {
        // Freeze to a fixed point before killing: a child may fork between
        // enumeration and SIGSTOP, but the following pass catches that child.
        let mut frozen = std::collections::HashMap::new();
        let result = (|| {
            for _ in 0..16 {
                let mut added = false;
                for process in self.processes()? {
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        frozen.entry((process.pid, process.birth))
                    {
                        process.signal(libc::SIGSTOP);
                        entry.insert(process);
                        added = true;
                    }
                }
                if !added {
                    return Ok(());
                }
            }
            Err(std::io::Error::other(
                "Seatbelt descendants did not settle during cleanup",
            ))
        })();
        // Also kill already frozen processes if a later enumeration failed.
        for process in frozen.values() {
            process.signal(libc::SIGKILL);
        }
        result
    }
}
