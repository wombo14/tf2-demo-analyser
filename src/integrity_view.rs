//! The demo integrity badge next to the demo's name, and its details window.
//! The check (`util::integrity`) runs in the background when a demo is analysed.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};

use eframe::egui::{self, Color32, RichText};

use crate::util::integrity::{Report, Severity};

const GREEN: Color32 = Color32::from_rgb(110, 200, 120);
const AMBER: Color32 = Color32::from_rgb(235, 175, 70);
const RED: Color32 = Color32::from_rgb(235, 80, 70);
const MUTED: Color32 = Color32::from_rgb(150, 150, 150);

pub struct IntegrityView {
    /// The demo the shown result is for, and the result (`None` while checking).
    current: Option<(PathBuf, Option<Result<Report, String>>)>,
    send: Sender<(PathBuf, Result<Report, String>)>,
    results: Receiver<(PathBuf, Result<Report, String>)>,
    window_open: bool,
}

impl Default for IntegrityView {
    fn default() -> Self {
        Self::new()
    }
}

fn color(severity: Option<Severity>) -> Color32 {
    match severity {
        Some(Severity::Edited) => RED,
        Some(Severity::Warning) => AMBER,
        _ => GREEN,
    }
}

impl IntegrityView {
    pub fn new() -> Self {
        let (send, results) = channel();
        Self { current: None, send, results, window_open: false }
    }

    /// Checks a demo in the background. The result replaces the shown one when it's ready.
    pub fn request(&mut self, path: &std::path::Path, ctx: &egui::Context) {
        if self.current.as_ref().is_some_and(|(shown, result)| shown == path && result.is_some()) {
            return;
        }
        self.current = Some((path.to_path_buf(), None));
        let send = self.send.clone();
        let path = path.to_path_buf();
        let ctx = ctx.clone();
        let _ = std::thread::Builder::new().name("integrity".into()).spawn(move || {
            let result = std::fs::read(&path)
                .map_err(|error| format!("Couldn't read the demo: {error}"))
                .map(|bytes| crate::util::integrity::check(&bytes));
            let _ = send.send((path, result));
            ctx.request_repaint();
        });
    }

    fn poll(&mut self) {
        while let Ok((path, result)) = self.results.try_recv() {
            if let Some((shown, slot)) = &mut self.current {
                if *shown == path {
                    *slot = Some(result);
                }
            }
        }
    }

    /// The badge on the demo's name row. Clicking it opens the details.
    pub fn show(&mut self, ui: &mut egui::Ui, file: Option<&std::path::Path>) {
        self.poll();
        let Some((path, result)) = &self.current else { return };
        if file != Some(path.as_path()) {
            return;
        }
        let (text, color, hover) = match result {
            None => ("Checking integrity…".to_string(), MUTED, "Checking whether this demo looks edited or cut".to_string()),
            Some(Err(error)) => ("Integrity: couldn't check".to_string(), AMBER, error.clone()),
            Some(Ok(report)) => {
                let mark = match report.worst() {
                    Some(Severity::Edited) => "✗",
                    Some(Severity::Warning) => "⚠",
                    _ => "✓",
                };
                (
                    format!("{mark} Integrity: {}", report.verdict()),
                    color(report.worst()),
                    "Whether this demo looks edited or cut. Click for details.".to_string(),
                )
            }
        };
        let badge = ui.add(egui::Button::new(RichText::new(text).color(color)).frame(false)).on_hover_text(hover);
        if badge.clicked() && matches!(result, Some(Ok(_))) {
            self.window_open = true;
        }
    }

    /// The details window, when it's open.
    pub fn window(&mut self, ctx: &egui::Context) {
        if !self.window_open {
            return;
        }
        let Some((path, Some(Ok(report)))) = &self.current else {
            self.window_open = false;
            return;
        };
        let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let mut open = true;
        egui::Window::new("Demo integrity")
            .open(&mut open)
            .collapsible(false)
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(&name);
                    ui.label(RichText::new(report.verdict()).strong().color(color(report.worst())));
                });
                egui::Grid::new("integrity_facts").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
                    for (label, value) in &report.facts {
                        ui.label(RichText::new(label).color(MUTED));
                        ui.label(value);
                        ui.end_row();
                    }
                });
                ui.separator();
                for finding in &report.findings {
                    let (mark, color) = match finding.severity {
                        Severity::Edited => ("✗", RED),
                        Severity::Warning => ("⚠", AMBER),
                        Severity::Info => ("✓", GREEN),
                    };
                    ui.label(RichText::new(format!("{mark} {}", finding.title)).strong().color(color));
                    ui.add(egui::Label::new(&finding.detail).wrap());
                    ui.add_space(6.0);
                }
                ui.separator();
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "TF2 demos aren't signed, so no check can prove a demo is genuine. This one looks for what \
                             editing usually breaks: the header against the content, counters that only count up, and \
                             the server's own clock, which is inside every packet and keeps in step with the demo. \
                             Cutting part of a match out shows there even when everything else was fixed up.",
                        )
                        .small()
                        .color(MUTED),
                    )
                    .wrap(),
                );
            });
        self.window_open = open;
    }
}
