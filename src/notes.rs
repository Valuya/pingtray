//! Notes: what the graph cannot know ("crowded train", "Wi-Fi on board").
//! A small window to add one now, reuse a frequent one, or delete recent ones.

use std::path::PathBuf;

use eframe::egui::{self, RichText};

use crate::store::{Note, Store};

pub struct NoteApp {
    store: Store,
    text: String,
    frequent: Vec<String>,
    recent: Vec<Note>,
    message: Option<String>,
    focus: bool,
    snapshot: Option<(PathBuf, u32)>,
}

impl NoteApp {
    fn new(store: Store) -> NoteApp {
        let mut app = NoteApp {
            store,
            text: String::new(),
            frequent: Vec::new(),
            recent: Vec::new(),
            message: None,
            focus: true,
            snapshot: std::env::var_os("PINGTRAY_SNAPSHOT").map(|p| (PathBuf::from(p), 0)),
        };
        app.reload();
        app
    }

    fn reload(&mut self) {
        self.frequent = self.store.frequent_notes(8).unwrap_or_default();
        self.recent = self.store.recent_notes(8).unwrap_or_default();
    }

    fn add(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        match self.store.add_note(crate::pinger::now_ms(), text) {
            Ok(_) => {
                self.message = Some(format!("Noted: {text}"));
                self.text.clear();
                self.reload();
            }
            Err(e) => self.message = Some(format!("Could not save: {e}")),
        }
    }
}

fn when(ts_ms: i64) -> String {
    chrono::TimeZone::timestamp_millis_opt(&chrono::Local, ts_ms)
        .single()
        .map(|d| d.format("%a %d/%m %H:%M").to_string())
        .unwrap_or_default()
}

impl eframe::App for NoteApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        crate::viewer::snapshot_step(ui, &mut self.snapshot, true);
        egui::CentralPanel::default().show(ui, |ui| {
            ui.label(RichText::new("Note for now").strong());
            ui.horizontal(|ui| {
                let edit = ui.add_sized(
                    [320.0, 24.0],
                    egui::TextEdit::singleline(&mut self.text).hint_text("crowded, quiet carriage, Wi-Fi on board…"),
                );
                if self.focus {
                    edit.request_focus();
                    self.focus = false;
                }
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Add").clicked() || enter {
                    let text = self.text.clone();
                    self.add(&text);
                }
            });
            if !self.frequent.is_empty() {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.weak("Again:");
                    for t in self.frequent.clone() {
                        if ui.button(&t).clicked() {
                            self.add(&t);
                        }
                    }
                });
            }
            if let Some(m) = &self.message {
                ui.add_space(4.0);
                ui.label(RichText::new(m).color(egui::Color32::from_rgb(0x2e, 0xc2, 0x7e)));
            }
            if !self.recent.is_empty() {
                ui.add_space(10.0);
                ui.separator();
                ui.label(RichText::new("Recent notes").strong());
                let mut delete = None;
                egui::Grid::new("recent").num_columns(3).spacing([12.0, 4.0]).show(ui, |ui| {
                    for n in &self.recent {
                        ui.weak(when(n.ts_ms));
                        ui.label(&n.text);
                        if ui.small_button("Delete").clicked() {
                            delete = Some(n.id);
                        }
                        ui.end_row();
                    }
                });
                if let Some(id) = delete {
                    let _ = self.store.delete_note(id);
                    self.reload();
                }
            }
        });
    }
}

/// `pingtray note TEXT…` adds directly; without text, opens the window.
pub fn run(words: &[String]) -> anyhow::Result<()> {
    let store = Store::open(&crate::config::db_path())?;
    let text = words.join(" ");
    if !text.trim().is_empty() {
        store.add_note(crate::pinger::now_ms(), text.trim())?;
        println!("noted: {}", text.trim());
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("pingtray note")
            .with_app_id("pingtray-note")
            .with_inner_size([460.0, 360.0]),
        ..Default::default()
    };
    eframe::run_native("pingtray note", options, Box::new(move |_| Ok(Box::new(NoteApp::new(store)))))
        .map_err(|e| anyhow::anyhow!("{e}"))
}
