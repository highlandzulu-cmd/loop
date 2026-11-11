//! Loop desktop — GPUI coding agent UI.

use std::path::PathBuf;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    // `loop-desktop __doc-to-text <format>` is the document reader child of
    // the read and write tools; otherwise this binary becomes that reader.
    loop_agent::harness::run_document_reader_child();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();

    let cwd = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    if let Err(error) = loop_desktop::run_desktop(cwd).await {
        eprintln!("loop-desktop: {error:#}");
        std::process::exit(1);
    }
}
