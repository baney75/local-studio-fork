pub use local_studio_core::*;

uniffi::setup_scaffolding!("local_studio_kit");

#[cfg(unix)]
mod remote {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, PoisonError};

    use local_studio_core::Studio;
    use local_studio_remote::host::{self, Handle};

    #[derive(uniffi::Object)]
    pub struct RemoteHost(Mutex<Option<Handle>>);

    #[uniffi::export]
    impl RemoteHost {
        pub fn stop(&self) {
            let handle = self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(handle) = handle {
                handle.stop();
            }
        }
    }

    #[uniffi::export]
    pub fn start_remote_host(studio: Arc<Studio>, data_dir: Option<String>) -> Option<Arc<RemoteHost>> {
        let data_dir = data_dir.map(PathBuf::from);
        match host::start(studio, data_dir.as_deref()) {
            Ok(handle) => handle.map(|handle| Arc::new(RemoteHost(Mutex::new(Some(handle))))),
            Err(error) => {
                eprintln!("local-studio-serve: the in-process server did not start: {error:#}");
                None
            }
        }
    }
}
