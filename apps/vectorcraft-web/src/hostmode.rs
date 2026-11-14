//! Pure host-mode rules, shared by the bridge (`host`) and testable without a browser: the size
//! limit for a file from the host, the path a hosted file is kept under, and the ids that tie a
//! save to the host's confirmation. Which pages host the app is `craft-web-host`'s.

use vectorcraft_ui_egui::io::opens_as;

/// The largest file taken from the host page, refused before it is copied into the module: wasm32
/// has 4 GiB in all, and opening a drawing takes several times its file.
pub const MAX_OPEN_BYTES: u64 = 512 << 20;

/// The name `craft-web-host` gives an open request that carries no usable one (it cleans a host's
/// name and falls back to this).
const NAMELESS: &str = "document";

/// The prefix of the ids sent with `vectorcraft:save`.
const SAVE_ID: &str = "save-";

/// Whether a file of `size` bytes from the host may be opened.
pub fn accepts_size(size: f64) -> bool {
    size.is_finite() && size >= 0.0 && size <= MAX_OPEN_BYTES as f64
}

/// The path a file from the host is opened under: its name, so File › Save writes back to it when
/// its format keeps the document (the engine keeps the path only then). Files that open as
/// something else than a document (swatches, presets, libraries, scripts, plug-ins) and a file
/// with no usable name have none: those would be read again from it.
pub fn hosted_path(name: &str) -> Option<String> {
    (name != NAMELESS && opens_as(name).is_none()).then(|| name.to_string())
}

/// The id sent to the host with save `save` ([`vectorcraft_ui_egui::DocumentWrite::save`]).
pub fn save_id(save: u64) -> String {
    format!("{SAVE_ID}{save}")
}

/// The save a host's `vectorcraft:saved { id }` confirms; `None` for an id this app never sent.
pub fn save_of(id: &str) -> Option<u64> {
    id.strip_prefix(SAVE_ID)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_file_over_the_limit_or_with_a_bad_size_is_refused() {
        assert!(accepts_size(0.0));
        assert!(accepts_size(MAX_OPEN_BYTES as f64));
        assert!(!accepts_size(MAX_OPEN_BYTES as f64 + 1.0));
        assert!(!accepts_size(f64::NAN));
        assert!(!accepts_size(-1.0));
        assert!(!accepts_size(f64::INFINITY));
    }

    #[test]
    fn a_document_lives_under_its_name_and_other_files_have_no_path() {
        assert_eq!(hosted_path("logo.svg").as_deref(), Some("logo.svg"));
        assert_eq!(hosted_path("poster.vectorcraft").as_deref(), Some("poster.vectorcraft"));
        assert_eq!(hosted_path("photo.png").as_deref(), Some("photo.png"), "the engine drops it: not a save format");
        assert_eq!(hosted_path("document"), None, "what the crate says for a missing or blank name");
        assert_eq!(hosted_path("brand.vcswatches"), None, "a swatch library would be read again from its path");
        assert_eq!(hosted_path("filter.wasm"), None, "a plug-in");
    }

    #[test]
    fn a_save_id_comes_back_as_its_save() {
        assert_eq!(save_of(&save_id(7)), Some(7));
        assert_eq!(save_of("save-"), None);
        assert_eq!(save_of("save-x"), None);
        assert_eq!(save_of("7"), None);
        assert_eq!(save_of("save-99999999999999999999999"), None, "no number that big was sent");
    }
}
