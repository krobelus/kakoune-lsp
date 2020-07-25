use crate::context::*;
use crate::language_features::rust_analyzer;
use crate::position::{kakoune_position_to_lsp, kakoune_range_to_lsp};
use crate::types::*;
use crate::util::*;
use jsonrpc_core::{Id, Params};
use lsp_types::notification::*;
use lsp_types::request::*;
use lsp_types::*;
use serde::Deserialize;
use serde_json::{self, Value};
use std::fs;
use std::io;
use toml;

fn insert_value<'a, 'b, P>(
    target: &'b mut serde_json::map::Map<String, Value>,
    mut path: P,
    local_key: String,
    value: Value,
) -> Result<(), String>
where
    P: Iterator<Item = &'a str>,
    P: 'a,
{
    match path.next() {
        Some(key) => {
            let maybe_new_target = target
                .entry(key)
                .or_insert_with(|| Value::Object(serde_json::Map::new()))
                .as_object_mut();

            if maybe_new_target.is_none() {
                return Err(format!(
                    "Expected path {:?} to be object, found {:?}",
                    key, &maybe_new_target,
                ));
            }

            insert_value(maybe_new_target.unwrap(), path, local_key, value)
        }
        None => match target.insert(local_key, value) {
            Some(old_value) => Err(format!("Replaced old value: {:?}", old_value)),
            None => Ok(()),
        },
    }
}

pub fn did_change_configuration(params: EditorParams, ctx: &mut Context) {
    let default_settings = toml::value::Table::new();

    let raw_settings = params
        .as_table()
        .and_then(|t| t.get("settings"))
        .and_then(|val| val.as_table())
        .unwrap_or_else(|| &default_settings);

    let mut settings = serde_json::Map::new();

    for (raw_key, raw_value) in raw_settings.iter() {
        let mut key_parts = raw_key.split('.');
        let local_key = match key_parts.next_back() {
            Some(name) => name,
            None => {
                warn!("Got a setting with an empty local name: {:?}", raw_key);
                continue;
            }
        };

        let value: Value = match raw_value.clone().try_into() {
            Ok(value) => value,
            Err(e) => {
                warn!("Could not convert setting {:?} to JSON: {}", raw_value, e,);
                continue;
            }
        };

        match insert_value(&mut settings, key_parts, local_key.into(), value) {
            Ok(_) => (),
            Err(e) => {
                warn!("Could not set {:?} to {:?}: {}", raw_key, raw_value, e);
                continue;
            }
        }
    }

    let params = DidChangeConfigurationParams {
        settings: Value::Object(settings),
    };
    ctx.notify::<DidChangeConfiguration>(params);
}

pub fn workspace_symbol(meta: EditorMeta, params: EditorParams, ctx: &mut Context) {
    let params = WorkspaceSymbolParams::deserialize(params)
        .expect("Params should follow WorkspaceSymbolParams structure");
    ctx.call::<WorkspaceSymbol, _>(meta, params, move |ctx: &mut Context, meta, result| {
        editor_workspace_symbol(meta, result, ctx)
    });
}

pub fn editor_workspace_symbol(
    meta: EditorMeta,
    result: Option<Vec<SymbolInformation>>,
    ctx: &mut Context,
) {
    if result.is_none() {
        return;
    }
    let result = result.unwrap();
    let content = format_symbol_information(result, ctx);
    let command = format!(
        "lsp-show-workspace-symbol {} {}",
        editor_quote(&ctx.root_path),
        editor_quote(&content),
    );
    ctx.exec(meta, command);
}

#[derive(Deserialize)]
struct EditorExecuteCommand {
    command: String,
    arguments: String,
}

pub fn execute_command(meta: EditorMeta, params: EditorParams, ctx: &mut Context) {
    let params = EditorExecuteCommand::deserialize(params)
        .expect("Params should follow ExecuteCommand structure");
    let req_params = ExecuteCommandParams {
        command: params.command,
        // arguments is quoted to avoid parsing issues
        arguments: serde_json::from_str(&params.arguments).unwrap(),
        work_done_progress_params: Default::default(),
    };
    match &*req_params.command {
        "rust-analyzer.applySourceChange" => {
            rust_analyzer::apply_source_change(meta, req_params, ctx);
        }
        _ => {
            ctx.call::<ExecuteCommand, _>(meta, req_params, move |_: &mut Context, _, _| ());
        }
    }
}

pub fn apply_document_resource_op(
    _meta: &EditorMeta,
    op: ResourceOp,
    _ctx: &mut Context,
) -> io::Result<()> {
    match op {
        ResourceOp::Create(op) => {
            let path = op.uri.to_file_path().unwrap();
            let ignore_if_exists = if let Some(options) = op.options {
                !options.overwrite.unwrap_or(false) && options.ignore_if_exists.unwrap_or(false)
            } else {
                false
            };
            if ignore_if_exists && path.exists() {
                Ok(())
            } else {
                fs::write(&path, [])
            }
        }
        ResourceOp::Delete(op) => {
            let path = op.uri.to_file_path().unwrap();
            if path.is_dir() {
                let recursive = if let Some(options) = op.options {
                    options.recursive.unwrap_or(false)
                } else {
                    false
                };
                if recursive {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_dir(&path)
                }
            } else if path.is_file() {
                fs::remove_file(&path)
            } else {
                Ok(())
            }
        }
        ResourceOp::Rename(op) => {
            let from = op.old_uri.to_file_path().unwrap();
            let to = op.new_uri.to_file_path().unwrap();
            let ignore_if_exists = if let Some(options) = op.options {
                !options.overwrite.unwrap_or(false) && options.ignore_if_exists.unwrap_or(false)
            } else {
                false
            };
            if ignore_if_exists && to.exists() {
                Ok(())
            } else {
                fs::rename(&from, &to)
            }
        }
    }
}

// TODO handle version, so change is not applied if buffer is modified (and need to show a warning)
pub fn apply_edit(
    meta: EditorMeta,
    edit: WorkspaceEdit,
    ctx: &mut Context,
) -> ApplyWorkspaceEditResponse {
    if let Some(document_changes) = edit.document_changes {
        match document_changes {
            DocumentChanges::Edits(edits) => {
                for edit in edits {
                    apply_text_edits(&meta, &edit.text_document.uri, &edit.edits, ctx);
                }
            }
            DocumentChanges::Operations(ops) => {
                for op in ops {
                    match op {
                        DocumentChangeOperation::Edit(edit) => {
                            apply_text_edits(&meta, &edit.text_document.uri, &edit.edits, ctx);
                        }
                        DocumentChangeOperation::Op(op) => {
                            if let Err(e) = apply_document_resource_op(&meta, op, ctx) {
                                error!("failed to apply document change operation: {}", e);
                                return ApplyWorkspaceEditResponse { applied: false };
                            }
                        }
                    }
                }
            }
        }
    } else if let Some(changes) = edit.changes {
        for (uri, change) in &changes {
            apply_text_edits(&meta, uri, change, ctx);
        }
    }
    ApplyWorkspaceEditResponse { applied: true }
}

#[derive(Deserialize)]
struct EditorApplyEdit {
    edit: String,
}

pub fn apply_edit_from_editor(meta: EditorMeta, params: EditorParams, ctx: &mut Context) {
    let params = EditorApplyEdit::deserialize(params).expect("Failed to parse params");
    let edit = WorkspaceEdit::deserialize(serde_json::from_str::<Value>(&params.edit).unwrap())
        .expect("Failed to parse edit");

    apply_edit(meta, edit, ctx);
}

#[derive(Deserialize)]
struct CompletionApplyEdit {
    range: String,
    label: String,
}

// We store text edits from textDocument/completion in a map, indexed by the completion's label,
// which is inserted by Kakoune's completion engine when accepting the completion.
// Replace the label by the completion's text edit.
// Additionally clean up leftover text right of the cursor. This would
// be replaced by the completion's text edit, however since the label was
// inserted at the cursor, we need to recompute the ranges.
pub fn apply_completion_edit_from_editor(
    meta: EditorMeta,
    params: EditorParams,
    ctx: &mut Context,
) {
    let params = CompletionApplyEdit::deserialize(params).expect("Failed to parse params");
    let saved_text_edit = match ctx.completion_text_edits.get_mut(&meta.client) {
        None => {
            return warn!(
                "Cannot find text edits for client: {}",
                meta.client.unwrap_or("".to_string())
            )
        }
        Some(client_completion_text_edits) => {
            client_completion_text_edits
                .remove_entry(&params.label)
                .or_else(|| {
                    client_completion_text_edits
                        // HACK we get one character too many from "select %val{hook_param}" in InsertCompletionHide.
                        .remove_entry(without_last_character(&params.label))
                })
        }
    };
    match saved_text_edit {
        Some((completion_label, completion_text_edit)) => {
            apply_completion_edit(&meta, &params, ctx, completion_label, completion_text_edit)
        }
        None => warn!("Cannot find completion by label: {}", &params.label),
    };

    // Invalidate this client's edits since they were accepted or rejected.
    if let Some(map) = ctx.completion_text_edits.get_mut(&meta.client) {
        map.clear();
    }
}

fn apply_completion_edit(
    meta: &EditorMeta,
    params: &CompletionApplyEdit,
    ctx: &Context,
    completion_label: String,
    completion_text_edit: CompletionTextEdit,
) {
    let coordinates: Vec<_> = params
        .range
        .split(|c| c == ',' || c == '.')
        .filter_map(|num| num.parse::<u64>().ok())
        .collect();
    if coordinates.len() != 4 {
        return error!("Failed to parse position: {}", &params.range);
    }
    let document = match ctx.documents.get(&meta.buffile) {
        Some(doc) => doc,
        None => return error!("No document in context for file: {}", &meta.buffile),
    };
    let inserted_range = kakoune_range_to_lsp(
        &KakouneRange {
            start: KakounePosition {
                line: coordinates[0],
                column: coordinates[1],
            },
            end: KakounePosition {
                line: coordinates[2],
                column: coordinates[3],
            },
        },
        &document.text,
        ctx.offset_encoding,
    );

    let text_edit = match completion_text_edit {
        CompletionTextEdit::Edit(e) => e,
        CompletionTextEdit::InsertAndReplace(e) => {
            // TODO We assume replace.
            TextEdit::new(e.replace, e.new_text)
        }
    };

    // Replace the completion label left of the cursor with the completion text.
    let mut left_range = Range::new(text_edit.range.start, text_edit.range.start);
    // After the text edit was sent, we did insert the completion label.
    left_range.end.character += completion_label.chars().count() as u64;
    let left_edit = TextEdit::new(left_range, text_edit.new_text);

    // Delete leftover text right of the cursor.
    let mut right_range = Range::new(inserted_range.end, inserted_range.end);
    let cursor_width = 1;
    right_range.start.character += cursor_width;
    let cursor_position = kakoune_position_to_lsp(
        &ctx.completion_cursor_position,
        &document.text,
        &ctx.offset_encoding,
    );
    // TODO I think we get an overflow because of stale documents.
    let range_right_of_cursor = text_edit
        .range
        .end
        .character
        .saturating_sub(cursor_position.character);
    right_range.end.character += cursor_width + range_right_of_cursor;
    let right_edit = TextEdit::new(right_range, "".to_string());

    let mut edits = vec![left_edit];
    assert!(left_range.start != left_range.end);
    if right_edit.range.start != right_edit.range.end {
        edits.push(right_edit);
    }

    let uri = Url::from_file_path(meta.buffile.clone()).expect("Failed to construct URI");
    apply_text_edits(&meta, &uri, &edits[..], ctx);
}

fn without_last_character(s: &str) -> &str {
    let end = s.char_indices().last().map_or(0, |tuple| tuple.0);
    &s[..end]
}

pub fn apply_edit_from_server(id: Id, params: Params, ctx: &mut Context) {
    let params: ApplyWorkspaceEditParams = params.parse().expect("Failed to parse params");
    let meta = ctx.meta_for_session();
    let response = apply_edit(meta, params.edit, ctx);
    ctx.reply(id, Ok(serde_json::to_value(response).unwrap()));
}
