//! Apply calculated columns, row guides and document baseline settings together.

use designcraft_doc::{Columns, GridRelative, Guide, GuideSel, Orientation, SpreadRef};
use serde_json::{Value, json};

use super::{CommandSpec, bad, cmd, guides, has_doc, spread_param};
use crate::modular_grid::{GridFit, GridRequest};
use crate::{Result, Session};

const ID: &str = "layout.modularBaselineGrid";

pub(super) fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            query "layout.calculateModularBaselineGrid",
            "Calculate Modular Baseline Grid",
            [],
            None,
            "{spread?, page? (zero-based within spread, default 0), columns (1..216), rows (1..1000), columnGutter (pt), gutterLines (row gutter in whole baseline intervals), leading (pt), fit (true to fit the type area, false to retain leading)} → {effectiveLeading, linesPerRow, moduleWidth, moduleHeight, rowGutter, remainder} in points except the interval count",
            has_doc,
            |s, p| run(s, p, false)
        ),
        cmd!(
            "layout.modularBaselineGrid",
            "Apply Modular Baseline Grid",
            [],
            None,
            "{…settings as layout.calculateModularBaselineGrid} — set the document baseline from each page's top margin, set the target page's columns and replace its calculator row guides. Returns the calculated dimensions",
            has_doc,
            |s, p| run(s, p, true)
        ),
    ]
}

fn row_guides(top: f64, request: GridRequest, grid: GridFit) -> Vec<Guide> {
    let mut guides = Vec::new();
    let mut edge = |interval: u32| {
        guides.push(Guide {
            orientation: Orientation::Horizontal,
            position: top + f64::from(interval) * grid.effective_leading,
            spread: false,
            locked: false,
            layer: None,
            liquid: false,
            modular_row: true,
        });
    };
    let stride = grid.lines_per_row + request.gutter_lines;
    for row in 1..request.rows {
        edge(row * stride - request.gutter_lines);
        if request.gutter_lines > 0 {
            edge(row * stride);
        }
    }
    if grid.remainder > 0.0 {
        edge((request.rows - 1) * stride + grid.lines_per_row);
    }
    guides
}

fn run(s: &mut Session, params: &Value, apply: bool) -> Result<Value> {
    let id = if apply { ID } else { "layout.calculateModularBaselineGrid" };
    let spread = spread_param(params, id)?;
    let page = match params.get("page") {
        None => 0,
        Some(v) => v.as_u64().and_then(|i| usize::try_from(i).ok()).ok_or_else(|| bad(id, "page must be a non-negative integer"))?,
    };
    let doc = &s.doc()?.doc;
    let target = doc.spread(spread).and_then(|sp| sp.pages.get(page)).ok_or_else(|| bad(id, "no such page"))?;
    let request: GridRequest = serde_json::from_value(params.clone()).map_err(|e| bad(id, e.to_string()))?;
    let grid = request.calculate(target).map_err(|e| bad(id, e))?;
    let result = json!(grid);
    if !apply {
        return Ok(result);
    }

    let rows = row_guides(target.margins.top, request, grid);
    let replace_rows = !target.guides.iter().filter(|g| g.modular_row).eq(&rows);
    let columns = Columns { count: request.columns, gutter: request.column_gutter, positions: None };
    let baseline = &doc.settings.baseline_grid;
    if !replace_rows
        && target.modular_grid == Some(request)
        && target.columns == columns
        && (matches!(spread, SpreadRef::Parent(_)) || target.local_margins)
        && baseline.start == 0.0
        && baseline.increment == grid.effective_leading
        && baseline.relative_to == GridRelative::TopMargin
    {
        return Ok(result);
    }
    let mut removed = Vec::new();
    if replace_rows {
        for (index, guide) in target.guides.iter().enumerate().filter(|(_, g)| g.modular_row) {
            if !guide.editable_in(doc) {
                return Err(bad(id, "Unlock the previous grid's row guides before replacing them."));
            }
            removed.push(GuideSel { spread, page, index });
        }
    }

    s.edit(|doc, sel| {
        if !removed.is_empty() {
            guides::remove_guides(doc, sel, &removed, id)?;
        }
        let target = doc.spread_mut(spread).and_then(|sp| sp.pages.get_mut(page)).ok_or_else(|| bad(id, "no such page"))?;
        if replace_rows {
            target.guides.extend(rows);
        }
        target.modular_grid = Some(request);
        target.columns = columns;
        match spread {
            SpreadRef::Doc(_) => target.local_margins = true,
            SpreadRef::Parent(_) => doc.sync_parent_margins(),
        }
        let baseline = &mut doc.settings.baseline_grid;
        baseline.start = 0.0;
        baseline.increment = grid.effective_leading;
        baseline.relative_to = GridRelative::TopMargin;
        Ok(result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Value {
        json!({"columns": 3, "rows": 4, "columnGutter": 8.5, "gutterLines": 1, "leading": 13.2, "fit": true})
    }

    #[test]
    fn fit_chooses_the_closest_leading_with_every_row_edge_on_a_baseline() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"height": 487, "margins": 36})).unwrap();
        let before = s.doc().unwrap().doc.clone();
        let mut p = request();
        // Four rows plus three gutter intervals: 1 or 2 intervals per row are the
        // closest candidates. Rounding the ideal count down would choose the worse one.
        let area = before.spreads[0].pages[0].margin_rect();
        p["leading"] = json!(area.height() / 8.8);
        let result = s.execute("layout.calculateModularBaselineGrid", &p).unwrap();
        assert!(std::sync::Arc::ptr_eq(&before, &s.doc().unwrap().doc));
        assert_eq!(result, s.execute(ID, &p).unwrap());
        let doc = &s.doc().unwrap().doc;
        let grid = &doc.settings.baseline_grid;
        let pg = &doc.spreads[0].pages[0];
        assert_eq!(grid.relative_to, GridRelative::TopMargin);
        assert_eq!(grid.origin(pg), area.y0);
        assert_eq!(result["linesPerRow"], 2);
        assert!((area.height() - grid.increment * 11.0).abs() < 1e-9);
        assert_eq!(pg.guides.len(), 6);
        for g in &pg.guides {
            let step = (g.position - area.y0) / grid.increment;
            assert!((step - step.round()).abs() < 1e-9);
        }
        assert!((result["moduleWidth"].as_f64().unwrap() - pg.column_rects()[0].width()).abs() < 1e-9);
        s.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(*before, *s.doc().unwrap().doc);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn kept_leading_and_guide_ownership_survive_reopen_and_recalculation() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"pages": 3})).unwrap();
        s.execute("guide.add", &json!({"spread": 1, "page": 1, "orientation": "horizontal", "position": 11.25})).unwrap();
        let mut p = request();
        p["spread"] = json!(1);
        p["page"] = json!(1);
        p["fit"] = json!(false);
        let result = s.execute(ID, &p).unwrap();
        assert_eq!(result["effectiveLeading"], p["leading"]);
        let doc = &s.doc().unwrap().doc;
        assert!(doc.spreads[1].pages[0].guides.is_empty());
        let pg = &doc.spreads[1].pages[1];
        let last = pg.guides.last().unwrap();
        let remainder = result["remainder"].as_f64().unwrap();
        assert!(remainder > 0.0 && (pg.margin_rect().y1 - last.position - remainder).abs() < 1e-9);
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("grid.designcraft");
        s.execute("file.saveAs", &json!({"path": path})).unwrap();
        let mut reopened = Session::new();
        reopened.execute("file.open", &json!({"path": path})).unwrap();
        let before = reopened.doc().unwrap().doc.clone();
        assert_eq!(before.spreads[1].pages[1].modular_grid, Some(serde_json::from_value(p.clone()).unwrap()));
        reopened.execute(ID, &p).unwrap();
        assert!(std::sync::Arc::ptr_eq(&before, &reopened.doc().unwrap().doc), "same settings leave the file clean");

        // Drag one generated row edge to turn it into an independent ruler guide.
        reopened.execute("guide.move", &json!({"spread": 1, "page": 1, "index": 1, "position": 88.5})).unwrap();
        p["rows"] = json!(2);
        p["gutterLines"] = json!(0);
        reopened.execute(ID, &p).unwrap();
        let guides = &reopened.doc().unwrap().doc.spreads[1].pages[1].guides;
        assert_eq!(guides.iter().filter(|g| !g.modular_row).map(|g| g.position).collect::<Vec<_>>(), [11.25, 88.5]);
        assert_eq!(guides.iter().filter(|g| g.modular_row).count(), 2, "one internal boundary plus the bottom edge");
    }

    #[test]
    fn local_grid_overrides_its_parent_rows_and_columns_only_on_the_target_page() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"pages": 4, "facingPages": true})).unwrap();
        let mut parent = request();
        parent["spread"] = json!(SpreadRef::Parent(0));
        parent["page"] = json!(1);
        s.execute("guide.add", &json!({"spread": SpreadRef::Parent(0), "page": 1, "orientation": "horizontal", "position": 10})).unwrap();
        s.execute(ID, &parent).unwrap();
        let doc = &s.doc().unwrap().doc;
        assert!(doc.inherited_guides(1).is_empty(), "facing left page");
        assert!(doc.inherited_guides(2).iter().any(|g| g.modular_row), "facing right page");
        let mut local = parent.clone();
        local["spread"] = json!(1);
        local["rows"] = json!(5);
        s.execute(ID, &local).unwrap();
        parent["columns"] = json!(2);
        s.execute(ID, &parent).unwrap();
        let doc = &s.doc().unwrap().doc;
        assert_eq!(doc.page(0).unwrap().columns.count, parent["columns"].as_u64().unwrap() as u32);
        assert_eq!(doc.page(2).unwrap().columns.count, local["columns"].as_u64().unwrap() as u32);
        assert_eq!(doc.inherited_guides(2).iter().map(|g| g.position).collect::<Vec<_>>(), [10.0]);
        assert!(doc.page(2).unwrap().guides.iter().any(|g| g.modular_row));
    }

    #[test]
    fn rejected_geometry_and_bad_targets_leave_the_document_untouched() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let before = s.doc().unwrap().doc.clone();
        for (key, value) in [
            ("page", json!(99)),
            ("spread", json!(-1)),
            ("leading", json!(0.5)),
            ("rows", json!(0)),
            ("columnGutter", json!(5000)),
            ("gutterLines", json!(999)),
        ] {
            let mut p = request();
            p[key] = value;
            assert!(s.execute(ID, &p).is_err(), "accepted {p}");
            assert!(std::sync::Arc::ptr_eq(&before, &s.doc().unwrap().doc));
        }
    }
}
