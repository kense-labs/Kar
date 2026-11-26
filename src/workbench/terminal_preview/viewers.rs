//! A viewer owns its grid and query-reply vote while any of its attach streams is alive.
use super::*;
use std::collections::hash_map::Entry;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Viewer {
    pub device: Option<Uuid>,
    pub id: Uuid,
}
type Grid = (u16, u16);
struct View {
    streams: usize,
    grid: Option<Grid>,
}
#[derive(Default)]
pub(super) struct Viewers {
    attached: HashMap<Viewer, View>,
    reply_owner: Option<Viewer>,
}
impl Viewers {
    fn shared(&self) -> Option<Grid> {
        self.attached
            .values()
            .filter_map(|v| v.grid)
            .reduce(|(a, b), (c, d)| (a.min(c), b.min(d)))
    }
    fn release(&mut self, viewer: Viewer) {
        if let Entry::Occupied(mut entry) = self.attached.entry(viewer) {
            let v = entry.get_mut();
            v.streams -= 1;
            if v.streams == 0 {
                entry.remove();
                if self.reply_owner == Some(viewer) {
                    self.reply_owner = None;
                }
            }
        }
    }
    pub(super) fn can_answer(&mut self, viewer: Option<Viewer>) -> bool {
        if viewer.is_some_and(|v| !self.attached.contains_key(&v)) {
            return false;
        }
        if let Some(owner) = self.reply_owner {
            return viewer == Some(owner);
        }
        self.reply_owner = viewer;
        true
    }
}

pub(super) struct Lease {
    term: Arc<Terminal>,
    viewer: Viewer,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut views = self.term.viewers.lock().unwrap();
        views.release(self.viewer);
        if let Some(grid) = views.shared()
            && let Err(err) = self.term.apply_grid(grid)
        {
            eprintln!("Kar could not restore the shared terminal grid: {err}");
        }
    }
}
impl Terminal {
    pub(super) fn attach(self: &Arc<Self>, viewer: Viewer, grid: Option<Grid>) -> Result<Lease> {
        if let Some((c, r)) = grid {
            size(c, r)?;
        }
        let mut views = self.viewers.lock().unwrap();
        let prior_grid = views.attached.get(&viewer).and_then(|v| v.grid);
        let view = views
            .attached
            .entry(viewer)
            .or_insert(View { streams: 0, grid });
        view.streams += 1;
        if view.grid.is_none() {
            view.grid = grid;
        }
        if let Some(grid) = views.shared()
            && let Err(e) = self.apply_grid(grid)
        {
            views.release(viewer);
            if let Some(view) = views.attached.get_mut(&viewer) {
                view.grid = prior_grid;
            }
            return Err(e);
        }
        Ok(Lease {
            term: self.clone(),
            viewer,
        })
    }
    pub(super) fn resize_view(&self, viewer: Option<Viewer>, cols: u16, rows: u16) -> Result<Meta> {
        size(cols, rows)?;
        let mut views = self.viewers.lock().unwrap();
        if let Some(id) = viewer
            && let Some(view) = views.attached.get_mut(&id)
        {
            let old = view.grid.replace((cols, rows));
            let grid = views.shared().expect("attached viewer has grid");
            if let Err(err) = self.apply_grid(grid) {
                views
                    .attached
                    .get_mut(&id)
                    .expect("viewer remains attached")
                    .grid = old;
                return Err(err);
            }
        } else if views.shared().is_none() {
            // Legacy unfenced writes retain their original closed-terminal error.
            self.resize(cols, rows)?;
        }
        Ok(self.snapshot())
    }
    fn apply_grid(&self, (cols, rows): Grid) -> Result<()> {
        let live = self.live.lock().unwrap();
        let m = self.meta.lock().unwrap().clone();
        if m.closed || !matches!(m.status, Status::Running) || (m.cols, m.rows) == (cols, rows) {
            return Ok(());
        }
        if let Some(pty) = live.as_ref() {
            pty.master.resize(size(cols, rows)?).map_err(pty_error)?;
            self.update(|m| {
                m.cols = cols;
                m.rows = rows;
            })?;
        }
        Ok(())
    }
}
