mod access;
mod cli;
mod devices;
pub mod host;
mod hub;
mod limits;
mod lock;
mod noise;
mod ops;
mod server;
mod service;
mod settings;

pub use cli::cli;

const VERSION: &str = env!("LOCAL_STUDIO_BUILD_VERSION");
const CHANNEL: &str = env!("LOCAL_STUDIO_BUILD_CHANNEL");
