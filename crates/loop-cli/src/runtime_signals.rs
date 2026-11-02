//! Signal handling: the Runtime command's graceful shutdown path, and the
//! terminal `loop`'s release of its Session leases on SIGTERM and SIGHUP.

/// Wait for SIGINT or SIGTERM. Registration failures are returned to the caller.
/// Platforms without Unix signals report Unsupported.
pub async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Runtime shutdown signals are unsupported on this platform",
        ))
    }
}

/// On SIGTERM or SIGHUP (and SIGINT when `interrupt`), end the Session work,
/// release its leases and exit with 128 + the signal number. With
/// `restore_terminal`, leave the TUI's raw mode first. Unix only; elsewhere
/// this does nothing.
pub fn release_on_signals(
    shutdown: loop_app_core::runtime::SessionShutdown,
    interrupt: bool,
    restore_terminal: bool,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        let mut interrupted = if interrupt {
            Some(signal(SignalKind::interrupt())?)
        } else {
            None
        };
        tokio::spawn(async move {
            let code = tokio::select! {
                _ = terminate.recv() => 143,
                _ = hangup.recv() => 129,
                _ = async {
                    match interrupted.as_mut() {
                        Some(signal) => signal.recv().await,
                        None => std::future::pending().await,
                    }
                } => 130,
            };
            if restore_terminal {
                restore();
            }
            if let Err(error) = shutdown.run().await {
                eprintln!("loop: {error:#}");
            }
            std::process::exit(code);
        });
    }
    #[cfg(not(unix))]
    let _ = (shutdown, interrupt, restore_terminal);
    Ok(())
}

/// Undo what the TUI set up, best effort: the terminal may already be gone.
#[cfg(unix)]
fn restore() {
    use crossterm::{
        cursor::Show,
        event::{DisableBracketedPaste, PopKeyboardEnhancementFlags},
        execute,
    };
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        PopKeyboardEnhancementFlags,
        Show
    );
}
