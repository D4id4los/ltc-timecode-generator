//! Command sink that assigns sender-side sequence numbers.
//!
//! The engine acks every command by bumping
//! `AppStateSnapshot.applied_command_seq`; a shadow edit sent at seq N is
//! confirmed once the published snapshot satisfies
//! `applied_command_seq >= N`. The GUI is the sole producer on the command
//! channel, so all sends must go through this sink to keep the sender's
//! sequence 1:1 with the engine counter — a bypass makes the engine counter
//! run ahead and causes premature acks.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use gui_engine::command::GuiCommand;

pub struct CommandSink {
    tx: mpsc::Sender<GuiCommand>,
    next_send_seq: AtomicU64,
}

impl CommandSink {
    pub fn new(tx: mpsc::Sender<GuiCommand>) -> Self {
        Self {
            tx,
            next_send_seq: AtomicU64::new(0),
        }
    }

    /// Send a command and return its sender-assigned sequence number.
    pub fn send(&self, cmd: GuiCommand) -> u64 {
        let seq = self.next_send_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.tx.send(cmd);
        seq
    }
}
