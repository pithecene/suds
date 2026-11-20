//! The storage map: each project and each checkout as a rectangle whose area is its size.

use crate::project::Project;
use crate::workstation::Checkout;

/// A rectangle of the map, in the units of the map's width and height.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn area(&self) -> f32 {
        self.width * self.height
    }

    /// The part of the rectangle at the end of its longer side that holds `share` of its area.
    fn tail(&self, share: f32) -> Rect {
        let share = share.clamp(0.0, 1.0);
        if self.width >= self.height {
            let width = self.width * share;
            Rect {
                x: self.x + self.width - width,
                width,
                ..*self
            }
        } else {
            let height = self.height * share;
            Rect {
                y: self.y + self.height - height,
                height,
                ..*self
            }
        }
    }
}

/// The worst ratio of the sides among the rectangles of one strip. The strip
/// holds `sum` of area along a side of `side`, and `largest` and `smallest`
/// are the areas of its extremes.
fn worst(sum: f64, largest: f64, smallest: f64, side: f64) -> f64 {
    let square = side * side;
    (square * largest / (sum * sum)).max(sum * sum / (square * smallest))
}

/// One rectangle inside `bounds` for each weight, in the order of the
/// weights, whose area is the weight's share of the bounds. The layout keeps
/// each rectangle near a square, as Bruls, Huizing, and van Wijk describe. A
/// weight of zero takes an empty rectangle.
pub fn squarify(weights: &[u64], bounds: Rect) -> Vec<Rect> {
    let empty = Rect {
        x: bounds.x,
        y: bounds.y,
        ..Rect::default()
    };
    let mut rects = vec![empty; weights.len()];
    let total: f64 = weights.iter().map(|weight| *weight as f64).sum();
    if total <= 0.0 || bounds.width <= 0.0 || bounds.height <= 0.0 {
        return rects;
    }
    let mut order: Vec<usize> = (0..weights.len()).filter(|i| weights[*i] > 0).collect();
    order.sort_by(|a, b| weights[*b].cmp(&weights[*a]).then_with(|| a.cmp(b)));
    let scale = f64::from(bounds.width) * f64::from(bounds.height) / total;
    let area = |position: usize| weights[order[position]] as f64 * scale;
    let (mut x, mut y) = (f64::from(bounds.x), f64::from(bounds.y));
    let (mut width, mut height) = (f64::from(bounds.width), f64::from(bounds.height));
    let mut start = 0;
    while start < order.len() {
        let side = width.min(height);
        let mut sum = area(start);
        let mut ratio = worst(sum, area(start), area(start), side);
        let mut end = start + 1;
        while end < order.len() {
            let next = worst(sum + area(end), area(start), area(end), side);
            if next > ratio {
                break;
            }
            sum += area(end);
            ratio = next;
            end += 1;
        }
        // The strip lies along the shorter side and takes its thickness from the longer one.
        let thickness = sum / side;
        let mut offset = 0.0;
        for position in start..end {
            let length = area(position) / thickness;
            rects[order[position]] = if width >= height {
                Rect {
                    x: x as f32,
                    y: (y + offset) as f32,
                    width: thickness as f32,
                    height: length as f32,
                }
            } else {
                Rect {
                    x: (x + offset) as f32,
                    y: y as f32,
                    width: length as f32,
                    height: thickness as f32,
                }
            };
            offset += length;
        }
        if width >= height {
            x += thickness;
            width -= thickness;
        } else {
            y += thickness;
            height -= thickness;
        }
        start = end;
    }
    rects
}

/// One project's frame on the map.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// The project's position among the projects.
    pub project: usize,
    pub rect: Rect,
    /// The part of the frame that shows the project's build state. Only a frame without tiles draws it.
    pub build: Rect,
    /// Whether the frame has room for the project's name above its tiles.
    pub labelled: bool,
    /// Whether the frame has room for a tile of each checkout.
    pub detailed: bool,
}

/// One checkout's tile on the map.
#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    /// The checkout's position in the view's checkouts.
    pub checkout: usize,
    /// The position of the checkout's project among the projects.
    pub project: usize,
    pub rect: Rect,
    /// The part of the tile that shows the checkout's build state.
    pub build: Rect,
}

/// The map of some projects.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Map {
    pub frames: Vec<Frame>,
    pub tiles: Vec<Tile>,
}

/// The height of a project's name above its tiles.
pub const LABEL: f32 = 16.0;
/// The shortest side of a frame that shows a tile of each checkout.
const DETAILED: f32 = 28.0;
/// The smallest frame that shows its project's name.
const LABELLED: (f32, f32) = (64.0, 44.0);

fn build_share(own: u64, build: u64) -> f32 {
    let total = own.saturating_add(build);
    if total == 0 {
        0.0
    } else {
        (build as f64 / total as f64) as f32
    }
}

/// The map of `projects` in a space of `width` by `height`. A frame's area is
/// its project's size, and a tile's area is its checkout's size. A frame too
/// small for tiles stands for its whole project.
pub fn map(projects: &[Project], checkouts: &[Checkout], width: f32, height: f32) -> Map {
    let bounds = Rect {
        width,
        height,
        ..Rect::default()
    };
    let weights: Vec<u64> = projects.iter().map(Project::total_bytes).collect();
    let mut map = Map::default();
    for (position, rect) in squarify(&weights, bounds).into_iter().enumerate() {
        let project = &projects[position];
        if rect.area() <= 0.0 {
            continue;
        }
        let detailed = rect.width >= DETAILED && rect.height >= DETAILED;
        let labelled = rect.width >= LABELLED.0 && rect.height >= LABELLED.1;
        map.frames.push(Frame {
            project: position,
            rect,
            build: rect.tail(build_share(project.own_bytes, project.build_bytes)),
            labelled,
            detailed,
        });
        if !detailed {
            continue;
        }
        let top = if labelled { LABEL } else { 0.0 };
        let inner = Rect {
            y: rect.y + top,
            height: rect.height - top,
            ..rect
        };
        let sizes: Vec<u64> = project
            .members
            .iter()
            .map(|member| checkouts[*member].total_bytes())
            .collect();
        for (member, tile) in project.members.iter().zip(squarify(&sizes, inner)) {
            let checkout = &checkouts[*member];
            if tile.area() <= 0.0 {
                continue;
            }
            map.tiles.push(Tile {
                checkout: *member,
                project: position,
                rect: tile,
                build: tile.tail(build_share(checkout.own_bytes, checkout.build_bytes)),
            });
        }
    }
    map
}
