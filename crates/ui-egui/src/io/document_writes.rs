//! Document saves kept by a host ([`crate::Services::write_document`]): the web build in a page
//! that hosts it hands each save of a document to its own file to the page, and the document
//! counts as saved only once the page says the file is stored
//! ([`VectorcraftApp::confirm_document_write`]). Until then it stays modified, so a write the page
//! refused or never finished can't leave it looking saved.

use std::collections::VecDeque;

use serde_json::{Value, json};
use vectorcraft_engine::EngineError;
use vectorcraft_engine::cmd::fileio::{self, SaveJob, SaveMode};

use super::open::note_recent;
use super::save::{report_saved, report_write_error, write_to};
use crate::{DocumentWrite, VectorcraftApp};

/// Saves still waiting for the host, at most: a host that never answers can't make the list grow
/// without end (the oldest is forgotten, and its document stays modified).
pub const MAX_WAITING_SAVES: usize = 64;

/// A save handed to the host, waiting for its confirmation.
struct Waiting {
    /// [`DocumentWrite::save`].
    save: u64,
    /// The document ([`vectorcraft_engine::DocState::uid`]).
    doc: u64,
    /// The save, with the document as it was written: what counts as saved once it is confirmed.
    job: SaveJob,
}

/// The saves handed to [`crate::Services::write_document`] and not confirmed yet.
pub struct DocumentWrites {
    next: u64,
    waiting: VecDeque<Waiting>,
}

impl Default for DocumentWrites {
    fn default() -> Self {
        Self { next: 1, waiting: VecDeque::new() }
    }
}

impl DocumentWrites {
    /// The number of the next save.
    fn take_number(&mut self) -> u64 {
        let n = self.next;
        self.next = self.next.saturating_add(1);
        n
    }

    fn add(&mut self, save: u64, doc: u64, job: SaveJob) {
        if self.waiting.len() >= MAX_WAITING_SAVES {
            self.waiting.pop_front();
        }
        self.waiting.push_back(Waiting { save, doc, job });
    }

    /// Save `save`, once: `None` for one never handed over or already confirmed. The same
    /// document's earlier saves still waiting are forgotten: a late confirmation of one of them
    /// must not take the document back to an older file or state.
    fn confirm(&mut self, save: u64) -> Option<(u64, SaveJob)> {
        let at = self.waiting.iter().position(|w| w.save == save)?;
        let w = self.waiting.remove(at)?;
        self.waiting.retain(|o| o.doc != w.doc || o.save > w.save);
        Some((w.doc, w.job))
    }

    /// How many saves wait for the host.
    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    /// Whether no save waits for the host.
    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }
}

/// Whether a save in `mode` of a document whose own file is `own` to `path` is a Save As for the
/// host ([`DocumentWrite::save_as`]): asked for as Save As, or written anywhere but the document's
/// own file (a first save, another name, another format).
pub fn save_as(mode: SaveMode, own: Option<&str>, path: &str) -> bool {
    mode != SaveMode::Save || own != Some(path)
}

/// Whether `job` goes to the host: it writes the document's own file (Save, Save As, not a copy
/// or a template), to a path, and a host keeps the documents' files.
pub(super) fn goes_to_host(app: &VectorcraftApp, job: &SaveJob) -> bool {
    app.services.write_document.is_some() && job.plan().retargets() && job.plan().path.is_some()
}

/// Encode and write `job`, a save of document `doc`: the document's file goes to the host
/// ([`crate::Services::write_document`]), the other files it writes (separate artboards, linked
/// images) are downloaded. The document is not marked saved here but when the host confirms the
/// save ([`VectorcraftApp::confirm_document_write`]) → the save's result with `waiting: true`.
pub(super) fn write_to_host(app: &mut VectorcraftApp, job: SaveJob, doc: u64, save_as: bool) -> Result<Value, String> {
    let own = job.plan().path.clone().unwrap_or_default();
    let save = app.document_writes.take_number();
    let mut handed = false;
    let services = &mut app.services;
    let written = job
        .write(|p, bytes| {
            let r = match services.write_document.as_mut() {
                Some(write) if !handed && p == own => {
                    handed = true;
                    write(DocumentWrite { doc, save, path: p, bytes, save_as })
                }
                _ => write_to(services, p, bytes),
            };
            r.map_err(EngineError::Other)
        })
        .map_err(|e| e.to_string());
    let mut r = match written {
        Ok(r) => r,
        Err(e) => {
            report_write_error(app, false, &own, &e);
            return Err(e);
        }
    };
    if !handed {
        // Nothing went to the host (an encoder that named its file otherwise): written as before.
        job.complete(&mut app.session, doc);
        note_recent(app, &own);
        report_saved(app, &own, &r);
        return Ok(r);
    }
    app.document_writes.add(save, doc, job);
    app.status(format!("Saving {}", fileio::file_name(&own)));
    r["waiting"] = json!(true);
    Ok(r)
}

impl VectorcraftApp {
    /// The host stored the file of save `save` ([`DocumentWrite::save`]): the document takes the
    /// file and counts as saved as it was when it was written (edits made since keep it modified).
    /// A save already confirmed, superseded by a later one of the same document, or unknown is
    /// left alone; a document closed meanwhile is too. → whether a save was confirmed.
    pub fn confirm_document_write(&mut self, save: u64) -> bool {
        let Some((doc, job)) = self.document_writes.confirm(save) else { return false };
        let path = job.plan().path.clone().unwrap_or_default();
        let r = json!({ "path": path, "warnings": job.known_warnings() });
        job.complete(&mut self.session, doc);
        note_recent(self, &path);
        report_saved(self, &path, &r);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use vectorcraft_engine::Session;

    use super::*;
    use crate::Services;

    const SVG: &[u8] =
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="80"><rect x="10" y="10" width="30" height="20" fill="red"/></svg>"#;

    /// What the host was handed: (save, document, path, save_as, bytes).
    type Handed = Rc<RefCell<Vec<(u64, u64, String, bool, Vec<u8>)>>>;
    /// What was downloaded: (name, bytes).
    type Downloads = Rc<RefCell<Vec<(String, Vec<u8>)>>>;

    /// A browser app whose documents' files a host keeps (`hosted`), or a plain one.
    fn app(hosted: bool) -> (VectorcraftApp, Handed, Downloads) {
        let (handed, downloads) = (Handed::default(), Downloads::default());
        let (h, d) = (handed.clone(), downloads.clone());
        let services = Services {
            download: Some(Box::new(move |name: &str, b: &[u8]| {
                d.borrow_mut().push((name.to_string(), b.to_vec()));
                Ok(())
            })),
            write_document: hosted.then(|| {
                Box::new(move |w: DocumentWrite<'_>| {
                    h.borrow_mut().push((w.save, w.doc, w.path.to_string(), w.save_as, w.bytes.to_vec()));
                    Ok(())
                }) as crate::WriteDocumentFn
            }),
            hosted,
            ..Default::default()
        };
        (VectorcraftApp::new(Session::new(), services), handed, downloads)
    }

    /// `logo.svg` from the host, opened under its name, then changed.
    fn open_logo(app: &mut VectorcraftApp) {
        crate::io::open_bytes(app, "logo.svg", SVG, Some("logo.svg".into())).unwrap();
        edit(app);
    }

    fn edit(app: &mut VectorcraftApp) {
        app.run("shape.rectangle", json!({"x": 50, "y": 10, "width": 10, "height": 10})).unwrap();
    }

    fn dirty(app: &VectorcraftApp) -> bool {
        app.session.active().unwrap().is_dirty()
    }

    fn last_save(handed: &Handed) -> (u64, String, bool) {
        let h = handed.borrow();
        let (save, _, path, save_as, _) = h.last().unwrap();
        (*save, path.clone(), *save_as)
    }

    #[test]
    fn save_rewrites_the_document_s_own_file_and_is_saved_only_once_confirmed() {
        let (mut app, handed, downloads) = app(true);
        open_logo(&mut app);
        let r = crate::io::save(&mut app, SaveMode::Save, &json!({}), false).unwrap();
        assert_eq!(r["waiting"], true);
        let (save, path, save_as) = last_save(&handed);
        assert_eq!((path.as_str(), save_as), ("logo.svg", false));
        assert!(handed.borrow()[0].4.starts_with(b"<?xml") || handed.borrow()[0].4.starts_with(b"<svg"), "the SVG itself");
        assert!(downloads.borrow().is_empty(), "the document's file is not downloaded");
        assert!(dirty(&app), "not saved before the host says so");
        assert!(app.confirm_document_write(save));
        assert!(!dirty(&app));
        assert!(!app.confirm_document_write(save), "confirmed once");
        assert_eq!(app.session.active().unwrap().path.as_deref(), Some("logo.svg"));
    }

    #[test]
    fn a_first_save_save_as_and_a_format_change_are_saves_as() {
        let (mut app, handed, _) = app(true);
        app.run("file.new", json!({"width": 100, "height": 80})).unwrap();
        edit(&mut app);
        crate::io::save(&mut app, SaveMode::Save, &json!({"path": "Untitled-1.vectorcraft"}), false).unwrap();
        assert_eq!(last_save(&handed).1, "Untitled-1.vectorcraft");
        assert!(last_save(&handed).2, "a first save");

        let (mut app, handed, _) = app_with_logo();
        crate::io::save(&mut app, SaveMode::SaveAs, &json!({"path": "logo.svg", "svg": {}}), false).unwrap();
        assert_eq!(last_save(&handed).1, "logo.svg");
        assert!(last_save(&handed).2, "Save As under the same name");

        let (mut app, handed, _) = app_with_logo();
        crate::io::save(&mut app, SaveMode::SaveAs, &json!({"path": "logo.vectorcraft", "options": {}}), false).unwrap();
        let (save, path, save_as) = last_save(&handed);
        assert_eq!((path.as_str(), save_as), ("logo.vectorcraft", true), "another format");
        assert!(app.confirm_document_write(save));
        let st = app.session.active().unwrap();
        assert_eq!((st.path.as_deref(), st.format, st.is_dirty()), (Some("logo.vectorcraft"), "vectorcraft", false));
    }

    fn app_with_logo() -> (VectorcraftApp, Handed, Downloads) {
        let (mut app, handed, downloads) = app(true);
        open_logo(&mut app);
        (app, handed, downloads)
    }

    #[test]
    fn an_edit_after_the_save_keeps_the_document_modified() {
        let (mut app, handed, _) = app_with_logo();
        crate::io::save(&mut app, SaveMode::Save, &json!({}), false).unwrap();
        edit(&mut app);
        assert!(app.confirm_document_write(last_save(&handed).0));
        assert!(dirty(&app), "the edit made since was not in the file");
    }

    #[test]
    fn a_late_confirmation_of_an_older_save_changes_nothing() {
        let (mut app, handed, _) = app_with_logo();
        crate::io::save(&mut app, SaveMode::Save, &json!({}), false).unwrap();
        let first = last_save(&handed).0;
        crate::io::save(&mut app, SaveMode::SaveAs, &json!({"path": "poster.svg", "svg": {}}), false).unwrap();
        let second = last_save(&handed).0;
        assert!(app.confirm_document_write(second));
        assert!(!app.confirm_document_write(first), "superseded by the later save");
        assert_eq!(app.session.active().unwrap().path.as_deref(), Some("poster.svg"));
    }

    #[test]
    fn copies_templates_and_exports_still_download() {
        let (mut app, handed, downloads) = app_with_logo();
        crate::io::save(&mut app, SaveMode::Copy, &json!({"path": "logo copy.svg", "svg": {}}), false).unwrap();
        crate::io::save(&mut app, SaveMode::Template, &json!({"path": "logo.vctemplate"}), false).unwrap();
        crate::io::export(&mut app, Some("png"), Some("logo.png".into()), &json!({})).unwrap();
        assert!(handed.borrow().is_empty(), "nothing went to the host");
        let names: Vec<String> = downloads.borrow().iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names, ["logo copy.svg", "logo.vctemplate", "logo.png"]);
        assert!(dirty(&app), "a copy doesn't save the document");
    }

    #[test]
    fn without_a_host_a_save_downloads_and_is_saved_at_once() {
        let (mut app, _, downloads) = app(false);
        open_logo(&mut app);
        let r = crate::io::save(&mut app, SaveMode::Save, &json!({}), false).unwrap();
        assert!(r.get("waiting").is_none());
        assert_eq!(downloads.borrow().len(), 1);
        assert!(!dirty(&app));
    }

    #[test]
    fn a_refused_hand_over_fails_the_save_and_leaves_the_document_modified() {
        let services = Services {
            download: Some(Box::new(|_: &str, _: &[u8]| Ok(()))),
            write_document: Some(Box::new(|_: DocumentWrite<'_>| Err("the page is gone".to_string()))),
            hosted: true,
            ..Default::default()
        };
        let mut app = VectorcraftApp::new(Session::new(), services);
        open_logo(&mut app);
        app.ui.answering_control = true;
        let e = crate::io::save(&mut app, SaveMode::Save, &json!({}), false).unwrap_err();
        assert!(e.contains("the page is gone"), "{e}");
        assert!(dirty(&app));
        assert!(app.document_writes.is_empty());
    }

    #[test]
    fn a_host_that_never_answers_cannot_grow_the_list() {
        let (mut app, handed, _) = app_with_logo();
        for _ in 0..=MAX_WAITING_SAVES {
            crate::io::save(&mut app, SaveMode::Save, &json!({}), false).unwrap();
        }
        assert_eq!(app.document_writes.len(), MAX_WAITING_SAVES);
        assert!(!app.confirm_document_write(handed.borrow()[0].0), "the oldest is forgotten");
    }

    #[test]
    fn inspect_names_each_document_s_id_path_and_whether_it_is_active() {
        let (mut app, _, _) = app_with_logo();
        app.run("file.new", json!({"width": 100, "height": 80})).unwrap();
        let docs = crate::control::inspect(&app, &egui::Context::default())["documents"].clone();
        let uids: Vec<u64> = app.session.documents().iter().map(|d| d.uid).collect();
        assert_eq!(docs[0]["id"], uids[0]);
        assert_eq!(docs[0]["path"], "logo.svg");
        assert_eq!(docs[0]["active"], false);
        assert_eq!(docs[0]["dirty"], true);
        assert_eq!(docs[1]["id"], uids[1]);
        assert_eq!(docs[1]["path"], Value::Null);
        assert_eq!(docs[1]["active"], true);
        assert!(docs[1]["title"].is_string());
    }

    #[test]
    fn only_a_save_over_the_document_s_own_file_is_a_plain_save() {
        assert!(!save_as(SaveMode::Save, Some("logo.svg"), "logo.svg"));
        assert!(save_as(SaveMode::SaveAs, Some("logo.svg"), "logo.svg"), "Save As under the same name");
        assert!(save_as(SaveMode::Save, None, "Untitled-1.vectorcraft"), "a first save");
        assert!(save_as(SaveMode::Save, Some("logo.svg"), "logo.vectorcraft"), "another format");
        assert!(save_as(SaveMode::SaveAs, Some("logo.svg"), "poster.svg"), "another name");
    }
}
