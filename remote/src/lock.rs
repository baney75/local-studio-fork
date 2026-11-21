use std::path::{Path, PathBuf};

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("studio.lock")
}

pub fn recorded(data_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(path(data_dir)).ok()?.trim().parse().ok()
}

pub enum Owner {
    Free,
    Held(Option<u32>),
}

pub fn holder(data_dir: &Path) -> Owner {
    let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(path(data_dir)) else { return Owner::Free };
    if matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)) {
        Owner::Held(recorded(data_dir))
    } else {
        Owner::Free
    }
}

pub fn describe(pid: Option<u32>) -> String {
    match pid {
        Some(pid) => match local_studio_core::process_name(pid) {
            Some(name) => format!("process {pid} ({name}, {})", kind(&name)),
            None => format!("process {pid}"),
        },
        None => "another process".to_string(),
    }
}

pub fn kind(name: &str) -> &'static str {
    match name {
        "local-studio-serve" => "serve",
        "LocalStudio" | "local-studio" => "the desktop app",
        _ => "another Local Studio",
    }
}

pub fn kind_id(name: &str) -> &'static str {
    match name {
        "local-studio-serve" => "serve",
        "LocalStudio" | "local-studio" => "app",
        _ => "other",
    }
}
