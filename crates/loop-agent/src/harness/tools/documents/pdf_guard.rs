//! Checks on a loaded PDF before pdf-extract walks it.

/// Refuse what would make pdf-extract misbehave. (Filled in below.)
pub(super) fn check(_doc: &pdf_extract::Document) -> Result<(), String> {
    Ok(())
}
