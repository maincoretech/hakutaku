use eframe::egui::{self, Color32, Rect, Response, Ui};

#[derive(Clone, Copy)]
pub enum Icon {
    Folder,
    Key,
    Scan,
    Package,
    Shield,
    Clock,
    Arrow,
}

pub fn show(ui: &mut Ui, icon: Icon, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
    paint(ui, rect, icon, color);
}

pub fn button(ui: &mut Ui, icon: Icon, label: &str, width: f32) -> Response {
    let id = ui.make_persistent_id(("action-icon", label));
    let button = egui::Button::new((
        egui::Atom::custom(id, egui::Vec2::splat(18.0)),
        egui::RichText::new(label).size(14.0),
    ))
    .gap(6.0)
    .min_size(egui::vec2(width, 30.0))
    .atom_ui(ui);
    if let Some(rect) = button.rect(id) {
        paint(
            ui,
            rect,
            icon,
            ui.style().interact(&button.response).fg_stroke.color,
        );
    }
    button.response
}

fn paint(ui: &Ui, rect: Rect, icon: Icon, color: Color32) {
    let p = |x: f32, y: f32| rect.min + rect.size() * egui::vec2(x / 24.0, y / 24.0);
    let stroke = egui::Stroke::new(1.5, color);
    let line = |points: &[(f32, f32)]| {
        ui.painter().add(egui::Shape::line(
            points.iter().map(|&(x, y)| p(x, y)).collect(),
            stroke,
        ));
    };
    match icon {
        Icon::Folder => {
            line(&[
                (3., 20.),
                (3., 5.),
                (9., 5.),
                (12., 8.),
                (21., 8.),
                (21., 20.),
                (3., 20.),
            ]);
            line(&[(3., 10.), (21., 10.)]);
        }
        Icon::Key => {
            ui.painter()
                .circle_stroke(p(8., 8.), rect.width() * 4.0 / 24.0, stroke);
            line(&[(11., 11.), (21., 21.), (23., 19.)]);
            line(&[(17., 17.), (19., 15.)]);
        }
        Icon::Scan => {
            ui.painter()
                .circle_stroke(p(10., 10.), rect.width() * 6.0 / 24.0, stroke);
            line(&[(15., 15.), (22., 22.)]);
        }
        Icon::Package => {
            line(&[
                (3., 7.),
                (12., 3.),
                (21., 7.),
                (21., 17.),
                (12., 21.),
                (3., 17.),
                (3., 7.),
                (12., 11.),
                (21., 7.),
            ]);
            line(&[(12., 11.), (12., 21.)]);
            line(&[(8., 5.), (17., 9.), (17., 13.)]);
        }
        Icon::Shield => {
            line(&[
                (12., 2.),
                (21., 6.),
                (20., 14.),
                (17., 19.),
                (12., 22.),
                (7., 19.),
                (4., 14.),
                (3., 6.),
                (12., 2.),
            ]);
            line(&[(7., 12.), (10., 15.), (17., 8.)]);
        }
        Icon::Clock => {
            ui.painter()
                .circle_stroke(p(12., 12.), rect.width() * 9.0 / 24.0, stroke);
            line(&[(12., 6.), (12., 12.), (16., 14.)]);
        }
        Icon::Arrow => {
            line(&[(3., 12.), (21., 12.)]);
            line(&[(15., 6.), (21., 12.), (15., 18.)]);
        }
    }
}
