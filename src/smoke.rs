//! SmokePing's "smoke" as a true gradient: for each time slot, a column whose
//! opacity follows the distribution of reply times (densest at the median,
//! fading to nothing at the extremes).

use std::ops::RangeInclusive;

use eframe::egui::{Color32, Mesh, Pos2, Shape, Ui};
use egui_plot::{PlotBounds, PlotGeometry, PlotItem, PlotItemBase, PlotPoint, PlotTransform};

use crate::store::PCTS;

/// One column of smoke: its centre (plot x) and the plot y of each percentile.
pub type Column = (f64, [f64; PCTS.len()]);

pub struct Smoke {
    base: PlotItemBase,
    /// Runs of contiguous columns; gaps in the data split them.
    runs: Vec<Vec<Column>>,
    half_width: f64,
    color: Color32,
    max_alpha: f32,
}

impl Smoke {
    pub fn new(name: &str, runs: Vec<Vec<Column>>, bucket_width: f64, color: Color32, max_alpha: f32) -> Smoke {
        let base = PlotItemBase::new(name.to_string());
        Smoke { base, runs, half_width: bucket_width / 2.0, color, max_alpha }
    }
}

/// Opacity at percentile `q` (0..=1): highest at the median, zero at min and max.
fn alpha_at(q: f64, max_alpha: f32) -> f32 {
    let centre = 1.0 - (2.0 * q - 1.0).abs();
    max_alpha * centre.powf(0.8) as f32
}

impl PlotItem for Smoke {
    fn shapes(&self, _ui: &Ui, transform: &PlotTransform, shapes: &mut Vec<Shape>) {
        let colors: Vec<Color32> = PCTS.iter().map(|&q| self.color.gamma_multiply(alpha_at(q, self.max_alpha))).collect();
        let mut mesh = Mesh::default();
        let pos = |x: f64, y: f64| -> Pos2 { transform.position_from_point(&PlotPoint::new(x, y)) };
        let n = PCTS.len() as u32;
        // SmokePing style: each point is an interval, its own column with a
        // vertical gradient; neighbours are not blended
        for (x, ys) in self.runs.iter().flatten() {
            let base = mesh.vertices.len() as u32;
            for edge in [x - self.half_width, x + self.half_width] {
                for (k, y) in ys.iter().enumerate() {
                    mesh.colored_vertex(pos(edge, *y), colors[k]);
                }
            }
            for k in 0..n - 1 {
                let (a, b) = (base + k, base + k + 1);
                let (d, e) = (base + n + k, base + n + k + 1);
                mesh.add_triangle(a, b, e);
                mesh.add_triangle(a, e, d);
            }
        }
        shapes.push(Shape::mesh(mesh));
    }

    fn initialize(&mut self, _x_range: RangeInclusive<f64>) {}

    fn allow_hover(&self) -> bool {
        false
    }

    fn color(&self) -> Color32 {
        self.color
    }

    fn geometry(&self) -> PlotGeometry<'_> {
        PlotGeometry::None
    }

    fn bounds(&self) -> PlotBounds {
        let mut b = PlotBounds::NOTHING;
        for (x, ys) in self.runs.iter().flatten() {
            b.extend_with(&PlotPoint::new(*x - self.half_width, ys[0]));
            b.extend_with(&PlotPoint::new(*x + self.half_width, ys[ys.len() - 1]));
        }
        b
    }

    fn base(&self) -> &PlotItemBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PlotItemBase {
        &mut self.base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn densest_at_the_median_and_fading_out() {
        assert_eq!(alpha_at(0.0, 0.5), 0.0);
        assert_eq!(alpha_at(1.0, 0.5), 0.0);
        assert_eq!(alpha_at(0.5, 0.5), 0.5);
        assert!(alpha_at(0.25, 0.5) < alpha_at(0.4, 0.5));
    }
}
