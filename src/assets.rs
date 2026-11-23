//! Assets: SplitGrid's own icons (copied from the Electron app's SVGs, so they
//! match) first, then gpui-kit's icon set for its components.

use std::borrow::Cow;

use gpui_kit::{AssetSource, SharedString};

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        const ICONS: &[(&str, &[u8])] = &[
            $((concat!("sg/", $name, ".svg"), include_bytes!(concat!("../assets/icons/", $name, ".svg")))),*
        ];
    };
}

icons!(
    "activity",
    "checklist",
    "chevron-down",
    "chevron-right",
    "commands",
    "compress",
    "expand",
    "folder",
    "folder-open",
    "gear",
    "logo",
    "menu-new",
    "menu-open",
    "menu-recent",
    "new-file",
    "new-folder",
    "notes",
    "plus-square",
    "ports",
    "processes",
    "prompts",
    "reconnect",
    "refresh",
    "sidebar",
    "split-down",
    "split-right",
    "sql-arrow-right",
    "sql-column",
    "sql-database",
    "sql-function",
    "sql-index",
    "sql-key",
    "sql-link",
    "sql-schema",
    "sql-sequence",
    "sql-table",
    "sql-view",
    "tray-in",
    "tray-out",
);

/// Path of one of our icons, for `svg().path(..)`.
pub fn icon(name: &str) -> SharedString {
    format!("sg/{name}.svg").into()
}

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = ICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut out: Vec<SharedString> = ICONS.iter().filter(|(p, _)| p.starts_with(path)).map(|(p, _)| (*p).into()).collect();
        out.extend(gpui_kit::assets::Assets.list(path)?);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_icons_load_and_parse() {
        for (path, bytes) in ICONS {
            assert!(Assets.load(path).unwrap().is_some(), "{path}");
            let text = std::str::from_utf8(bytes).unwrap();
            assert!(text.starts_with("<svg") && text.contains("viewBox"), "{path} is an svg");
        }
        assert_eq!(icon("gear"), "sg/gear.svg");
    }
}
