//! Baseline-aligned rows and columns within the page margins. Row heights and vertical
//! gutters contain whole baseline intervals, so every row starts on the same rhythm.
//! The fitted and fixed-leading workflows follow the public
//! [Grid Calculator overview](https://designersbookshop.com/products/grid-calculator-pe/).

use serde::{Deserialize, Serialize};

use crate::{MAX_COLUMNS, Page};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GridRequest {
    pub columns: u32,
    pub rows: u32,
    pub column_gutter: f64,
    /// Row gutter in baseline intervals.
    pub gutter_lines: u32,
    /// Desired baseline increment in points.
    pub leading: f64,
    /// Adjust leading to end at the bottom margin, or keep it and leave space below.
    pub fit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GridFit {
    pub effective_leading: f64,
    pub lines_per_row: u32,
    pub module_width: f64,
    pub module_height: f64,
    pub row_gutter: f64,
    pub remainder: f64,
}

impl GridRequest {
    pub fn calculate(&self, page: &Page) -> Result<GridFit, &'static str> {
        // The canvas displays at most one baseline per point. Match its minimum increment.
        if !(1.0..=15552.0).contains(&self.leading) {
            return Err("Enter a leading between 1 and 15552 pt.");
        }
        if !(1..=MAX_COLUMNS).contains(&self.columns) || !(1..=1000).contains(&self.rows) {
            return Err("Use 1 to 216 columns and 1 to 1000 rows.");
        }
        if !(0.0..=15552.0).contains(&self.column_gutter) || self.gutter_lines > 1000 {
            return Err("Use a column gutter from 0 to 15552 pt and a row gutter from 0 to 1000 intervals.");
        }
        let area = page.margin_rect();
        let height = area.height();
        let width = area.width();
        if ![width, height].iter().all(|v| v.is_finite() && *v > 0.0 && *v <= 15552.0) {
            return Err("The page needs a positive area inside its margins, no larger than 216 inches per side.");
        }
        let cols = f64::from(self.columns);
        let module_width = (width - self.column_gutter * (cols - 1.0)) / cols;
        if module_width <= 0.0 {
            return Err("Reduce the column count or gutter to leave room for the columns.");
        }

        let rows = f64::from(self.rows);
        let gaps = (rows - 1.0) * f64::from(self.gutter_lines);
        let ideal = (height / self.leading - gaps) / rows;
        let total = |lines: f64| rows * lines + gaps;
        let (lines, effective_leading) = if self.fit {
            // The closest increment lies on either side of the ideal interval count.
            // Compare increments themselves: rounding the count can choose the worse fit.
            let max_lines = ((height - gaps) / rows).floor();
            if max_lines < 1.0 {
                return Err("Reduce the rows or row gutter to fit at least one baseline interval per row.");
            }
            let low = ideal.floor().clamp(1.0, max_lines);
            let high = ideal.ceil().clamp(1.0, max_lines);
            let increment = |lines| height / total(lines);
            let best = if (increment(high) - self.leading).abs() < (increment(low) - self.leading).abs() { high } else { low };
            (best, increment(best))
        } else {
            // A decimal leading may divide exactly on paper but land just below an integer
            // in binary. Tolerate only arithmetic noise when deciding whether a line fits.
            let lines = (ideal + 8.0 * f64::EPSILON * ideal.abs().max(1.0)).floor();
            if lines < 1.0 {
                return Err("Reduce the leading, rows or row gutter to fit a whole interval in every row.");
            }
            (lines, self.leading)
        };
        let remaining = (height - total(lines) * effective_leading).max(0.0);
        Ok(GridFit {
            effective_leading,
            lines_per_row: lines as u32,
            module_width,
            module_height: lines * effective_leading,
            row_gutter: f64::from(self.gutter_lines) * effective_leading,
            remainder: if self.fit || remaining <= height * 8.0 * f64::EPSILON { 0.0 } else { remaining },
        })
    }
}
