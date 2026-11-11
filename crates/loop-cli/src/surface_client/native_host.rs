//! Private native owner. No Surface method returns this type.
pub(super) struct NativeHost {
    pub(super) inner: loop_app_core::Runtime,
}
impl NativeHost {
    pub(super) fn clear_session_approvals(&self) {
        if let Some(bridge) = &self.inner.tool_approval {
            bridge.clear_session_grants();
        }
    }
    pub(super) async fn restore_session_approvals(&self) {
        self.clear_session_approvals();
        if let Some(bridge) = &self.inner.tool_approval {
            if let Ok(entries) = self.inner.harness.read_session_entries().await {
                for group in crate::tool_approval::auto_approve_from_entries(&entries) {
                    bridge.grant_session(&group);
                }
            }
        }
    }
}
