//! Building a snapshot from the model.

use std::collections::HashMap;
use std::path::PathBuf;

use super::{OptionSnap, PaneSnap, SNAPSHOT_VERSION, SessionSnap, Snapshot, WindowSnap};
use crate::server::dispatch::options::Owner;
use crate::server::layout::{GeometryCell, LayoutGeometry};
use crate::server::model::{Model, Window};
use crate::types::{PaneId, SessionId};

/// The options at exactly `owner` that carry the `--persist` mark.
fn persisted(model: &Model, owner: Owner) -> Vec<OptionSnap> {
    // ponytail: scalar options only; list options (`status-format[1]`)
    // keep their mark but are not saved.
    model
        .options
        .at(owner)
        .filter(|(_, e)| e.persist)
        .map(|(name, e)| OptionSnap {
            name: name.clone(),
            value: e.value.clone(),
        })
        .collect()
}

/// The snapshot of `model` now, `written_at` left empty (set at write).
/// `cwds` holds each pane's last sampled cwd; a pane not sampled yet has
/// the cwd it was spawned in.
pub fn build(model: &Model, cwds: &HashMap<PaneId, PathBuf>) -> Snapshot {
    // Least recently used first: the restore makes them in this order, each
    // more recent than the one before, so the session used last is the one
    // a bare `grofe` attaches to again.
    let mut by_use: Vec<_> = model.sessions.values().collect();
    by_use.sort_by_key(|s| s.activity);
    let sessions = by_use
        .into_iter()
        .map(|s| SessionSnap {
            name: s.name.clone(),
            cwd: s.cwd.clone(),
            active_window: model.index_in(s.id, s.active).unwrap_or_default(),
            options: persisted(model, Owner::Session(s.id)),
            // ponytail: a window linked into several sessions is saved
            // only in its first one, so no uid is saved twice.
            windows: s
                .windows
                .iter()
                .filter_map(|id| model.windows.get(id))
                .filter(|w| w.session == s.id)
                .map(|w| window(model, s.id, w, cwds))
                .collect(),
        })
        .collect();
    Snapshot {
        version: SNAPSHOT_VERSION,
        written_at: String::new(),
        grofe_version: env!("CARGO_PKG_VERSION").to_string(),
        options: persisted(model, Owner::Global),
        sessions,
    }
}

fn window(
    model: &Model,
    session: SessionId,
    w: &Window,
    cwds: &HashMap<PaneId, PathBuf>,
) -> WindowSnap {
    let uid = |id: PaneId| {
        model
            .panes
            .get(&id)
            .map(|p| p.uid.clone())
            .unwrap_or_default()
    };
    let chain = crate::server::dispatch::options::chain(model, w.active);
    // ponytail: floating panes (outside the layout) are not saved.
    let panes = w
        .tiled_ids()
        .into_iter()
        .filter_map(|id| model.panes.get(&id))
        .map(|p| PaneSnap {
            uid: p.uid.clone(),
            cwd: cwds.get(&p.id).unwrap_or(&p.cwd).clone(),
            argv: p.argv.clone(),
            resume: p.resume.clone(),
            options: persisted(model, Owner::Pane(p.id)),
        })
        .collect();
    WindowSnap {
        index: model.index_in(session, w.id).unwrap_or(w.index),
        name: w.name.clone(),
        automatic_rename: model.options.lookup("automatic-rename", &chain) == Some("on"),
        cols: w.cols,
        rows: w.rows,
        layout: by_uid(w.layout.export(w.area()), &uid),
        active_pane: uid(w.active),
        zoomed: w.zoomed,
        options: persisted(model, Owner::Window(w.id)),
        panes,
    }
}

/// `g` with its cells keyed by pane `uid` instead of runtime id.
fn by_uid(g: LayoutGeometry, uid: &impl Fn(PaneId) -> String) -> LayoutGeometry<String> {
    let cell = match g.cell {
        GeometryCell::Pane(id) => GeometryCell::Pane(uid(id)),
        GeometryCell::Split { dir, children } => GeometryCell::Split {
            dir,
            children: children.into_iter().map(|c| by_uid(c, uid)).collect(),
        },
    };
    LayoutGeometry {
        x: g.x,
        y: g.y,
        cols: g.cols,
        rows: g.rows,
        cell,
    }
}
