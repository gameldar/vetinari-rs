//! The run board shared between whatever keeps it current and every client
//! watching it.

use std::sync::Mutex;

use tokio::sync::broadcast;
use vetinari_core::application::devflow::api::{BoardChange, RunBoard, RunItem};
use vetinari_core::domain::devflow::ids::RunId;

/// How many changes a slow watcher may fall behind before it is sent a
/// fresh snapshot instead.
const BACKLOG: usize = 256;

/// The board and its watchers.
pub struct Hub {
    board: Mutex<RunBoard>,
    changes: broadcast::Sender<BoardChange>,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            board: Mutex::new(RunBoard::default()),
            changes: broadcast::channel(BACKLOG).0,
        }
    }
}

impl Hub {
    /// Record a run's item and tell the watchers, if it changed.
    pub fn put(&self, item: RunItem) {
        let mut board = self.board.lock().expect("board lock");
        if let Some(change) = board.put(item) {
            let _ = self.changes.send(change);
        }
    }

    /// Drop a run and tell the watchers, if it was there.
    pub fn remove(&self, run: &RunId) {
        let mut board = self.board.lock().expect("board lock");
        if let Some(change) = board.remove(run) {
            let _ = self.changes.send(change);
        }
    }

    /// The board now and every change after it, taken together so nothing
    /// falls between them.
    pub fn watch(&self) -> (Vec<RunItem>, broadcast::Receiver<BoardChange>) {
        let board = self.board.lock().expect("board lock");
        (board.snapshot(), self.changes.subscribe())
    }

    /// The board now.
    pub fn snapshot(&self) -> Vec<RunItem> {
        self.board.lock().expect("board lock").snapshot()
    }
}
