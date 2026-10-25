//! The run board shared between whatever keeps it current and every client
//! watching it.

use std::collections::BTreeMap;
use std::sync::Mutex;

use tokio::sync::broadcast;
use vetinari_core::application::devflow::api::{BoardChange, RunBoard, RunItem};
use vetinari_core::domain::devflow::ids::RunId;
use vetinari_core::domain::devflow::run_view::RunGraph;

/// How many changes a slow watcher may fall behind before it is sent a
/// fresh snapshot instead.
const BACKLOG: usize = 256;

/// The board and its watchers.
pub struct Hub {
    board: Mutex<RunBoard>,
    changes: broadcast::Sender<BoardChange>,
    graphs: Mutex<BTreeMap<RunId, RunGraph>>,
    touched: broadcast::Sender<RunId>,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            board: Mutex::new(RunBoard::default()),
            changes: broadcast::channel(BACKLOG).0,
            graphs: Mutex::default(),
            touched: broadcast::channel(BACKLOG).0,
        }
    }
}

impl Hub {
    /// Record a run's item and tell the watchers, if it changed.
    pub fn put(&self, item: RunItem) {
        let mut board = self.board.lock().expect("board lock");
        let run = item.summary.run.clone();
        if let Some(change) = board.put(item) {
            let _ = self.changes.send(change);
            let _ = self.touched.send(run);
        }
    }

    /// Record a run's graph, for its detail, and tell its watchers.
    pub fn put_graph(&self, graph: RunGraph) {
        let run = graph.run.clone();
        let mut graphs = self.graphs.lock().expect("graphs lock");
        if graphs.get(&run) == Some(&graph) {
            return;
        }
        graphs.insert(run.clone(), graph);
        drop(graphs);
        let _ = self.touched.send(run);
    }

    /// A run's item.
    pub fn item(&self, run: &RunId) -> Option<RunItem> {
        self.board.lock().expect("board lock").get(run)
    }

    /// A run's item and graph, when it has both.
    pub fn run(&self, run: &RunId) -> Option<(RunItem, RunGraph)> {
        let item = self.board.lock().expect("board lock").get(run)?;
        let graph = self.graphs.lock().expect("graphs lock").get(run).cloned()?;
        Some((item, graph))
    }

    /// Hear which run changed next, item or graph.
    pub fn touches(&self) -> broadcast::Receiver<RunId> {
        self.touched.subscribe()
    }

    /// Drop a run and tell the watchers, if it was there.
    pub fn remove(&self, run: &RunId) {
        let mut board = self.board.lock().expect("board lock");
        if let Some(change) = board.remove(run) {
            let _ = self.changes.send(change);
            let _ = self.touched.send(run.clone());
        }
        self.graphs.lock().expect("graphs lock").remove(run);
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
