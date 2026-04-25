/// Syma LSP server — provides IDE features for the Syma language.
///
/// Features:
/// - Diagnostics (lexer/parser errors)
/// - Completions (builtins + user symbols)
/// - Hover (symbol info)
/// - Go-to-definition
/// - Document symbols (outline)
use std::collections::HashMap;
use std::error::Error;

use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::*;

use syma::kernel::SymaKernel;
use syma::lexer::{Span, Token};

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

/// A definition site for a symbol.
#[derive(Debug, Clone)]
struct SymbolDef {
    name: String,
    uri: String,
    range: Range,
    kind: SymbolKind,
    detail: String,
}

/// The LSP server backend.
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

        // Lexer errors
        match syma::lexer::tokenize(text) {
            Ok(tokens) => {
                // Parser errors
                let mut parser = syma::parser::Parser::new(tokens);
                if let Err(e) = parser.parse_program() {
                    let range = e
                        .span
                        .as_ref()
                        .map(syma_span_to_lsp_range)
                        .unwrap_or_default();
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

    // ── Symbol scanning ──

    fn scan_document(&mut self, uri: &str, text: &str) {
        // Remove old defs for this URI
        self.symbol_defs.retain(|d| d.uri != uri);

        // Tokenize and scan for definition patterns
        let tokens = match syma::lexer::tokenize(text) {
            Ok(t) => t,
            Err(_) => return,
        };

        let mut i = 0;
        while i < tokens.len() {
            let tok = &tokens[i];
            match &tok.token {
                Token::Ident(name) if i + 1 < tokens.len() => {
                    let next = &tokens[i + 1].token;
                    match next {
                        // `Ident = expr` — variable assignment
                        Token::Assign => {
                            let range = syma_span_to_lsp_range(&tok.span);
                            self.symbol_defs.push(SymbolDef {
                                name: name.clone(),
                                uri: uri.to_string(),
                                range,
                                kind: SymbolKind::VARIABLE,
                                detail: format!("{} = ...", name),
                            });
                            // Skip past this token pair
                            i += 2;
                            continue;
                        }
                        // `Ident[...] := ...` or `Ident[...] = ...` — function definition
                        // These can also be `Ident := ...` for simple definitions
                        Token::DelayedAssign => {
                            let range = syma_span_to_lsp_range(&tok.span);
                            self.symbol_defs.push(SymbolDef {
                                name: name.clone(),
                                uri: uri.to_string(),
                                range,
                                kind: SymbolKind::FUNCTION,
                                detail: format!("{} := ...", name),
                            });
                            i += 2;
                            continue;
                        }
                        _ => {}
                    }
                }
                // `class Ident ...` — class definition
                Token::Class if i + 1 < tokens.len() => {
                    if let Token::Ident(name) = &tokens[i + 1].token {
                        let range = syma_span_to_lsp_range(&tokens[i + 1].span);
                        self.symbol_defs.push(SymbolDef {
                            name: name.clone(),
                            uri: uri.to_string(),
                            range,
                            kind: SymbolKind::CLASS,
                            detail: format!("class {}", name),
                        });
                        i += 2;
                        continue;
                    }
                }
                // `module Ident ...` — module definition
                Token::Module if i + 1 < tokens.len() => {
                    if let Token::Ident(name) = &tokens[i + 1].token {
                        let range = syma_span_to_lsp_range(&tokens[i + 1].span);
                        self.symbol_defs.push(SymbolDef {
                            name: name.clone(),
                            uri: uri.to_string(),
                            range,
                            kind: SymbolKind::MODULE,
                            detail: format!("module {}", name),
                        });
                        i += 2;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

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

    /// Parse a URI string into a proper URL.
    fn parse_uri(uri: &str) -> Option<Url> {
        Url::parse(uri)
            .or_else(|_| Url::parse(&format!("file://{}", uri)))
            .ok()
    }

    // ── Completions ──

    fn get_completions(&self, _uri: &str, _pos: Position) -> Vec<CompletionItem> {
        let mut items: Vec<CompletionItem> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // Built-in symbols from the kernel's environment
        let env = self.kernel.env();
        let bindings = env.all_bindings();
        for (name, _val) in &bindings {
            if seen.insert(name.clone()) {
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
        }

        // User-defined symbols from document scanning
        for def in &self.symbol_defs {
            if seen.insert(def.name.clone()) {
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
        }

        items
    }

    // ── Hover ──

    fn get_hover(&self, uri: &str, pos: Position) -> Option<Hover> {
        let text = self.documents.get(uri)?;
        let token = Self::token_at_position(text, pos)?;

        // Try to find the symbol in the kernel environment
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
                let contents = format!("```syma\n{}\n```", def.detail);
                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: contents,
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
                // Skip if this is the definition site itself
                if def.uri == uri && def.range.start == pos {
                    continue;
                }
                let url = Self::parse_uri(&def.uri)?;
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
            .map(|d| DocumentSymbol {
                name: d.name.clone(),
                detail: Some(d.detail.clone()),
                kind: d.kind,
                range: d.range,
                selection_range: d.range,
                children: None,
                tags: None,
                deprecated: None,
            })
            .collect()
    }

    // ── Publishing diagnostics ──

    fn publish_diagnostics(
        &self,
        connection: &Connection,
        uri: &str,
        diagnostics: Vec<Diagnostic>,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let url = Self::parse_uri(uri)
            .ok_or_else(|| format!("invalid URI: {uri}"))?;
        let params = PublishDiagnosticsParams {
            uri: url,
            diagnostics,
            version: None,
        };
        let not = Notification {
            method: "textDocument/publishDiagnostics".to_string(),
            params: serde_json::to_value(&params)?,
        };
        connection
            .sender
            .send(Message::Notification(not))?;
        Ok(())
    }

    // ── Main loop ──

    fn run(&mut self, connection: Connection) -> Result<(), Box<dyn Error + Sync + Send>> {
        let server_capabilities = ServerCapabilities {
            text_document_sync: Some(TextDocumentSyncCapability::Kind(
                TextDocumentSyncKind::FULL,
            )),
            completion_provider: Some(CompletionOptions {
                trigger_characters: Some(vec![".".to_string()]),
                ..Default::default()
            }),
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            definition_provider: Some(OneOf::Left(true)),
            document_symbol_provider: Some(OneOf::Left(true)),
            ..Default::default()
        };

        let capabilities_value = serde_json::to_value(&server_capabilities)?;
        let _init_params = connection.initialize(capabilities_value)?;

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
        match req.method.as_str() {
            "textDocument/completion" => {
                let params: CompletionParams = serde_json::from_value(req.params)?;
                let uri = params.text_document_position.text_document.uri.to_string();
                let pos = params.text_document_position.position;
                let items = self.get_completions(&uri, pos);
                let result = CompletionResponse::Array(items);
                let resp = Response {
                    id: req.id,
                    result: Some(serde_json::to_value(&result)?),
                    error: None,
                };
                connection.sender.send(Message::Response(resp))?;
            }
            "textDocument/hover" => {
                let params: HoverParams = serde_json::from_value(req.params)?;
                let uri = params.text_document_position_params.text_document.uri.to_string();
                let pos = params.text_document_position_params.position;
                let result = self.get_hover(&uri, pos);
                let resp = Response {
                    id: req.id,
                    result: Some(serde_json::to_value(&result)?),
                    error: None,
                };
                connection.sender.send(Message::Response(resp))?;
            }
            "textDocument/definition" => {
                let params: GotoDefinitionParams = serde_json::from_value(req.params)?;
                let uri = params.text_document_position_params.text_document.uri.to_string();
                let pos = params.text_document_position_params.position;
                let result = self.get_definition(&uri, pos);
                let resp = Response {
                    id: req.id,
                    result: Some(serde_json::to_value(&result)?),
                    error: None,
                };
                connection.sender.send(Message::Response(resp))?;
            }
            "textDocument/documentSymbol" => {
                let params: DocumentSymbolParams = serde_json::from_value(req.params)?;
                let uri = params.text_document.uri.to_string();
                let symbols = self.get_document_symbols(&uri);
                let resp = Response {
                    id: req.id,
                    result: Some(serde_json::to_value(&symbols)?),
                    error: None,
                };
                connection.sender.send(Message::Response(resp))?;
            }
            _ => {
                // Unknown request — silently ignore (ProtocolError is not constructable
                // from outside lsp-server, so we skip sending an error for now)
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
                self.close_document(&params.text_document.uri.to_string());
            }
            "textDocument/didSave" => {
                // Diagnostics are already published on change
            }
            _ => {}
        }
        Ok(())
    }
}

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
