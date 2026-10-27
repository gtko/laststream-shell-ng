pub mod player;
pub use player::Player;
pub mod communication;
pub use communication::{
    CmdVal, InMsg, InMsgArgs, InMsgFn, PlayerEnded, PlayerEvent, PlayerProprChange, PlayerResponse,
    PropKey, PropVal,
};
#[cfg(test)]
mod communication_tests;
pub mod vlc_backend;
pub mod vlc_ffi;
pub mod vlc_player;
