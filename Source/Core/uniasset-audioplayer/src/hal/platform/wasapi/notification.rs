use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::{implement, Result as WinResult, PCWSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eConsole, eRender, EDataFlow, ERole, IMMNotificationClient, IMMNotificationClient_Impl,
    DEVICE_STATE,
};

use super::device::WakeEvent;

pub(super) struct DeviceChangeSignal {
    requested: AtomicBool,
    wake: Arc<WakeEvent>,
}

impl DeviceChangeSignal {
    pub(super) fn new(wake: Arc<WakeEvent>) -> Arc<Self> {
        Arc::new(Self {
            requested: AtomicBool::new(false),
            wake,
        })
    }

    pub(super) fn request(&self) {
        self.requested.store(true, Ordering::Release);
        let _ = self.wake.signal();
    }

    pub(super) fn take(&self) -> bool {
        self.requested.swap(false, Ordering::AcqRel)
    }
}

/// Notification callbacks only set an atomic and wake the worker.
#[implement(IMMNotificationClient)]
pub(super) struct NotificationClient {
    signal: Arc<DeviceChangeSignal>,
}

impl NotificationClient {
    pub(super) fn new(signal: Arc<DeviceChangeSignal>) -> Self {
        Self { signal }
    }
}

impl IMMNotificationClient_Impl for NotificationClient_Impl {
    fn OnDeviceStateChanged(&self, _device_id: &PCWSTR, _new_state: DEVICE_STATE) -> WinResult<()> {
        self.signal.request();
        Ok(())
    }

    fn OnDeviceAdded(&self, _device_id: &PCWSTR) -> WinResult<()> {
        self.signal.request();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _device_id: &PCWSTR) -> WinResult<()> {
        self.signal.request();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _device_id: &PCWSTR,
    ) -> WinResult<()> {
        if flow == eRender && role == eConsole {
            self.signal.request();
        }
        Ok(())
    }

    fn OnPropertyValueChanged(&self, _device_id: &PCWSTR, _key: &PROPERTYKEY) -> WinResult<()> {
        Ok(())
    }
}

pub(super) fn into_interface(client: NotificationClient) -> IMMNotificationClient {
    client.into()
}
