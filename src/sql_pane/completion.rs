//! Schema-aware completion in the SQL editor: gpui-component's completion menu,
//! filled by `sql_lang::complete` against the pane's catalog.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use gpui_kit::component::{Rope, RopeExt as _};
use gpui_kit::component::input::CompletionProvider;
use gpui_kit::{App, Task, Window};
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, TextEdit};
use sql_core::{Catalog, Dialect};
use sql_lang::CompletionKind;

/// What completion reads; the pane swaps the catalog in after (re)introspecting.
#[derive(Clone)]
pub struct Shared {
    pub catalog: Rc<RefCell<Arc<Catalog>>>,
    pub dialect: Rc<Cell<Dialect>>,
}

pub struct SqlCompletion(pub Shared);

fn kind(k: CompletionKind) -> CompletionItemKind {
    match k {
        CompletionKind::Keyword => CompletionItemKind::KEYWORD,
        CompletionKind::Schema => CompletionItemKind::MODULE,
        CompletionKind::Table => CompletionItemKind::STRUCT,
        CompletionKind::View => CompletionItemKind::INTERFACE,
        CompletionKind::Column => CompletionItemKind::FIELD,
        CompletionKind::Function => CompletionItemKind::FUNCTION,
        CompletionKind::Alias => CompletionItemKind::VARIABLE,
        CompletionKind::Join => CompletionItemKind::REFERENCE,
        CompletionKind::Snippet => CompletionItemKind::SNIPPET,
    }
}

impl CompletionProvider for SqlCompletion {
    fn completions(&self, text: &Rope, offset: usize, _: CompletionContext, _: &mut Window, cx: &mut App) -> Task<Result<CompletionResponse>> {
        let (rope, catalog, dialect) = (text.clone(), self.0.catalog.borrow().clone(), self.0.dialect.get());
        cx.background_executor().spawn(async move {
            let res = sql_lang::complete(&rope.to_string(), offset, dialect, &catalog);
            let range = lsp_types::Range { start: rope.offset_to_position(res.replace.start), end: rope.offset_to_position(res.replace.end) };
            let items = res
                .items
                .into_iter()
                .enumerate()
                .map(|(i, c)| CompletionItem {
                    label: c.label,
                    kind: Some(kind(c.kind)),
                    detail: c.detail,
                    // Ranked by sql-lang: keep its order.
                    sort_text: Some(format!("{i:05}")),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text: c.insert })),
                    ..Default::default()
                })
                .collect();
            Ok(CompletionResponse::Array(items))
        })
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _: &mut App) -> bool {
        new_text.chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
    }
}
