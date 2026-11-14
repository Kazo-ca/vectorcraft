//! Host mode: a page that runs VectorCraft (as an iframe, or in its own document) opens files in
//! it and keeps what it saves, instead of the browser picker and downloads. The messages and the
//! two transports are `craft-web-host`'s (storytold/craft-libs), the contract the other Craft web
//! builds share, with the `vectorcraft:` prefix.
//!
//! - `?host=parent` (an `<iframe>`) talks to the same-origin parent over `postMessage`;
//!   `data-host="page"` on the canvas (the page's own document) uses DOM `CustomEvent`s on the
//!   canvas. With no parent, or a parent on another origin, there is no bridge: the normal app.
//! - `vectorcraft:ready { version, appVersion, file: false }` once the app listens: send a file
//!   now. `vectorcraft:failed { error }` when the app couldn't start, couldn't take a file, or
//!   lost its graphics for good.
//! - `vectorcraft:open { name, bytes }` opens the file as File › Open does, a document kept under
//!   its name, so File › Save writes back to it when its format keeps the document. A file over
//!   [`MAX_OPEN_BYTES`] is refused before it is copied.
//! - `vectorcraft:save { name, bytes, saveAs, id }` for each save of a document to its own file
//!   (Save, Save As; `Services::write_document`). `saveAs` is `false` when the document is written
//!   over its own file, `true` otherwise (Save As, a first save, another format). The document is
//!   saved only when the host answers `vectorcraft:saved { id }`; until then it stays modified.
//!   Copies, templates, exports, preset files and a save's other files (separate artboards,
//!   linked images) still download.
//! - `vectorcraft:dirty { dirty }` when unsaved changes appear in any open document or go away.
//!
//! The bridge is made once, on the page's first canvas: after a graphics restart the app draws on
//! a new canvas, but the host keeps talking to the first one, which still carries the listeners.

use std::cell::RefCell;
use std::rc::Rc;

use craft_web_host::web::{FileData, Host, Message};
use vectorcraft_ui_egui::{DocumentWrite, Services, VectorcraftApp, io};

use crate::hostmode::{MAX_OPEN_BYTES, accepts_size, hosted_path, save_id, save_of};

/// The app id in the host messages (`vectorcraft:ready`).
const APP: &str = "vectorcraft";

#[derive(Default)]
struct Queues {
    /// Files from the host, for the next frame: (name, bytes).
    opens: Vec<(String, Vec<u8>)>,
    /// `saved` ids from the host, for the next frame.
    saved: Vec<String>,
}

#[derive(Clone)]
pub struct Bridge {
    host: Rc<Host>,
    queues: Rc<RefCell<Queues>>,
    /// The context of the runner that draws now, woken when a message arrives (a graphics restart
    /// brings a new one).
    ctx: Rc<RefCell<Option<egui::Context>>>,
}

impl Bridge {
    /// The bridge for this page, listening for the host's files and confirmations, or `None` when
    /// VectorCraft isn't hosted (see the module docs).
    pub fn detect(canvas: &web_sys::HtmlCanvasElement) -> Option<Self> {
        let bridge = Self { host: Rc::new(Host::connect(APP, canvas)?), queues: Rc::default(), ctx: Rc::default() };
        bridge.listen();
        Some(bridge)
    }

    /// A new app's (or a restarted app's) services: document saves go to the host, and the app
    /// knows it is hosted. `ctx` is the runner's that draws it.
    pub fn connect(&self, services: &mut Services, ctx: &egui::Context) {
        if let Ok(mut c) = self.ctx.try_borrow_mut() {
            *c = Some(ctx.clone());
        }
        services.hosted = true;
        let host = self.host.clone();
        services.write_document = Some(Box::new(move |write: DocumentWrite<'_>| post_save(&host, write)));
    }

    pub fn post_ready(&self) {
        if let Err(e) = self.host.post_ready(env!("CARGO_PKG_VERSION"), false) {
            log::error!("couldn't tell the page VectorCraft is ready: {e}");
        }
    }

    pub fn post_failed(&self, error: &str) {
        if let Err(e) = self.host.post_failed(error, None) {
            log::error!("couldn't tell the page that {error}: {e}");
        }
    }

    /// Each frame, before the app's: open the host's files, mark confirmed saves, and tell the
    /// host when unsaved changes appear or go away (the crate doesn't repeat a state).
    pub fn sync(&self, app: &mut VectorcraftApp) {
        let (opens, saved) = {
            let Ok(mut q) = self.queues.try_borrow_mut() else { return };
            (std::mem::take(&mut q.opens), std::mem::take(&mut q.saved))
        };
        for (name, bytes) in opens {
            if let Err(e) = io::open_bytes(app, &name, &bytes, hosted_path(&name)) {
                log::error!("couldn't open {name} from the page: {e}");
                self.post_failed(&format!("couldn't open {name}: {e}"));
                io::report_open_error(app, &name, &e);
            }
        }
        for id in saved {
            match save_of(&id) {
                Some(save) if app.confirm_document_write(save) => {}
                _ => log::warn!("the page confirmed a save VectorCraft isn't waiting for ({id})"),
            }
        }
        self.host.post_dirty(app.session.documents().iter().any(|d| d.is_dirty()));
    }

    fn listen(&self) {
        let (queues, host, ctx) = (self.queues.clone(), self.host.clone(), self.ctx.clone());
        self.host.listen(move |message| {
            let Ok(mut q) = queues.try_borrow_mut() else {
                log::warn!("a message from the page arrived while the last one was being read; dropped");
                return;
            };
            match message {
                Message::Open { name, data } => match take_open(&name, &data) {
                    Ok(bytes) => q.opens.push((name, bytes)),
                    Err(e) => {
                        log::warn!("{e}");
                        if let Err(e) = host.post_failed(&e, None) {
                            log::error!("couldn't tell the page a file was refused: {e}");
                        }
                        return;
                    }
                },
                Message::Saved { id } => q.saved.push(id),
                Message::Host { .. } | Message::HandoffDone { .. } => return,
            }
            if let Ok(c) = ctx.try_borrow()
                && let Some(ctx) = c.as_ref()
            {
                ctx.request_repaint();
            }
        });
    }
}

/// A save of a document to its own file, handed to the host with an id it answers with `saved`.
fn post_save(host: &Host, write: DocumentWrite<'_>) -> Result<(), String> {
    let name = vectorcraft_engine::cmd::fileio::file_name(write.path);
    host.post_save(&name, write.bytes, Some(&save_id(write.save)), write.save_as).map_err(|e| format!("couldn't hand {name} to the page: {e}"))
}

/// The bytes an open message carries. A host's file is untrusted input: an oversized one is
/// refused before it is copied into the module's memory (the data's own length, not the host's
/// claim).
fn take_open(name: &str, data: &FileData) -> Result<Vec<u8>, String> {
    let FileData::Bytes(array) = data else {
        return Err(format!("{name} came as a File; VectorCraft reads bytes only (ready says file: false)"));
    };
    if !accepts_size(data.size()) {
        return Err(format!("{name} is {} bytes, over the {} MiB limit for a file from the page", data.size(), MAX_OPEN_BYTES >> 20));
    }
    Ok(array.to_vec())
}
