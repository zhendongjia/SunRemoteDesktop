#[cfg(windows)]
mod windows_service_impl {
    use std::ffi::OsString;
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

    use crate::{config, host};

    const SERVICE_NAME: &str = "RdpDesktopHost";

    pub fn run() -> Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .map_err(|error| anyhow::anyhow!("无法启动 Windows 服务入口：{error}"))?;
        Ok(())
    }

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(error) = service_main_inner() {
            eprintln!("RdpDesktopHost service stopped: {error:#}");
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

        let config_path = config::config_path();
        let (done_sender, done_receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| runtime.block_on(host::run_server(&config_path)));
            let _ = done_sender.send(result);
        });

        loop {
            if shutdown_receiver.try_recv().is_ok() {
                break;
            }
            if let Ok(result) = done_receiver.try_recv() {
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
}

#[cfg(windows)]
pub fn run() -> anyhow::Result<()> {
    windows_service_impl::run()
}

#[cfg(not(windows))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("Windows 服务入口只在 Windows 构建中可用")
}
