//! Several demos open in the 3D player at once, one tab each. Switching tabs is
//! instant because each demo stays loaded; the one you leave pauses. "Analysis"
//! goes back to the main window and keeps the tabs for when you come back.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, RichText};
use eframe::glow;

use super::view::{DemoPlayer, OpenRequest, PlayerRequest};

/// Each open demo keeps its map and match in memory, so there's a limit. Opening one
/// more closes the tab that's been open longest (not the one you're on).
pub const MAX_TABS: usize = 6;
/// Longer demo names are shortened on their tab.
const TAB_NAME_CHARS: usize = 28;
const MUTED: Color32 = Color32::from_rgb(150, 150, 150);

#[derive(Default)]
pub struct PlayerTabs {
    tabs: Vec<DemoPlayer>,
    active: usize,
    /// Whether the player covers the window. False after "Analysis", with the tabs kept.
    visible: bool,
    /// Closed tabs whose map is still on the GPU, freed on the next frame that has the GL context.
    closing: Vec<DemoPlayer>,
    requests: Vec<PlayerRequest>,
}

enum TabAction {
    Switch(usize),
    Close(usize),
    Back,
    Pick,
}

impl PlayerTabs {
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Whether the player is on screen.
    pub fn is_visible(&self) -> bool {
        self.visible && !self.tabs.is_empty()
    }

    /// Brings the player back with the tabs as they were.
    pub fn show_again(&mut self) {
        if !self.tabs.is_empty() {
            self.visible = true;
            self.tabs[self.active].reactivate();
        }
    }

    /// The tab on screen.
    pub fn active_mut(&mut self) -> Option<&mut DemoPlayer> {
        self.tabs.get_mut(self.active)
    }

    /// The demos open in the player, in tab order, and which one is on screen.
    pub fn demos(&self) -> Vec<(PathBuf, bool)> {
        self.tabs.iter().enumerate().map(|(index, tab)| (tab.demo().to_path_buf(), index == self.active)).collect()
    }

    /// Whether this demo is open in a tab.
    pub fn is_open(&self, demo: &Path) -> bool {
        self.tabs.iter().any(|tab| same_file(tab.demo(), demo))
    }

    /// Gives a demo's tab the detections of a newer scan, without switching to it.
    pub fn set_markers(&mut self, demo: &Path, markers: Vec<super::view::Marker>) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| same_file(tab.demo(), demo)) {
            tab.retarget(OpenRequest { demo: demo.to_path_buf(), markers, ..OpenRequest::default() });
        }
    }

    /// Opens a demo in a new tab and shows it. A demo that's already open gets its tab
    /// back, moved to the tick and player asked for, instead of loading it twice.
    pub fn open(&mut self, request: OpenRequest) {
        self.pause_active();
        if let Some(index) = self.tabs.iter().position(|tab| same_file(tab.demo(), &request.demo)) {
            self.tabs[index].retarget(request);
            self.active = index;
        } else {
            if self.tabs.len() >= MAX_TABS {
                // The oldest tab that isn't the one on screen.
                let oldest = if self.active == 0 { 1 } else { 0 };
                self.close(oldest, None);
            }
            self.tabs.push(DemoPlayer::open(request));
            self.active = self.tabs.len() - 1;
        }
        self.visible = true;
        self.tabs[self.active].reactivate();
    }

    /// Requests for the rest of the tool, such as marking someone or picking a demo to open.
    pub fn take_requests(&mut self) -> Vec<PlayerRequest> {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            self.requests.extend(tab.take_requests());
        }
        std::mem::take(&mut self.requests)
    }

    /// Shows the tab bar and the demo on screen. Returns whether the player still covers
    /// the window; false once every tab is closed or after "Analysis".
    pub fn show(&mut self, ctx: &egui::Context, gl: Option<&glow::Context>) -> bool {
        if let Some(gl) = gl {
            for mut tab in self.closing.drain(..) {
                tab.destroy(gl);
            }
        }
        if !self.is_visible() {
            return false;
        }
        self.active = self.active.min(self.tabs.len() - 1);

        let mut action = None;
        egui::TopBottomPanel::top("player_tabs").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                if ui
                    .button("← Analysis")
                    .on_hover_text("Back to the main window, with this demo's results. The demos stay open here.")
                    .clicked()
                {
                    action = Some(TabAction::Back);
                }
                ui.separator();
                for (index, tab) in self.tabs.iter().enumerate() {
                    let (name, about) = tab_label(tab);
                    let selected = index == self.active;
                    let text = if selected { RichText::new(name).strong() } else { RichText::new(name) };
                    let response = ui.selectable_label(selected, text).on_hover_text(about);
                    if response.clicked() && !selected {
                        action = Some(TabAction::Switch(index));
                    }
                    if response.middle_clicked() {
                        action = Some(TabAction::Close(index));
                    }
                    let close = ui.add(egui::Button::new(RichText::new("×").color(MUTED)).frame(false).small());
                    if close.on_hover_text("Close this demo").clicked() {
                        action = Some(TabAction::Close(index));
                    }
                    ui.add_space(6.0);
                }
                let open = ui.button("+ Open demo…").on_hover_text(format!(
                    "Opens another demo in a new tab, without its detections. To see detections, analyse it first and \
                     press Watch demo. Up to {MAX_TABS} demos stay open; you can also drop .dem files here."
                ));
                if open.clicked() {
                    action = Some(TabAction::Pick);
                }
                if self.tabs.len() > 1 {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new("Ctrl+Tab next demo").small().color(MUTED));
                    });
                }
            });
            ui.add_space(2.0);
        });

        // Ctrl+Tab and Ctrl+Shift+Tab move between demos.
        if self.tabs.len() > 1 && action.is_none() {
            let step = ctx.input_mut(|input| {
                if input.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::Tab) {
                    Some(self.tabs.len() - 1)
                } else if input.consume_key(egui::Modifiers::CTRL, egui::Key::Tab) {
                    Some(1)
                } else {
                    None
                }
            });
            if let Some(step) = step {
                action = Some(TabAction::Switch((self.active + step) % self.tabs.len()));
            }
        }

        // Demos dropped on the player open in new tabs.
        let dropped: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .filter(|path| path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("dem")))
                .collect()
        });

        let keep_open = self.tabs[self.active].show(ctx);
        self.requests.extend(self.tabs[self.active].take_requests());
        if !keep_open {
            self.close(self.active, gl);
        }

        match action {
            Some(TabAction::Switch(index)) if index < self.tabs.len() => {
                self.pause_active();
                self.active = index;
                self.tabs[index].reactivate();
            }
            Some(TabAction::Close(index)) if index < self.tabs.len() => self.close(index, gl),
            Some(TabAction::Back) => {
                self.pause_active();
                self.visible = false;
                if let Some(tab) = self.tabs.get(self.active) {
                    self.requests.push(PlayerRequest::ShowAnalysis(tab.demo().to_path_buf()));
                }
            }
            Some(TabAction::Pick) => self.requests.push(PlayerRequest::PickDemo),
            _ => {}
        }
        for demo in dropped {
            self.open(OpenRequest { demo, ..OpenRequest::default() });
        }
        self.is_visible()
    }

    /// Frees every tab's GPU data. Call when the app closes.
    pub fn destroy_all(&mut self, gl: &glow::Context) {
        for mut tab in self.tabs.drain(..).chain(self.closing.drain(..)) {
            tab.destroy(gl);
        }
    }

    fn pause_active(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.pause();
        }
    }

    fn close(&mut self, index: usize, gl: Option<&glow::Context>) {
        let mut tab = self.tabs.remove(index);
        tab.pause();
        match gl {
            Some(gl) => tab.destroy(gl),
            None => self.closing.push(tab),
        }
        if index < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.reactivate();
        }
    }
}

/// A tab's name, and the text shown when hovering it.
fn tab_label(tab: &DemoPlayer) -> (String, String) {
    let path = tab.demo();
    let stem = path.file_stem().map_or_else(|| path.display().to_string(), |stem| stem.to_string_lossy().into_owned());
    let mut name = shorten(&stem, TAB_NAME_CHARS);
    if let Some(share) = tab.loading_share() {
        name = format!("{name} ({:.0}%)", share * 100.0);
    } else if tab.failure().is_some() {
        name = format!("⚠ {name}");
    }
    let about = match tab.map_name() {
        Some(map) => format!("{}\n{map}", path.display()),
        None => path.display().to_string(),
    };
    (name, about)
}

fn shorten(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_names_are_shortened() {
        assert_eq!(shorten("short", 28), "short");
        assert_eq!(shorten("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn the_same_demo_by_another_path_is_the_same_file() {
        let here = std::env::current_dir().unwrap().join("Cargo.toml");
        let dotted = std::env::current_dir().unwrap().join(".").join("Cargo.toml");
        assert!(same_file(&here, &dotted));
        assert!(!same_file(&here, Path::new("missing.dem")));
    }
}
