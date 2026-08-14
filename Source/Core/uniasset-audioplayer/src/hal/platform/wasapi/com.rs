//! COM apartment ownership for the WASAPI worker.

use crate::error::AudioError;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// RAII guard for the worker thread's MTA.
pub(crate) struct ComGuard;

impl ComGuard {
    pub(crate) fn initialize() -> Result<Self, AudioError> {
        // SAFETY: called by the thread that owns all WASAPI interfaces.
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        result.ok().map(|_| Self).map_err(|error| {
            AudioError::BackendError(format!("WASAPI COM initialization: {error}"))
        })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: paired with initialize(), on the same thread.
        unsafe { CoUninitialize() };
    }
}
