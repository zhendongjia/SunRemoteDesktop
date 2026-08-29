#[cfg(windows)]
mod windows_service_impl {
    use std::ffi::OsString;
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::Duration;

    use anyhow::Result;
    use windows_service::define_windows_service;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_dispatcher;

    use crate::{bridge, config, host};

    const SERVICE_NAME: &str = "SunRemoteDesktop";
    const MAINTENANCE_WORKER: &str = include_str!("../scripts/maintenance-worker.ps1");

    pub fn run() -> Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .map_err(|error| anyhow::anyhow!("无法启动 Windows 服务入口：{error}"))?;
        Ok(())
    }

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(error) = service_main_inner() {
            eprintln!("SunRemoteDesktop service stopped: {error:#}");
        }
    }

    fn service_main_inner() -> Result<()> {
        let (shutdown_sender, shutdown_receiver) = mpsc::channel();
        let status_handle =
            service_control_handler::register(SERVICE_NAME, move |event| match event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = shutdown_sender.send(());
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            })?;

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        schedule_maintenance_worker_refresh();

        let config_path = config::config_path();
        let console_agent_config = config_path.clone();
        let (console_agent_shutdown_sender, console_agent_shutdown_receiver) = mpsc::channel();
        let console_agent_thread = std::thread::Builder::new()
            .name("sunrdp-console-agent-supervisor".to_string())
            .spawn(move || {
                bridge::windows::supervise_console_agent(
                    console_agent_config,
                    console_agent_shutdown_receiver,
                );
            })?;
        let (runtime_shutdown_sender, runtime_shutdown_receiver) =
            tokio::sync::watch::channel(false);
        let (done_sender, done_receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| {
                    runtime.block_on(host::run_service_server(
                        &config_path,
                        runtime_shutdown_receiver,
                    ))
                });
            let _ = done_sender.send(result);
        });

        let mut stop_requested = false;
        loop {
            if shutdown_receiver.try_recv().is_ok() {
                stop_requested = true;
                let _ = runtime_shutdown_sender.send(true);
                let _ = console_agent_shutdown_sender.send(());
                break;
            }
            if let Ok(result) = done_receiver.try_recv() {
                let _ = console_agent_shutdown_sender.send(());
                if let Err(error) = result {
                    status_handle.set_service_status(ServiceStatus {
                        service_type: ServiceType::OWN_PROCESS,
                        current_state: ServiceState::Stopped,
                        controls_accepted: ServiceControlAccept::empty(),
                        exit_code: ServiceExitCode::ServiceSpecific(1),
                        checkpoint: 0,
                        wait_hint: Duration::default(),
                        process_id: None,
                    })?;
                    return Err(error);
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }

        if stop_requested {
            match done_receiver.recv_timeout(Duration::from_secs(10)) {
                Ok(Err(error)) => return Err(error),
                Ok(Ok(())) => {}
                Err(error) => {
                    tracing::warn!(?error, "timed out waiting for the RDP runtime to stop");
                }
            }
        }
        let _ = console_agent_thread.join();

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

    fn schedule_maintenance_worker_refresh() {
        let maintenance_root = config::data_dir().join("Maintenance");
        let policy_path = maintenance_root.join("policy.json");
        let worker_path = maintenance_root.join("maintenance-worker.ps1");
        std::thread::spawn(move || {
            // During Deploy, the previous worker is still running while the
            // new service starts. Retry until PowerShell releases the script.
            for _ in 0..20 {
                std::thread::sleep(Duration::from_secs(1));
                if !policy_path.is_file() {
                    return;
                }
                if std::fs::read_to_string(&worker_path).ok().as_deref() == Some(MAINTENANCE_WORKER)
                {
                    return;
                }
                let write_result = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(&worker_path)
                    .and_then(|mut file| {
                        file.write_all(MAINTENANCE_WORKER.as_bytes())?;
                        file.sync_all()
                    });
                match write_result {
                    Ok(()) => {
                        tracing::info!(
                            path = %worker_path.display(),
                            "protected maintenance worker refreshed"
                        );
                        return;
                    }
                    Err(error) => {
                        tracing::debug!(
                            ?error,
                            path = %worker_path.display(),
                            "maintenance worker is busy; retrying"
                        );
                    }
                }
            }
            tracing::warn!(
                path = %worker_path.display(),
                "unable to refresh the protected maintenance worker"
            );
        });
    }
}

#[cfg(windows)]
pub fn run() -> anyhow::Result<()> {
    windows_service_impl::run()
}

#[cfg(not(windows))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("Windows 服务入口只在 Windows 构建中可用")
}
