/// Syma LSP server — provides IDE features for the Syma language.
///
/// Features:
/// - Diagnostics (lexer/parser errors)
/// - Completions (builtins + user symbols)
/// - Hover (symbol info via get_help)
/// - Go-to-definition
/// - Document symbols (outline)
/// - Signature help
/// - Rename
/// - Workspace symbols
/// - References
/// - Code actions
/// - Folding ranges
/// - Semantic tokens
/// - Document highlights
use std::collections::HashMap;
use std::error::Error;

use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response, ResponseError};
use lsp_types::*;

use syma::ast::Expr;
use syma::builtins::get_help;
use syma::kernel::SymaKernel;
use syma::lexer::{Span, SpannedToken, Token};

// ── Helpers ──

/// Convert a Syma Span (1-based line/col) to an LSP Position (0-based).
fn syma_span_to_lsp_position(span: &Span) -> Position {
    Position {
        line: (span.line.max(1) - 1) as u32,
        character: (span.col.max(1) - 1) as u32,
    }
}

/// Convert a Syma Span to an LSP Range spanning a single character.
fn syma_span_to_lsp_range(span: &Span) -> Range {
    let pos = syma_span_to_lsp_position(span);
    Range {
        start: pos,
        end: Position {
            line: pos.line,
            character: pos.character + 1,
        },
    }
}

/// Create an LSP Range for a token span with known character length.
fn span_to_range_with_length(span: &Span, length: usize) -> Range {
    let pos = syma_span_to_lsp_position(span);
    Range {
        start: pos,
        end: Position {
            line: pos.line,
            character: pos.character + length as u32,
        },
    }
}

/// Parse a URI string into a proper URL.
fn parse_uri(uri: &str) -> Option<Url> {
    Url::parse(uri).or_else(|_| Url::parse(&format!("file://{}", uri))).ok()
}

// ── Symbol definition ──

/// A definition site for a symbol.
#[derive(Debug, Clone)]
struct SymbolDef {
    name: String,
    uri: String,
    range: Range,
    kind: SymbolKind,
    detail: String,
    /// Optional function signature showing parameter names, e.g. "x, y"
    signature: Option<String>,
}

// ── LSP server backend ──

struct Backend {
    kernel: SymaKernel,
    /// Open documents: URI → source text.
    documents: HashMap<String, String>,
    /// All known symbol definitions across open documents.
    symbol_defs: Vec<SymbolDef>,
}

impl Backend {
    fn new() -> Self {
        Backend {
            kernel: SymaKernel::new(),
            documents: HashMap::new(),
            symbol_defs: Vec::new(),
        }
    }

    // ── Document management ──

    fn open_document(&mut self, uri: &str, text: &str) {
        self.documents.insert(uri.to_string(), text.to_string());
        self.scan_document(uri, text);
    }

    fn update_document(&mut self, uri: &str, text: &str) {
        self.documents.insert(uri.to_string(), text.to_string());
        self.scan_document(uri, text);
    }

    fn close_document(&mut self, uri: &str) {
        self.documents.remove(uri);
        self.symbol_defs.retain(|d| d.uri != uri);
    }

    // ── Diagnostics ──

    fn compute_diagnostics(&self, text: &str) -> Vec<Diagnostic> {
        let mut diags: Vec<Diagnostic> = Vec::new();

        match syma::lexer::tokenize(text) {
            Ok(tokens) => {
                let mut parser = syma::parser::Parser::new(tokens);
                if let Err(e) = parser.parse_program() {
                    let range = e.span.as_ref().map(syma_span_to_lsp_range).unwrap_or_default();
                    diags.push(Diagnostic {
                        range,
                        severity: Some(DiagnosticSeverity::ERROR),
                        message: e.message,
                        source: Some("syma".to_string()),
                        ..Default::default()
                    });
                }
            }
            Err(e) => {
                let range = Range {
                    start: Position {
                        line: (e.line.max(1) - 1) as u32,
                        character: (e.col.max(1) - 1) as u32,
                    },
                    end: Position {
                        line: (e.line.max(1) - 1) as u32,
                        character: e.col.max(1) as u32,
                    },
                };
                diags.push(Diagnostic {
                    range,
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: e.message,
                    source: Some("syma".to_string()),
                    ..Default::default()
                });
            }
        }

        diags
    }

    // ── Symbol scanning (AST-based) ──

    fn scan_document(&mut self, uri: &str, text: &str) {
        self.symbol_defs.retain(|d| d.uri != uri);

        let tokens = match syma::lexer::tokenize(text) {
            Ok(t) => t,
            Err(_) => return,
        };

        let mut parser = syma::parser::Parser::new(tokens);
        let statements = match parser.parse_program() {
            Ok(stmts) => stmts,
            Err(_) => return,
        };

        // Re-tokenize for span lookups
        let all_tokens = syma::lexer::tokenize(text).unwrap_or_default();

        for stmt in &statements {
            walk_expr_for_symbols(stmt, uri, &all_tokens, &mut self.symbol_defs);
        }
    }

    // ── Token extraction ──

    /// Extract the word token at a given position in source text.
    fn token_at_position(text: &str, pos: Position) -> Option<String> {
        let line = pos.line as usize;
        let col = pos.character as usize;
        let line_str = text.lines().nth(line)?;
        let mut char_idx = 0;
        for (i, c) in line_str.char_indices() {
            if char_idx == col {
                let rest = &line_str[i..];
                let end = rest
                    .find(|c: char| !c.is_alphanumeric() && c != '_' && c != '$')
                    .unwrap_or(rest.len());
                if end == 0 {
                    return None;
                }
                return Some(rest[..end].to_string());
            }
            char_idx += c.len_utf8();
        }
        None
    }

    /// Extract the word prefix at cursor position for completion filtering.
    fn word_prefix_at_position(text: &str, pos: Position) -> String {
        let line = pos.line as usize;
        let col = pos.character as usize;
        let line_str = text.lines().nth(line).unwrap_or("");
        let col = col.min(line_str.len());
        let before = &line_str[..col];
        let start = before
            .rfind(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|i| i + 1)
            .unwrap_or(0);
        before[start..].to_string()
    }

    /// Find the byte offset of a Position in the text.
    fn position_to_byte_offset(text: &str, pos: Position) -> Option<usize> {
        let mut line_idx: usize = 0;
        for line in text.lines() {
            if line_idx == pos.line as usize {
                let char_idx = pos.character as usize;
                let mut byte_offset = 0;
                for (i, _) in line.char_indices() {
                    if i == char_idx {
                        byte_offset = i;
                        break;
                    }
                    byte_offset = i;
                }
                return Some(byte_offset);
            }
            line_idx += 1;
        }
        None
    }

    // ── Completions ──

    fn get_completions(&self, uri: &str, pos: Position) -> Vec<CompletionItem> {
        let prefix = self
            .documents
            .get(uri)
            .map(|text| Self::word_prefix_at_position(text, pos))
            .unwrap_or_default();
        let prefix_lower = prefix.to_lowercase();

        let mut items: Vec<CompletionItem> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        let env = self.kernel.env();
        let bindings = env.all_bindings();
        for (name, _val) in &bindings {
            if !seen.insert(name.clone()) {
                continue;
            }
            if !prefix_lower.is_empty()
                && !name.to_lowercase().starts_with(&prefix_lower)
            {
                continue;
            }
            let kind = if name.starts_with(|c: char| c.is_uppercase()) {
                CompletionItemKind::FUNCTION
            } else {
                CompletionItemKind::VARIABLE
            };
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(kind),
                detail: Some("Syma builtin".to_string()),
                insert_text: Some(name.clone()),
                ..Default::default()
            });
        }

        for def in &self.symbol_defs {
            if !seen.insert(def.name.clone()) {
                continue;
            }
            if !prefix_lower.is_empty()
                && !def.name.to_lowercase().starts_with(&prefix_lower)
            {
                continue;
            }
            items.push(CompletionItem {
                label: def.name.clone(),
                kind: Some(match def.kind {
                    SymbolKind::FUNCTION => CompletionItemKind::FUNCTION,
                    SymbolKind::CLASS => CompletionItemKind::CLASS,
                    SymbolKind::MODULE => CompletionItemKind::MODULE,
                    _ => CompletionItemKind::VARIABLE,
                }),
                detail: Some(def.detail.clone()),
                insert_text: Some(def.name.clone()),
                ..Default::default()
            });
        }

        items
    }

    // ── Hover ──

    fn get_hover(&self, uri: &str, pos: Position) -> Option<Hover> {
        let text = self.documents.get(uri)?;
        let token = Self::token_at_position(text, pos)?;

        // Check builtins via get_help first
        if let Some(help_text) = get_help(&token) {
            return Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: format!("```syma\n{}\n```", help_text),
                }),
                range: None,
            });
        }

        // Try kernel environment
        let env = self.kernel.env();
        let bindings = env.all_bindings();
        for (name, val) in &bindings {
            if name == &token {
                let contents = format!("```syma\n{} = {}\n```", name, val);
                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: contents,
                    }),
                    range: None,
                });
            }
        }

        // Check user-defined symbols
        for def in &self.symbol_defs {
            if def.name == token {
                let mut contents = def.detail.clone();
                if let Some(sig) = &def.signature {
                    contents = format!("{} | {}", contents, sig);
                }
                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!("```syma\n{}\n```", contents),
                    }),
                    range: None,
                });
            }
        }

        None
    }

    // ── Go-to-definition ──

    fn get_definition(&self, uri: &str, pos: Position) -> Option<Location> {
        let text = self.documents.get(uri)?;
        let token = Self::token_at_position(text, pos)?;

        for def in &self.symbol_defs {
            if def.name == token {
                if def.uri == uri && def.range.start == pos {
                    continue;
                }
                let url = parse_uri(&def.uri)?;
                return Some(Location {
                    uri: url,
                    range: def.range,
                });
            }
        }
        None
    }

    // ── Document symbols ──

    #[allow(deprecated)]
    fn get_document_symbols(&self, uri: &str) -> Vec<DocumentSymbol> {
        self.symbol_defs
            .iter()
            .filter(|d| d.uri == uri)
            .map(|d| {
                let detail = if let Some(sig) = &d.signature {
                    format!("{} | {}", d.detail, sig)
                } else {
                    d.detail.clone()
                };
                DocumentSymbol {
                    name: d.name.clone(),
                    detail: Some(detail),
                    kind: d.kind,
                    range: d.range,
                    selection_range: d.range,
                    children: None,
                    tags: None,
                    deprecated: None,
                }
            })
            .collect()
    }

    // ── Signature help ──

    fn get_signature_help(&self, uri: &str, pos: Position) -> Option<SignatureHelp> {
        let text = self.documents.get(uri)?;
        let byte_offset = Self::position_to_byte_offset(text, pos)?;

        // Look backward for the most recent `[` to find the call context
        let before = &text[..byte_offset];
        let bracket_pos = before.rfind('[')?;

        // Extract the head name (identifier before `[`)
        let head_text = before[..bracket_pos].trim_end();
        let head_name = if head_text.chars().all(|c: char| c.is_alphanumeric() || c == '_') && !head_text.is_empty() {
            head_text.to_string()
        } else {
            head_text.split(|c: char| !c.is_alphanumeric() && c != '_').next_back()?.to_string()
        };

        if head_name.is_empty() {
            return None;
        }

        // Count arguments already provided between `[` and cursor
        let inside_bracket = &before[bracket_pos + 1..];
        let arg_count = inside_bracket.matches(',').count() + 1;

        // Try to get help for the head name
        let (label, documentation) = if let Some(help) = get_help(&head_name) {
            (format!("{}[...]", head_name), Some(Documentation::String(help.to_string())))
        } else {
            // For user-defined functions, build a label from symbol_defs
            let sig = self.symbol_defs.iter()
                .find(|d| d.name == head_name)
                .and_then(|d| d.signature.clone())
                .map(|s| format!("{}[{}]", head_name, s))
                .unwrap_or_else(|| format!("{}[...]", head_name));
            (sig, None)
        };

        let parameters: Option<Vec<ParameterInformation>> = if let Some(help) = get_help(&head_name) {
            // Try to extract parameter count from help text like "Func[a, b, c] does X"
            if let Some(start) = help.find('[') {
                if let Some(end) = help[start..].find(']') {
                    let params_str = &help[start + 1..start + end];
                    if !params_str.is_empty() && params_str != "..." {
                        Some(params_str.split(',').map(|p| ParameterInformation {
                            label: ParameterLabel::Simple(p.trim().to_string()),
                            documentation: None,
                        }).collect())
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        Some(SignatureHelp {
            signatures: vec![SignatureInformation {
                label,
                documentation,
                parameters,
                active_parameter: None,
            }],
            active_signature: Some(0),
            active_parameter: Some((arg_count - 1) as u32),
        })
    }

    // ── Rename ──

    fn get_rename(
        &self,
        uri: &str,
        pos: Position,
        new_name: &str,
    ) -> Option<WorkspaceEdit> {
        let text = self.documents.get(uri)?;
        let old_name = Self::token_at_position(text, pos)?;

        if old_name == new_name {
            return Some(WorkspaceEdit::default());
        }

        // Collect all locations where this symbol is defined
        let def_uris: Vec<&str> = self
            .symbol_defs
            .iter()
            .filter(|d| d.name == old_name)
            .map(|d| d.uri.as_str())
            .collect();

        if def_uris.is_empty() {
            return self.rename_in_single_document(uri, text, &old_name, new_name);
        }

        let mut text_document_edits = Vec::new();

        for doc_uri in &def_uris {
            let doc_text = self.documents.get(*doc_uri)?;
            let edits = compute_rename_edits(doc_text, &old_name, new_name);
            let versioned = OptionalVersionedTextDocumentIdentifier {
                uri: parse_uri(doc_uri)?,
                version: None,
            };
            let oneof_edits: Vec<OneOf<TextEdit, AnnotatedTextEdit>> =
                edits.into_iter().map(|e| OneOf::Left(e)).collect();
            text_document_edits.push(TextDocumentEdit {
                text_document: versioned,
                edits: oneof_edits,
            });
        }

        Some(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(text_document_edits)),
            ..Default::default()
        })
    }

    /// Rename all occurrences of a token in a single document.
    fn rename_in_single_document(
        &self,
        uri: &str,
        text: &str,
        old_name: &str,
        new_name: &str,
    ) -> Option<WorkspaceEdit> {
        let edits = compute_rename_edits(text, old_name, new_name);
        if edits.is_empty() {
            return Some(WorkspaceEdit::default());
        }
        let versioned = OptionalVersionedTextDocumentIdentifier {
            uri: parse_uri(uri)?,
            version: None,
        };
        let oneof_edits: Vec<OneOf<TextEdit, AnnotatedTextEdit>> =
            edits.into_iter().map(|e| OneOf::Left(e)).collect();
        Some(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: versioned,
                edits: oneof_edits,
            }])),
            ..Default::default()
        })
    }

    // ── Workspace symbols ──

    #[allow(deprecated)]
    fn get_workspace_symbols(&self, query: &str) -> Vec<SymbolInformation> {
        let query_lower = query.to_lowercase();
        self.symbol_defs
            .iter()
            .filter(|d| d.name.to_lowercase().contains(&query_lower))
            .filter_map(|d| {
                let url = parse_uri(&d.uri)?;
                Some(SymbolInformation {
                    name: d.name.clone(),
                    kind: d.kind,
                    tags: None,
                    deprecated: None,
                    location: Location {
                        uri: url,
                        range: d.range,
                    },
                    container_name: None,
                })
            })
            .collect()
    }

    // ── References ──

    fn get_references(&self, uri: &str, pos: Position) -> Vec<Location> {
        let Some(text) = self.documents.get(uri) else {
            return Vec::new();
        };
        let Some(token) = Self::token_at_position(text, pos) else {
            return Vec::new();
        };

        let current_pos = pos;

        let mut locations = Vec::new();

        for (doc_uri, doc_text) in &self.documents {
            let edits = compute_rename_edits(doc_text, &token, &token);
            for edit in &edits {
                if edit.range.start != current_pos || *doc_uri != uri {
                    if let Some(url) = parse_uri(doc_uri) {
                        locations.push(Location {
                            uri: url,
                            range: edit.range,
                        });
                    }
                }
            }
        }

        locations
    }

    // ── Code actions ──

    fn get_code_actions(&self, _uri: &str, _range: Range, _context: CodeActionParams) -> Vec<CodeActionOrCommand> {
        vec![CodeActionOrCommand::CodeAction(CodeAction {
            title: "Dismiss error".to_string(),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: None,
            is_preferred: Some(true),
            disabled: None,
            command: None,
            edit: None,
            data: None,
        })]
    }

    // ── Folding ranges ──

    fn get_folding_ranges(&self, text: &str) -> Vec<FoldingRange> {
        let mut ranges = Vec::new();
        let chars: Vec<char> = text.chars().collect();
        let len = chars.len();

        // Stack of (open_index, FoldingRangeKind)
        let mut bracket_stack: Vec<(usize, FoldingRangeKind)> = Vec::new();

        let mut i = 0;
        while i < len {
            // Check for comment start (*
            if i + 1 < len && chars[i] == '(' && chars[i + 1] == '*' {
                let start = i;
                let mut depth = 1;
                let mut j = i + 2;
                let mut end = None;
                while j + 1 < len && depth > 0 {
                    if chars[j] == '(' && chars[j + 1] == '*' {
                        depth += 1;
                        j += 2;
                    } else if chars[j] == '*' && chars[j + 1] == ')' {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(j);
                            break;
                        }
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                if let Some(end_idx) = end {
                    let start_line = char_index_to_line_offset(text, start).line;
                    let end_line = char_index_to_line_offset(text, end_idx + 2).line;
                    if end_line > start_line || (end_line == start_line && end_idx > start + 2) {
                        ranges.push(FoldingRange {
                            start_line,
                            start_character: Some(char_index_to_line_offset(text, start).character),
                            end_line,
                            end_character: Some(char_index_to_line_offset(text, end_idx + 2).character),
                            kind: Some(FoldingRangeKind::Comment),
                            ..Default::default()
                        });
                    }
                }
                i = if let Some(e) = end { e + 2 } else { i + 1 };
                continue;
            }

            match chars[i] {
                '(' => bracket_stack.push((i, FoldingRangeKind::Region)),
                '[' => bracket_stack.push((i, FoldingRangeKind::Region)),
                '{' => bracket_stack.push((i, FoldingRangeKind::Region)),
                _ => {}
            }

            match chars[i] {
                ')' => {
                    if let Some((open_idx, kind)) = bracket_stack.pop() {
                        add_bracket_folding(&mut ranges, text, open_idx, i, kind);
                    }
                }
                ']' => {
                    if let Some((open_idx, kind)) = bracket_stack.pop() {
                        add_bracket_folding(&mut ranges, text, open_idx, i, kind);
                    }
                }
                '}' => {
                    if let Some((open_idx, kind)) = bracket_stack.pop() {
                        add_bracket_folding(&mut ranges, text, open_idx, i, kind);
                    }
                }
                _ => {}
            }

            i += 1;
        }

        ranges
    }

    // ── Semantic tokens ──

    fn get_semantic_tokens(&self, text: &str) -> Option<Vec<lsp_types::SemanticToken>> {
        let tokens = syma::lexer::tokenize(text).ok()?;

        let mut result: Vec<SemanticTokenRaw> = Vec::new();

        for spanned in &tokens {
            let token_type = match &spanned.token {
                // Keywords
                Token::If | Token::Which | Token::Switch | Token::Match
                | Token::For | Token::While | Token::Do | Token::Try
                | Token::Catch | Token::Finally | Token::Throw | Token::Function
                | Token::Class | Token::Extends | Token::With | Token::Method
                | Token::Field | Token::Constructor | Token::Module | Token::Import
                | Token::Export | Token::As | Token::RuleKw | Token::Hold
                | Token::HoldComplete | Token::ReleaseHold | Token::Mixin
                | Token::Else | Token::Def => 1, // keyword

                // Identifiers that look like built-in functions (uppercase start)
                Token::Ident(name) if name.starts_with(|c: char| c.is_uppercase()) => 0, // function

                // Numbers
                Token::Integer(_) | Token::Real(_) => 2, // number

                // Strings
                Token::Str(_) => 3, // string

                // Regular identifiers (variables)
                Token::Ident(_) => 4, // variable

                _ => continue,
            };

            let pos = syma_span_to_lsp_position(&spanned.span);
            let token_len = token_display_length(&spanned.token);

            result.push(SemanticTokenRaw {
                delta_line: pos.line,
                delta_start: pos.character,
                length: token_len as u32,
                token_type,
                modifier: 0,
            });
        }

        // Sort by position for correct delta encoding
        result.sort_by(|a, b| {
            a.delta_line
                .cmp(&b.delta_line)
                .then(a.delta_start.cmp(&b.delta_start))
        });

        // Encode as deltas and convert to lsp_types::SemanticToken
        let mut encoded: Vec<lsp_types::SemanticToken> = Vec::with_capacity(result.len());
        let mut prev_line = 0u32;
        let mut prev_start = 0u32;

        for token in &result {
            let delta_start = if token.delta_line == prev_line {
                token.delta_start - prev_start
            } else {
                token.delta_start
            };
            encoded.push(lsp_types::SemanticToken {
                delta_line: token.delta_line - prev_line,
                delta_start,
                length: token.length,
                token_type: token.token_type,
                token_modifiers_bitset: token.modifier,
            });
            prev_line = token.delta_line;
            prev_start = token.delta_start;
        }

        if encoded.is_empty() {
            return None;
        }

        Some(encoded)
    }

    // ── Document highlights ──

    fn get_document_highlights(&self, uri: &str, pos: Position) -> Vec<DocumentHighlight> {
        let Some(text) = self.documents.get(uri) else {
            return Vec::new();
        };
        let Some(token) = Self::token_at_position(text, pos) else {
            return Vec::new();
        };

        let mut highlights = Vec::new();

        for edit in compute_rename_edits(text, &token, &token) {
            highlights.push(DocumentHighlight {
                range: edit.range,
                kind: Some(DocumentHighlightKind::TEXT),
            });
        }

        highlights
    }

    // ── Publishing diagnostics ──

    fn publish_diagnostics(
        &self,
        connection: &Connection,
        uri: &str,
        diagnostics: Vec<Diagnostic>,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let url = parse_uri(uri).ok_or_else(|| format!("invalid URI: {uri}"))?;
        let params = PublishDiagnosticsParams {
            uri: url,
            diagnostics,
            version: None,
        };
        let not = Notification {
            method: "textDocument/publishDiagnostics".to_string(),
            params: serde_json::to_value(&params)?,
        };
        connection.sender.send(Message::Notification(not))?;
        Ok(())
    }

    // ── Response helpers ──

    fn send_response(
        &self,
        connection: &Connection,
        id: RequestId,
        result: impl serde::Serialize,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let resp = Response {
            id,
            result: Some(serde_json::to_value(&result)?),
            error: None,
        };
        connection.sender.send(Message::Response(resp))?;
        Ok(())
    }

    fn send_error(
        &self,
        connection: &Connection,
        id: RequestId,
        code: i32,
        message: String,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let resp = Response {
            id,
            result: None,
            error: Some(ResponseError {
                code,
                message,
                data: None,
            }),
        };
        connection.sender.send(Message::Response(resp))?;
        Ok(())
    }

    // ── Main loop ──

    fn run(&mut self, connection: Connection) -> Result<(), Box<dyn Error + Sync + Send>> {
        let server_capabilities = serde_json::json!({
            "textDocumentSync": { "openClose": true, "change": 1 },
            "completionProvider": {
                "resolveProvider": false,
                "triggerCharacters": ["."]
            },
            "hoverProvider": true,
            "definitionProvider": true,
            "documentSymbolProvider": true,
            "signatureHelp": {
                "triggerCharacters": ["(", ","]
            },
            "renameProvider": { "prepareProvider": false },
            "workspaceSymbolProvider": true,
            "referencesProvider": true,
            "codeActionProvider": {
                "codeActionKinds": ["quickfix"]
            },
            "foldingRangeProvider": true,
            "semanticTokensProvider": {
                "legend": {
                    "tokenTypes": ["function", "keyword", "number", "string", "variable"],
                    "tokenModifiers": []
                },
                "full": true
            },
            "documentHighlightProvider": true
        });

        let _init_params = connection.initialize(server_capabilities)?;

        // Main message loop
        for msg in &connection.receiver {
            match msg {
                Message::Request(req) => {
                    if connection.handle_shutdown(&req)? {
                        return Ok(());
                    }
                    self.handle_request(&connection, req)?;
                }
                Message::Notification(not) => {
                    self.handle_notification(&connection, not)?;
                }
                Message::Response(_) => {}
            }
        }

        Ok(())
    }

    fn handle_request(
        &self,
        connection: &Connection,
        req: Request,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let id = req.id.clone();

        match req.method.as_str() {
            "textDocument/completion" => {
                let params: CompletionParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        self.send_error(connection, id, ErrorCode::InvalidParams as i32, format!("invalid completion params: {e}"))?;
                        return Ok(());
                    }
                };
                let uri = params.text_document_position.text_document.uri.to_string();
                let pos = params.text_document_position.position;
                let items = self.get_completions(&uri, pos);
                self.send_response(connection, id, CompletionResponse::Array(items))?;
            }
            "textDocument/hover" => {
                let params: HoverParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        self.send_error(connection, id, ErrorCode::InvalidParams as i32, format!("invalid hover params: {e}"))?;
                        return Ok(());
                    }
                };
                let uri = params.text_document_position_params.text_document.uri.to_string();
                let pos = params.text_document_position_params.position;
                let result = self.get_hover(&uri, pos);
                self.send_response(connection, id, result)?;
            }
            "textDocument/definition" => {
                let params: GotoDefinitionParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        self.send_error(connection, id, ErrorCode::InvalidParams as i32, format!("invalid definition params: {e}"))?;
                        return Ok(());
                    }
                };
                let uri = params.text_document_position_params.text_document.uri.to_string();
                let pos = params.text_document_position_params.position;
                let result = self.get_definition(&uri, pos);
                self.send_response(connection, id, result)?;
            }
            "textDocument/documentSymbol" => {
                let params: DocumentSymbolParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        self.send_error(connection, id, ErrorCode::InvalidParams as i32, format!("invalid documentSymbol params: {e}"))?;
                        return Ok(());
                    }
                };
                let uri = params.text_document.uri.to_string();
                let symbols = self.get_document_symbols(&uri);
                self.send_response(connection, id, OneOf::<Vec<DocumentSymbol>, Vec<SymbolInformation>>::Left(symbols))?;
            }
            "textDocument/signatureHelp" => {
                let params: SignatureHelpParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        self.send_error(connection, id, ErrorCode::InvalidParams as i32, format!("invalid signatureHelp params: {e}"))?;
                        return Ok(());
                    }
                };
                let uri = params.text_document_position_params.text_document.uri.to_string();
                let pos = params.text_document_position_params.position;
                let result = self.get_signature_help(&uri, pos);
                self.send_response(connection, id, result)?;
            }
            "textDocument/rename" => {
                let params: RenameParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        self.send_error(connection, id, ErrorCode::InvalidParams as i32, format!("invalid rename params: {e}"))?;
                        return Ok(());
                    }
                };
                let uri = params.text_document_position.text_document.uri.to_string();
                let pos = params.text_document_position.position;
                let new_name = params.new_name;
                match self.get_rename(&uri, pos, &new_name) {
                    Some(edit) => self.send_response(connection, id, edit)?,
                    None => self.send_error(connection, id, ErrorCode::InvalidParams as i32, "No symbol found at position".to_string())?,
                }
            }
            "workspace/symbol" => {
                let params: WorkspaceSymbolParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        self.send_error(connection, id, ErrorCode::InvalidParams as i32, format!("invalid workspaceSymbol params: {e}"))?;
                        return Ok(());
                    }
                };
                let symbols = self.get_workspace_symbols(&params.query);
                self.send_response(connection, id, symbols)?;
            }
            "textDocument/references" => {
                let params: ReferenceParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        self.send_error(connection, id, ErrorCode::InvalidParams as i32, format!("invalid references params: {e}"))?;
                        return Ok(());
                    }
                };
                let uri = params.text_document_position.text_document.uri.to_string();
                let pos = params.text_document_position.position;
                let locations = self.get_references(&uri, pos);
                self.send_response(connection, id, locations)?;
            }
            "textDocument/codeAction" => {
                let params: CodeActionParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        self.send_error(connection, id, ErrorCode::InvalidParams as i32, format!("invalid codeAction params: {e}"))?;
                        return Ok(());
                    }
                };
                let uri = params.text_document.uri.to_string();
                let range = params.range;
                let actions = self.get_code_actions(&uri, range, params);
                self.send_response(connection, id, actions)?;
            }
            "textDocument/foldingRange" => {
                let params: FoldingRangeParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        self.send_error(connection, id, ErrorCode::InvalidParams as i32, format!("invalid foldingRange params: {e}"))?;
                        return Ok(());
                    }
                };
                let uri = params.text_document.uri.to_string();
                let text = match self.documents.get(&uri) {
                    Some(t) => t.clone(),
                    None => {
                        self.send_error(connection, id, ErrorCode::InvalidParams as i32, "Document not found".to_string())?;
                        return Ok(());
                    }
                };
                let ranges = self.get_folding_ranges(&text);
                self.send_response(connection, id, ranges)?;
            }
            "textDocument/semanticTokens/full" => {
                let params: SemanticTokensParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        self.send_error(connection, id, ErrorCode::InvalidParams as i32, format!("invalid semanticTokens params: {e}"))?;
                        return Ok(());
                    }
                };
                let uri = params.text_document.uri.to_string();
                let text = match self.documents.get(&uri) {
                    Some(t) => t.clone(),
                    None => {
                        self.send_error(connection, id, ErrorCode::InvalidParams as i32, "Document not found".to_string())?;
                        return Ok(());
                    }
                };
                match self.get_semantic_tokens(&text) {
                    Some(tokens) => {
                        let result = SemanticTokens {
                            result_id: None,
                            data: tokens,
                        };
                        self.send_response(connection, id, SemanticTokensResult::Tokens(result))?;
                    }
                    None => {
                        let result = SemanticTokens {
                            result_id: None,
                            data: vec![],
                        };
                        self.send_response(connection, id, SemanticTokensResult::Tokens(result))?;
                    }
                }
            }
            "textDocument/documentHighlight" => {
                let params: DocumentHighlightParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        self.send_error(connection, id, ErrorCode::InvalidParams as i32, format!("invalid documentHighlight params: {e}"))?;
                        return Ok(());
                    }
                };
                let uri = params.text_document_position_params.text_document.uri.to_string();
                let pos = params.text_document_position_params.position;
                let highlights = self.get_document_highlights(&uri, pos);
                self.send_response(connection, id, highlights)?;
            }
            _ => {
                // Unknown request method — silently ignore
            }
        }
        Ok(())
    }

    fn handle_notification(
        &mut self,
        connection: &Connection,
        not: Notification,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        match not.method.as_str() {
            "textDocument/didOpen" => {
                let params: DidOpenTextDocumentParams = serde_json::from_value(not.params)?;
                let uri = params.text_document.uri.to_string();
                let text = params.text_document.text;
                self.open_document(&uri, &text);
                let diags = self.compute_diagnostics(&text);
                self.publish_diagnostics(connection, &uri, diags)?;
            }
            "textDocument/didChange" => {
                let params: DidChangeTextDocumentParams = serde_json::from_value(not.params)?;
                let uri = params.text_document.uri.to_string();
                if let Some(change) = params.content_changes.into_iter().last() {
                    let text = change.text;
                    self.update_document(&uri, &text);
                    let diags = self.compute_diagnostics(&text);
                    self.publish_diagnostics(connection, &uri, diags)?;
                }
            }
            "textDocument/didClose" => {
                let params: DidCloseTextDocumentParams = serde_json::from_value(not.params)?;
                let uri = params.text_document.uri.to_string();
                self.close_document(&uri);
                self.publish_diagnostics(connection, &uri, vec![])?;
            }
            "textDocument/didSave" => {
                // Diagnostics are already published on change
            }
            _ => {}
        }
        Ok(())
    }
}

// ── AST-based symbol walking ──

/// Walk an expression tree and collect symbol definitions.
fn walk_expr_for_symbols(
    expr: &Expr,
    uri: &str,
    tokens: &[SpannedToken],
    defs: &mut Vec<SymbolDef>,
) {
    match expr {
        // Variable assignment: x = value
        Expr::Assign { lhs, .. } => {
            if let Expr::Symbol(name) = lhs.as_ref() {
                let span = find_token_span(tokens, &Token::Ident(name.clone()));
                let range = span.map(|s| span_to_range_with_length(&s, name.len())).unwrap_or_default();
                defs.push(SymbolDef {
                    name: name.clone(),
                    uri: uri.to_string(),
                    range,
                    kind: SymbolKind::VARIABLE,
                    detail: format!("{} = ...", name),
                    signature: None,
                });
            }
        }

        // Function definition: f[x_, y_] := body
        Expr::FuncDef { name, params, .. } => {
            let span = find_token_span(tokens, &Token::Ident(name.clone()));
            let range = span.map(|s| span_to_range_with_length(&s, name.len())).unwrap_or_default();

            let param_names: Vec<String> = params.iter().map(|p| extract_param_name(p)).collect();
            let signature = if !param_names.is_empty() {
                Some(param_names.join(", "))
            } else {
                None
            };

            defs.push(SymbolDef {
                name: name.clone(),
                uri: uri.to_string(),
                range,
                kind: SymbolKind::FUNCTION,
                detail: format!("{}[...] := ...", name),
                signature,
            });
        }

        // Class definition
        Expr::ClassDef { name, .. } => {
            let span = find_token_span(tokens, &Token::Ident(name.clone()));
            let range = span.map(|s| span_to_range_with_length(&s, name.len())).unwrap_or_default();
            defs.push(SymbolDef {
                name: name.clone(),
                uri: uri.to_string(),
                range,
                kind: SymbolKind::CLASS,
                detail: format!("class {}", name),
                signature: None,
            });
        }

        // Module definition
        Expr::ModuleDef { name, .. } => {
            let span = find_token_span(tokens, &Token::Ident(name.clone()));
            let range = span.map(|s| span_to_range_with_length(&s, name.len())).unwrap_or_default();
            defs.push(SymbolDef {
                name: name.clone(),
                uri: uri.to_string(),
                range,
                kind: SymbolKind::MODULE,
                detail: format!("module {}", name),
                signature: None,
            });
        }

        // Destructuring assignment: {a, b} = value
        Expr::DestructAssign { patterns, .. } => {
            for pat in patterns {
                if let Expr::Symbol(name) = pat {
                    let span = find_token_span(tokens, &Token::Ident(name.clone()));
                    let range = span.map(|s| span_to_range_with_length(&s, name.len())).unwrap_or_default();
                    defs.push(SymbolDef {
                        name: name.clone(),
                        uri: uri.to_string(),
                        range,
                        kind: SymbolKind::VARIABLE,
                        detail: format!("{} = ...", name),
                        signature: None,
                    });
                }
            }
        }

        // Recurse into sequences and compound expressions
        Expr::Sequence(items) | Expr::List(items) => {
            for item in items {
                walk_expr_for_symbols(item, uri, tokens, defs);
            }
        }

        _ => {}
    }
}

/// Extract a human-readable parameter name from a pattern expression.
fn extract_param_name(expr: &Expr) -> String {
    match expr {
        Expr::NamedBlank { name, type_constraint } => {
            if let Some(tc) = type_constraint {
                format!("{}_{}", name, tc)
            } else {
                format!("{}_", name)
            }
        }
        Expr::Blank { type_constraint } => {
            if let Some(tc) = type_constraint {
                format!("_{}", tc)
            } else {
                "_".to_string()
            }
        }
        Expr::BlankSequence { name, type_constraint } => {
            let n = name.as_deref().unwrap_or("_");
            if let Some(tc) = type_constraint {
                format!("{}__{}", n, tc)
            } else {
                format!("{}__", n)
            }
        }
        Expr::BlankNullSequence { name, type_constraint } => {
            let n = name.as_deref().unwrap_or("_");
            if let Some(tc) = type_constraint {
                format!("{}___{}", n, tc)
            } else {
                format!("{}___", n)
            }
        }
        Expr::OptionalNamedBlank { name, type_constraint, .. } => {
            if let Some(tc) = type_constraint {
                format!("{}_{tc}.", name)
            } else {
                format!("{}_.", name)
            }
        }
        Expr::OptionalBlank { type_constraint, .. } => {
            if let Some(tc) = type_constraint {
                format!("_{}.", tc)
            } else {
                "_.".to_string()
            }
        }
        Expr::Symbol(name) => name.clone(),
        _ => expr.to_string(),
    }
}

/// Find the first token span matching a given token.
fn find_token_span(tokens: &[SpannedToken], target: &Token) -> Option<Span> {
    tokens.iter().find(|t| &t.token == target).map(|t| t.span.clone())
}

// ── Rename helpers ──

/// Compute text edits to rename all whole-word occurrences of `old_name` to `new_name`.
fn compute_rename_edits(text: &str, old_name: &str, new_name: &str) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    let old_len = old_name.len();

    for (byte_idx, line) in text.lines().enumerate() {
        for (char_idx, _) in line.match_indices(old_name) {
            // Check word boundaries
            let before_ok = if char_idx == 0 {
                true
            } else {
                let before_char = line[..char_idx].chars().last().unwrap();
                !before_char.is_alphanumeric() && before_char != '_' && before_char != '$'
            };
            let after_pos = char_idx + old_len;
            let after_ok = if after_pos >= line.len() {
                true
            } else {
                let after_char = line[after_pos..].chars().next().unwrap();
                !after_char.is_alphanumeric() && after_char != '_' && after_char != '$'
            };

            if before_ok && after_ok {
                let char_col = line[..char_idx].chars().count() as u32;
                edits.push(TextEdit {
                    range: Range {
                        start: Position {
                            line: byte_idx as u32,
                            character: char_col,
                        },
                        end: Position {
                            line: byte_idx as u32,
                            character: (char_col + old_name.chars().count() as u32),
                        },
                    },
                    new_text: new_name.to_string(),
                });
            }
        }
    }

    edits
}

// ── Folding range helpers ──

/// Convert a character index in text to (line, character) in LSP 0-based format.
fn char_index_to_line_offset(text: &str, char_idx: usize) -> Position {
    let mut line = 0u32;
    let mut char_in_line = 0u32;
    let mut chars_consumed = 0;

    for c in text.chars() {
        if chars_consumed >= char_idx {
            break;
        }
        if c == '\n' {
            line += 1;
            char_in_line = 0;
        } else {
            char_in_line += 1;
        }
        chars_consumed += 1;
    }

    Position {
        line,
        character: char_in_line,
    }
}

fn add_bracket_folding(
    ranges: &mut Vec<FoldingRange>,
    text: &str,
    open_idx: usize,
    close_idx: usize,
    kind: FoldingRangeKind,
) {
    let start_pos = char_index_to_line_offset(text, open_idx);
    let end_pos = char_index_to_line_offset(text, close_idx);

    if end_pos.line > start_pos.line || (end_pos.line == start_pos.line && close_idx - open_idx > 4) {
        ranges.push(FoldingRange {
            start_line: start_pos.line,
            start_character: Some(start_pos.character),
            end_line: end_pos.line,
            end_character: Some(end_pos.character),
            kind: Some(kind),
            ..Default::default()
        });
    }
}

// ── Semantic token helpers ──

/// Internal representation of a semantic token before delta encoding.
struct SemanticTokenRaw {
    delta_line: u32,
    delta_start: u32,
    length: u32,
    token_type: u32,
    modifier: u32,
}

/// Get the display length of a token (in characters) for semantic token ranges.
fn token_display_length(token: &Token) -> usize {
    match token {
        Token::Ident(s) | Token::Integer(s) | Token::Real(s) | Token::Str(s) | Token::Operator(s) => s.len(),
        Token::SlotN(n) => format!("#{}", n).len(),
        Token::SlotSequenceN(n) => format!("##{}", n).len(),
        Token::LBrace | Token::RBrace | Token::LBracket | Token::RBracket
        | Token::LParen | Token::RParen | Token::Plus | Token::Minus | Token::Star
        | Token::Slash | Token::Caret | Token::Dot | Token::Comma | Token::Semicolon
        | Token::Colon | Token::Less | Token::Greater | Token::Not | Token::Pipe
        | Token::PipeAlt | Token::QuestionMark | Token::Quote | Token::Tilde
        | Token::FuncRef | Token::At | Token::Unset | Token::Slot => 1,
        Token::LAssoc => 2,        // <|
        Token::RAssoc => 2,        // |>
        Token::LDoubleBracket => 2, // [[
        Token::RDoubleBracket => 2, // ]]
        Token::Assign | Token::Rule | Token::Equal | Token::Unequal | Token::FatArrow => 2,
        Token::DelayedAssign | Token::GreaterEqual | Token::LessEqual => 2,
        Token::DelayedRule | Token::ReplaceAll | Token::And | Token::Or | Token::StarStar
        | Token::Increment | Token::Decrement | Token::MapOp | Token::ApplyOp | Token::AtTransform => 2,
        Token::ReplaceRepeated => 3,  // //.
        Token::StringJoinOp => 2,     // <>
        Token::PlusAssign | Token::MinusAssign | Token::StarAssign | Token::SlashAssign | Token::CaretAssign => 2,
        Token::SlotSequence => 2,     // ##
        Token::ColonColon => 2,       // ::
        Token::ColonSlashSemicolon => 2, // /;
        // Keywords
        Token::If | Token::For | Token::Try | Token::As | Token::Do | Token::Catch | Token::Mixin | Token::Def => 2,
        Token::Else | Token::With | Token::RuleKw | Token::Hold => 4,
        Token::Match | Token::Throw | Token::While | Token::Which | Token::Field => 5,
        Token::Switch | Token::Extends | Token::Import | Token::Export | Token::Finally => 6,
        Token::Method | Token::Module | Token::Class | Token::Function | Token::Constructor => 6,
        Token::HoldComplete => 12,
        Token::ReleaseHold => 13,
        Token::True => 4,
        Token::False => 5,
        Token::Null => 4,
        Token::Newline | Token::Eof => 0,
    }
}

// ── Main ──

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    eprintln!("syma-lsp: starting");

    let (connection, io_threads) = Connection::stdio();

    let mut server = Backend::new();
    server.run(connection)?;

    io_threads
        .join()
        .map_err(|e| format!("io_threads join error: {e:?}"))?;
    eprintln!("syma-lsp: shutdown complete");
    Ok(())
}
