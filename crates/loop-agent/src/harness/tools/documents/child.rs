//! Document parsing in a child process.
//!
//! The parsers (pdf-extract, lopdf, calamine, the zip and XML readers) run
//! on bytes an attacker may control. A stack overflow or a failed
//! allocation aborts a process and cannot be caught, and the Runtime owns
//! every Session, so the Runtime never parses a document itself: it starts
//! `<program> <args..> <format>` (by default the `loop` binary with
//! [`CHILD_ARG`]), writes the file to its stdin and reads the text from its
//! stdout.
//!
//! The child gets an empty environment (no API keys), no files to write
//! (`RLIMIT_FSIZE` 0), no core dumps, a CPU limit, and on Linux an address
//! space limit (`RLIMIT_AS`). macOS does not enforce `RLIMIT_AS`, so there
//! the parent polls the child's physical footprint and kills it above the
//! same limit; an allocation burst faster than the poll interval can still
//! overshoot it briefly. The parent also stops it at a wall-clock timeout,
//! when its output passes a cap, and when the tool call is cancelled. The
//! child's stderr (panic messages included) is captured, never printed.

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

use super::{Format, MAX_INPUT_BYTES};
use crate::harness::utils::DEFAULT_MAX_BYTES;

/// The first argument that turns a host binary into the document reader.
pub const CHILD_ARG: &str = "__doc-to-text";

/// The first bytes of every child reply, so output from a wrong program is
/// never taken for a document's text.
const MAGIC: &[u8] = b"LOOPDOC1\n";

/// How documents are parsed.
#[derive(Debug, Clone)]
pub enum DocumentReader {
    /// Run `program args.. <format>` for each document: the bytes go to
    /// its stdin and the reply comes from its stdout. The program must
    /// call [`run_document_reader_child`] for that argument list.
    Child {
        /// The executable, normally the running binary.
        program: PathBuf,
        /// Arguments before the format name, normally `[CHILD_ARG]`.
        args: Vec<OsString>,
    },
    /// Parse in this process. For tests only: a hostile file can crash
    /// the process.
    InProcess,
}

static READER: OnceLock<DocumentReader> = OnceLock::new();

/// Set how this process parses documents. Only the first call counts;
/// returns whether it was that one.
pub fn set_document_reader(reader: DocumentReader) -> bool {
    READER.set(reader).is_ok()
}

/// The reader set for this process. Unit tests of this crate parse in
/// process; any other host that set none cannot read documents.
pub(super) fn current_reader() -> Option<DocumentReader> {
    if let Some(reader) = READER.get() {
        return Some(reader.clone());
    }
    if cfg!(test) {
        Some(DocumentReader::InProcess)
    } else {
        None
    }
}

/// Call first in a host binary's `main`. When this process was started as
/// the document reader (`<binary> __doc-to-text <format>`), convert stdin
/// and exit. Otherwise make this executable the document reader of this
/// process and return.
pub fn run_document_reader_child() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(std::ffi::OsStr::new(CHILD_ARG)) {
        let format = args.next().unwrap_or_default();
        std::process::exit(child_main(&format.to_string_lossy()));
    }
    if let Ok(program) = std::env::current_exe() {
        set_document_reader(DocumentReader::Child {
            program,
            args: vec![CHILD_ARG.into()],
        });
    }
}

/// What a PDF reader learned besides the text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PdfInfo {
    /// The text layer holds enough real text.
    pub readable: bool,
    /// The file is encrypted (even when it opened without a password).
    pub encrypted: bool,
}

/// A document as text.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Converted {
    pub text: String,
    /// More text than [`DEFAULT_MAX_BYTES`] was cut off.
    pub truncated: bool,
    pub pdf: Option<PdfInfo>,
}

/// The reader's answer: the text, or why the reader declined this file.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Reading {
    Done(Converted),
    Refused { error: String, pdf: Option<PdfInfo> },
}

#[derive(Serialize, Deserialize)]
struct Header {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default)]
    truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pdf: Option<PdfInfo>,
}

/// Limits on one child run.
#[derive(Debug, Clone, Copy)]
pub(super) struct Limits {
    pub timeout: Duration,
    /// Address space (Linux) or physical footprint (macOS), in bytes.
    pub memory: u64,
    /// The most stdout bytes accepted.
    pub output: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            memory: 2 * 1024 * 1024 * 1024,
            output: DEFAULT_MAX_BYTES + 64 * 1024,
        }
    }
}

/// The prefix of every error about a reader that did not finish.
pub(super) const UNSAFE: &str = "could not read this document safely";

/// Read `bytes` as `format` with `reader`. `Err` means the reader did not
/// finish (it crashed, ran out of time or memory, or was cancelled);
/// `Ok(Reading::Refused)` means it finished and declined the file.
pub(super) async fn convert(
    reader: &DocumentReader,
    limits: Limits,
    format: Format,
    bytes: Vec<u8>,
    cancel: Option<&CancellationToken>,
) -> Result<Reading, String> {
    match reader {
        DocumentReader::InProcess => {
            let work = tokio::task::spawn_blocking(move || reading(format, &bytes));
            tokio::select! {
                r = work => r.map_err(|e| if e.is_panic() {
                    format!("{UNSAFE}: the reader crashed")
                } else {
                    e.to_string()
                }),
                _ = cancelled(cancel) => Err(format!("{UNSAFE}: cancelled")),
            }
        }
        DocumentReader::Child { program, args } => {
            run_child(program, args, limits, format, bytes, cancel).await
        }
    }
}

/// The conversion itself, as the child runs it; a panic becomes a refusal.
pub(super) fn reading(format: Format, bytes: &[u8]) -> Reading {
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        super::to_text(format, bytes)
    }));
    match run {
        Ok(Ok(converted)) => Reading::Done(converted),
        Ok(Err((error, pdf))) => Reading::Refused { error, pdf },
        Err(_) => Reading::Refused {
            error: "the file is damaged or uses features the reader does not support".into(),
            pdf: None,
        },
    }
}

async fn cancelled(cancel: Option<&CancellationToken>) {
    match cancel {
        Some(token) => token.cancelled().await,
        None => std::future::pending().await,
    }
}

enum Stop {
    Timeout,
    Cancelled,
    Memory,
}

async fn run_child(
    program: &PathBuf,
    args: &[OsString],
    limits: Limits,
    format: Format,
    bytes: Vec<u8>,
    cancel: Option<&CancellationToken>,
) -> Result<Reading, String> {
    if cancel.is_some_and(|c| c.is_cancelled()) {
        return Err(format!("{UNSAFE}: cancelled"));
    }
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .arg(format.name())
        .env_clear()
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let cpu = limits.timeout.as_secs().max(1) + 1;
    let _memory = limits.memory;
    // SAFETY: setrlimit is async-signal-safe; nothing between fork and exec
    // allocates or takes a lock.
    unsafe {
        cmd.pre_exec(move || {
            set_limit(libc::RLIMIT_CORE, 0)?;
            set_limit(libc::RLIMIT_FSIZE, 0)?;
            set_limit(libc::RLIMIT_CPU, cpu)?;
            #[cfg(target_os = "linux")]
            set_limit(libc::RLIMIT_AS, _memory)?;
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{UNSAFE}: could not start the reader: {e}"))?;
    let pid = child.id();
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let cap = limits.output;

    let feed = async move {
        // A child that exits early closes the pipe; its exit status says why.
        let _ = stdin.write_all(&bytes).await;
        drop(stdin);
        Ok::<(), ()>(())
    };
    let collect = async move {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = stdout.read(&mut buf).await.map_err(|_| ())?;
            if n == 0 {
                return Ok(Some(out));
            }
            if out.len() + n > cap {
                return Ok(None);
            }
            out.extend_from_slice(&buf[..n]);
        }
    };
    let drain = async move {
        let mut kept = Vec::new();
        let mut buf = vec![0u8; 16 * 1024];
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 {
                break;
            }
            let room = (8 * 1024usize).saturating_sub(kept.len());
            kept.extend_from_slice(&buf[..n.min(room)]);
        }
        Ok::<Vec<u8>, ()>(kept)
    };
    let io = async { tokio::try_join!(feed, collect, drain) };
    let stopped = tokio::select! {
        r = io => Ok(r),
        _ = tokio::time::sleep(limits.timeout) => Err(Stop::Timeout),
        _ = cancelled(cancel) => Err(Stop::Cancelled),
        _ = watch_memory(pid, limits.memory) => Err(Stop::Memory),
    };
    let (out, err) = match stopped {
        Ok(Ok(((), Some(out), err))) => (out, err),
        Ok(Ok(((), None, _))) => {
            kill(&mut child).await;
            return Err(format!(
                "{UNSAFE}: the reader produced more than {} KB of output",
                cap / 1024
            ));
        }
        Ok(Err(())) => {
            kill(&mut child).await;
            return Err(format!("{UNSAFE}: lost contact with the reader"));
        }
        Err(stop) => {
            kill(&mut child).await;
            return Err(match stop {
                Stop::Timeout => format!(
                    "{UNSAFE}: the reader took longer than {} s",
                    limits.timeout.as_secs()
                ),
                Stop::Cancelled => format!("{UNSAFE}: cancelled"),
                Stop::Memory => format!(
                    "{UNSAFE}: the reader used more than {} MB of memory",
                    limits.memory / (1024 * 1024)
                ),
            });
        }
    };
    // Output closed: the child is exiting. Do not wait on it forever.
    let status = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) => status,
        _ => {
            kill(&mut child).await;
            return Err(format!("{UNSAFE}: the reader did not exit"));
        }
    };
    if !status.success() {
        return Err(format!("{UNSAFE}: {}", describe_exit(status, &err)));
    }
    parse_reply(&out)
}

async fn kill(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

fn describe_exit(status: std::process::ExitStatus, stderr: &[u8]) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    let what = match status.signal() {
        Some(libc::SIGXCPU) => "the reader ran out of CPU time".to_owned(),
        Some(signal) => format!("the reader crashed (signal {signal})"),
        None => match status.code() {
            Some(code) => format!("the reader failed (exit code {code})"),
            None => "the reader failed".to_owned(),
        },
    };
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        return what;
    }
    let last: String = detail
        .lines()
        .last()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect();
    format!("{what}: {last}")
}

/// Lower `resource` to `value`, never above the hard limit already set.
fn set_limit(resource: LimitResource, value: u64) -> std::io::Result<()> {
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: plain syscalls on stack values.
    if unsafe { libc::getrlimit(resource, &mut current) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let value = (value as libc::rlim_t).min(current.rlim_max);
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    // SAFETY: as above.
    if unsafe { libc::setrlimit(resource, &limit) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
type LimitResource = libc::__rlimit_resource_t;
#[cfg(not(target_os = "linux"))]
type LimitResource = libc::c_int;

/// Resolves when the child's physical footprint passes `limit` (macOS,
/// which does not enforce `RLIMIT_AS`); never elsewhere.
async fn watch_memory(pid: Option<u32>, limit: u64) {
    #[cfg(target_os = "macos")]
    if let Some(pid) = pid {
        loop {
            tokio::time::sleep(Duration::from_millis(25)).await;
            match footprint(pid) {
                Some(bytes) if bytes > limit => return,
                Some(_) => {}
                None => break,
            }
        }
    }
    let _ = (pid, limit);
    std::future::pending::<()>().await
}

#[cfg(target_os = "macos")]
fn footprint(pid: u32) -> Option<u64> {
    // SAFETY: rusage_info_v2 is plain data, filled in by the kernel.
    unsafe {
        let mut info: libc::rusage_info_v2 = std::mem::zeroed();
        let r = libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V2,
            &mut info as *mut libc::rusage_info_v2 as *mut libc::rusage_info_t,
        );
        (r == 0).then_some(info.ri_phys_footprint)
    }
}

/// The reply in `out`.
fn parse_reply(out: &[u8]) -> Result<Reading, String> {
    let rest = out
        .strip_prefix(MAGIC)
        .ok_or_else(|| format!("{UNSAFE}: the reader gave no valid reply"))?;
    let end = rest
        .iter()
        .position(|b| *b == b'\n')
        .ok_or_else(|| format!("{UNSAFE}: the reader gave no valid reply"))?;
    let header: Header = serde_json::from_slice(&rest[..end])
        .map_err(|_| format!("{UNSAFE}: the reader gave no valid reply"))?;
    if !header.ok {
        return Ok(Reading::Refused {
            error: header.error.unwrap_or_else(|| "unknown error".into()),
            pdf: header.pdf,
        });
    }
    let text = String::from_utf8_lossy(&rest[end + 1..]).into_owned();
    Ok(Reading::Done(Converted {
        text,
        truncated: header.truncated,
        pdf: header.pdf,
    }))
}

/// The reply for `reading`, as the child writes it.
fn reply(reading: &Reading) -> Vec<u8> {
    let (header, text) = match reading {
        Reading::Done(c) => (
            Header {
                ok: true,
                error: None,
                truncated: c.truncated,
                pdf: c.pdf,
            },
            c.text.as_str(),
        ),
        Reading::Refused { error, pdf } => (
            Header {
                ok: false,
                error: Some(error.clone()),
                truncated: false,
                pdf: *pdf,
            },
            "",
        ),
    };
    let mut out = MAGIC.to_vec();
    out.extend(serde_json::to_vec(&header).unwrap_or_default());
    out.push(b'\n');
    out.extend_from_slice(text.as_bytes());
    out
}

/// The child: stdin to a reply on stdout. 0 unless stdout fails.
fn child_main(format: &str) -> i32 {
    // Panics are reported in the reply; the default hook would print them.
    std::panic::set_hook(Box::new(|_| {}));
    let reading = match Format::from_name(format) {
        None => Reading::Refused {
            error: format!("unknown document format `{format}`"),
            pdf: None,
        },
        Some(format) => {
            let mut bytes = Vec::new();
            let read = std::io::stdin()
                .lock()
                .take(MAX_INPUT_BYTES + 1)
                .read_to_end(&mut bytes);
            match read {
                Err(e) => Reading::Refused {
                    error: format!("could not read the input: {e}"),
                    pdf: None,
                },
                Ok(_) if bytes.len() as u64 > MAX_INPUT_BYTES => Reading::Refused {
                    error: super::too_large(bytes.len() as u64),
                    pdf: None,
                },
                Ok(_) => reading(format, &bytes),
            }
        }
    };
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(&reply(&reading))
        .and_then(|()| stdout.flush())
    {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> DocumentReader {
        DocumentReader::Child {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into(), "sh".into()],
        }
    }

    fn quick() -> Limits {
        Limits {
            timeout: Duration::from_secs(5),
            ..Limits::default()
        }
    }

    #[test]
    fn replies_round_trip() {
        let done = Reading::Done(Converted {
            text: "a\nb".into(),
            truncated: true,
            pdf: Some(PdfInfo {
                readable: false,
                encrypted: true,
            }),
        });
        assert_eq!(parse_reply(&reply(&done)).unwrap(), done);
        let refused = Reading::Refused {
            error: "bad".into(),
            pdf: None,
        };
        assert_eq!(parse_reply(&reply(&refused)).unwrap(), refused);
        assert!(parse_reply(b"running 0 tests\n").is_err());
    }

    #[tokio::test]
    async fn a_reply_from_the_child_is_read() {
        let reply = String::from_utf8(reply(&Reading::Done(Converted {
            text: "hello".into(),
            ..Default::default()
        })))
        .unwrap();
        let script = format!("cat >/dev/null; printf '%s' '{reply}'");
        let got = convert(&sh(&script), quick(), Format::Docx, b"x".to_vec(), None)
            .await
            .unwrap();
        let Reading::Done(c) = got else {
            panic!("{got:?}")
        };
        assert_eq!(c.text, "hello");
    }

    #[tokio::test]
    async fn the_child_gets_no_environment() {
        std::env::set_var("LOOP_DOC_CHILD_SECRET_PROBE", "s3cret");
        let script = r#"cat >/dev/null; printf 'LOOPDOC1\n{"ok":true}\n%s' "${LOOP_DOC_CHILD_SECRET_PROBE:-none}""#;
        let got = convert(&sh(script), quick(), Format::Docx, vec![], None)
            .await
            .unwrap();
        assert_eq!(
            got,
            Reading::Done(Converted {
                text: "none".into(),
                ..Default::default()
            })
        );
    }

    #[tokio::test]
    async fn a_crash_is_an_error_not_a_crash_here() {
        let err = convert(
            &sh("kill -SEGV $$"),
            quick(),
            Format::Pdf,
            vec![0; 1024 * 1024],
            None,
        )
        .await
        .unwrap_err();
        assert!(err.starts_with(UNSAFE), "{err}");
        assert!(err.contains("crashed (signal 11)"), "{err}");
    }

    #[tokio::test]
    async fn stderr_is_captured_into_the_error() {
        let err = convert(
            &sh("echo 'thread main panicked' >&2; exit 3"),
            quick(),
            Format::Pdf,
            vec![],
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("(exit code 3): thread main panicked"), "{err}");
    }

    #[tokio::test]
    async fn a_slow_child_is_stopped_at_the_timeout() {
        let limits = Limits {
            timeout: Duration::from_millis(300),
            ..Limits::default()
        };
        let started = std::time::Instant::now();
        let err = convert(&sh("sleep 30"), limits, Format::Pdf, vec![], None)
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(err.contains("took longer"), "{err}");
    }

    #[tokio::test]
    async fn a_child_over_the_memory_limit_is_stopped() {
        if !std::path::Path::new("/usr/bin/perl").exists() {
            return;
        }
        let reader = DocumentReader::Child {
            program: "/usr/bin/perl".into(),
            args: vec![
                "-e".into(),
                "my $x = 'a' x (1024 * 1024 * 1024); sleep 20;".into(),
            ],
        };
        let limits = Limits {
            timeout: Duration::from_secs(15),
            memory: 256 * 1024 * 1024,
            ..Limits::default()
        };
        let started = std::time::Instant::now();
        let err = convert(&reader, limits, Format::Pdf, vec![], None)
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10), "{err}");
        assert!(err.starts_with(UNSAFE), "{err}");
    }

    #[tokio::test]
    async fn endless_output_is_cut_off() {
        let err = convert(&sh("yes"), quick(), Format::Pdf, vec![], None)
            .await
            .unwrap_err();
        assert!(err.contains("more than"), "{err}");
    }

    #[tokio::test]
    async fn garbage_output_is_not_taken_for_text() {
        let err = convert(
            &sh("echo 'running 0 tests'"),
            quick(),
            Format::Pdf,
            vec![],
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("no valid reply"), "{err}");
    }

    #[tokio::test]
    async fn cancel_kills_the_child() {
        let token = CancellationToken::new();
        let later = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            later.cancel();
        });
        let started = std::time::Instant::now();
        let err = convert(
            &sh("sleep 30"),
            Limits::default(),
            Format::Pdf,
            vec![],
            Some(&token),
        )
        .await
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(err.contains("cancelled"), "{err}");
    }
}
