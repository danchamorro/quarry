//! Small stroked toolbar icons drawn with the painter, so no image assets ship.
use eframe::egui::{self, Color32, Pos2, Rect, Stroke, pos2, vec2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Icon {
    Undo,
    Redo,
    Search,
    Filter,
    Columns,
    Plus,
    ChevronUp,
    ChevronDown,
    Close,
    Document,
}

pub(super) const ICON_SIZE: f32 = 14.0;

/// Paints `icon` centered in `rect` with a stroke that follows `color`.
pub(super) fn paint(painter: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let size = rect.width().min(rect.height());
    let rect = Rect::from_center_size(rect.center(), vec2(size, size));
    let stroke = Stroke::new((size / 9.0).clamp(1.2, 1.6), color);
    let at = |x: f32, y: f32| -> Pos2 { pos2(rect.left() + x * size, rect.top() + y * size) };
    match icon {
        Icon::Undo | Icon::Redo => {
            let mirror = |p: Pos2| {
                if icon == Icon::Redo {
                    pos2(rect.left() + rect.right() - p.x, p.y)
                } else {
                    p
                }
            };
            // A hook: horizontal stroke ending in a half circle, with an arrow head.
            let center = at(0.55, 0.55);
            let radius = 0.26 * size;
            let mut points = vec![at(0.2, 0.29)];
            for step in 0..=12 {
                let angle =
                    -std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * step as f32 / 12.0;
                points.push(center + radius * vec2(angle.cos(), angle.sin()));
            }
            points.push(at(0.3, 0.81));
            let points: Vec<_> = points.into_iter().map(mirror).collect();
            painter.add(egui::Shape::line(points, stroke));
            let head = [at(0.38, 0.11), at(0.18, 0.29), at(0.38, 0.47)].map(mirror);
            painter.add(egui::Shape::line(head.to_vec(), stroke));
        }
        Icon::Search => {
            painter.circle_stroke(at(0.43, 0.43), 0.27 * size, stroke);
            painter.line_segment([at(0.63, 0.63), at(0.88, 0.88)], stroke);
        }
        Icon::Filter => {
            let points = vec![
                at(0.1, 0.18),
                at(0.9, 0.18),
                at(0.58, 0.55),
                at(0.58, 0.86),
                at(0.42, 0.76),
                at(0.42, 0.55),
                at(0.1, 0.18),
            ];
            painter.add(egui::Shape::line(points, stroke));
        }
        Icon::Columns => {
            let body = Rect::from_min_max(at(0.1, 0.16), at(0.9, 0.84));
            painter.rect_stroke(body, 2.0, stroke, egui::StrokeKind::Middle);
            for x in [0.37, 0.63] {
                painter.line_segment([at(x, 0.16), at(x, 0.84)], stroke);
            }
        }
        Icon::Plus => {
            painter.line_segment([at(0.5, 0.15), at(0.5, 0.85)], stroke);
            painter.line_segment([at(0.15, 0.5), at(0.85, 0.5)], stroke);
        }
        Icon::ChevronUp => {
            painter.add(egui::Shape::line(
                vec![at(0.2, 0.65), at(0.5, 0.35), at(0.8, 0.65)],
                stroke,
            ));
        }
        Icon::ChevronDown => {
            painter.add(egui::Shape::line(
                vec![at(0.2, 0.37), at(0.5, 0.67), at(0.8, 0.37)],
                stroke,
            ));
        }
        Icon::Close => {
            painter.line_segment([at(0.25, 0.25), at(0.75, 0.75)], stroke);
            painter.line_segment([at(0.75, 0.25), at(0.25, 0.75)], stroke);
        }
        Icon::Document => {
            let points = vec![
                at(0.22, 0.08),
                at(0.6, 0.08),
                at(0.8, 0.28),
                at(0.8, 0.92),
                at(0.22, 0.92),
                at(0.22, 0.08),
            ];
            painter.add(egui::Shape::line(points, stroke));
            painter.add(egui::Shape::line(
                vec![at(0.6, 0.08), at(0.6, 0.28), at(0.8, 0.28)],
                stroke,
            ));
        }
    }
}

/// A borderless toolbar button with an icon and an optional visible label.
///
/// `label` is always the accessible name, so icon-only buttons stay discoverable.
pub(super) fn button(
    ui: &mut egui::Ui,
    enabled: bool,
    icon: Icon,
    label: &str,
    show_label: bool,
) -> egui::Response {
    let id = ui.id().with(("toolbar-icon", label));
    let atom = egui::Atom::custom(id, vec2(ICON_SIZE, ICON_SIZE));
    let button = if show_label {
        egui::Button::new((atom, label))
    } else {
        egui::Button::new(atom)
    };
    let rendered = ui.add_enabled_ui(enabled, |ui| button.min_size(vec2(26.0, 26.0)).atom_ui(ui));
    let rendered = rendered.inner;
    if let Some(rect) = rendered.rect(id) {
        let color = if enabled {
            ui.visuals().widgets.inactive.fg_stroke.color
        } else {
            ui.visuals()
                .widgets
                .inactive
                .fg_stroke
                .color
                .gamma_multiply(0.35)
        };
        paint(ui.painter(), rect, icon, color);
    }
    let response = rendered.response;
    if !show_label {
        response
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    }
    response
}
