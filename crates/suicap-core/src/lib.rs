//! SuiCap core: GPU capture -> hardware encode -> RAM replay buffer -> mux.
//! No Tauri dependency (plan rule 4); driven by `suicap-cli` and later the shell.

pub mod apps;
pub mod audio;
pub mod buffer;
pub mod capture;
pub mod clips;
pub mod config;
pub mod encode;
pub mod error;
pub mod fault;
pub mod game;
pub mod game_exes;
pub mod mux;
pub mod pipeline;
pub mod preprocess;
pub mod probe;
pub mod shot;
pub mod targets;

pub use error::{Error, Result};
