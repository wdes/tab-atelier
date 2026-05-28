// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Windows Service (SCM) integration — the analogue of the Linux
//! systemd unit. When the Service Control Manager launches
//! tab-atelier-headless.exe (the MSI installs it with Start=auto), the
//! process runs under the dispatcher here, so there is NO console
//! window. A normal console launch detects "not started by the SCM"
//! and falls back to the console path in the binary's `main()`.

// The windows-service dispatcher macro expands, inside this crate, to
// an `extern "system"` entry point that dereferences raw SCM argument
// pointers — which requires `unsafe`. windows-service is the maintained
// wrapper for exactly this; confine the crate-wide `unsafe_code = deny`
// exception to this one file.
#![allow(unsafe_code)]

use std::ffi::OsString;
use std::sync::atomic::Ordering;
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

const SERVICE_NAME: &str = "tab-atelier-headless";

define_windows_service!(ffi_service_main, service_main);

/// Attempt to run under the SCM. Returns `true` if we were launched as
/// the installed service (the dispatcher ran the daemon and has since
/// stopped); returns `false` if not started by the SCM, in which case
/// the caller should run the normal console path.
#[must_use]
pub fn try_run_as_service() -> bool {
    // `start` blocks until the service stops when launched by the SCM;
    // from a console it returns ERROR_FAILED_SERVICE_CONTROLLER_CONNECT
    // almost immediately.
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).is_ok()
}

fn service_main(_args: Vec<OsString>) {
    // Nowhere useful to surface an error (no console); the SCM marks the
    // service failed if we never report Running.
    let _ = run_service();
}

fn run_service() -> windows_service::Result<()> {
    let event_handler = move |control| match control {
        // Same shutdown path as the ctrlc handler / SIGTERM:
        // headless::run() polls this flag and flushes state.
        ServiceControl::Stop | ServiceControl::Shutdown => {
            crate::SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // Owns the daemon loop until a Stop/Shutdown control flips
    // SHUTDOWN_REQUESTED, after which run() returns and we report
    // Stopped below.
    let _ = crate::headless::run();

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}
