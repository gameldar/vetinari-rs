//! Configuration loading.

pub mod loader;

pub use loader::{
    APP_DIR, LoadError, LoadedConfig, candidate_paths, find_config, load_file, load_or_default,
};
