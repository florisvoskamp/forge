//! Multi-language extraction via tree-sitter **tags queries** (`tags.scm`) — the same mechanism
//! GitHub code-nav uses. A source file + its detected language yields symbol *definitions*
//! (graph nodes) and *references* (call/use sites → graph edges), uniformly across every language
//! whose grammar ships a tags query. Pure: no I/O, no store; the caller reads files and persists.
//!
//! Adding a language is one row in [`LANGS`] (grammar `Language` + its bundled `TAGS_QUERY` +
//! file extensions). The category strings come from the grammar's own `@definition.*` /
//! `@reference.*` captures, so we stay grammar-driven rather than hand-classifying per language.

use std::collections::HashMap;
use tree_sitter_tags::{TagsConfiguration, TagsContext};

// Hand-crafted tags queries for grammars that ship a tags.scm but do not export it as a const,
// or that have no tags.scm at all. Validated against the grammar's node-types.json.

// The bundled tree-sitter-c-sharp TAGS_QUERY contains a bare `@module` capture which is not a
// valid tags capture name (tree-sitter-tags only accepts `@definition.*` / `@reference.*` /
// `@name` / `@doc`). Use a fixed version that replaces `@module` with `@definition.module`.
const CSHARP_TAGS_QUERY: &str = r#"
(class_declaration name: (identifier) @name) @definition.class
(class_declaration (base_list (_) @name)) @reference.class
(interface_declaration name: (identifier) @name) @definition.interface
(interface_declaration (base_list (_) @name)) @reference.interface
(method_declaration name: (identifier) @name) @definition.method
(object_creation_expression type: (identifier) @name) @reference.class
(type_parameter_constraints_clause (identifier) @name) @reference.class
(type_parameter_constraint (type type: (identifier) @name)) @reference.class
(variable_declaration type: (identifier) @name) @reference.class
(invocation_expression function: (member_access_expression name: (identifier) @name)) @reference.send
(namespace_declaration name: (identifier) @name) @definition.module
"#;

const BASH_TAGS_QUERY: &str = r#"
(function_definition
  name: (word) @name) @definition.function
"#;

const HASKELL_TAGS_QUERY: &str = r#"
(function
  name: (variable) @name) @definition.function
(signature
  name: (variable) @name) @definition.type
"#;

const SCALA_TAGS_QUERY: &str = r#"
(package_clause
  name: (package_identifier) @name) @definition.module
(trait_definition
  name: (identifier) @name) @definition.interface
(enum_definition
  name: (identifier) @name) @definition.enum
(class_definition
  name: (identifier) @name) @definition.class
(object_definition
  name: (identifier) @name) @definition.object
(function_definition
  name: (identifier) @name) @definition.function
(val_definition
  pattern: (identifier) @name) @definition.variable
(type_definition
  name: (type_identifier) @name) @definition.type
(call_expression
  (identifier) @name) @reference.call
"#;

const KOTLIN_TAGS_QUERY: &str = r#"
(function_declaration
  name: (identifier) @name) @definition.function
(class_declaration
  name: (identifier) @name) @definition.class
(object_declaration
  name: (identifier) @name) @definition.object
"#;

/// A symbol definition pulled from a tags query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    /// Normalized tags category: `function|method|class|struct|enum|trait|interface|module|
    /// constant|type|constructor|macro|union|field` (grammar-driven; unknowns kept verbatim).
    pub kind: String,
    pub name: String,
    /// Enclosing-definition chain joined by `.` (e.g. `Session.run_turn`), this symbol included.
    pub qualname: String,
    pub signature: Option<String>,
    pub span_start: usize,
    pub span_end: usize,
    pub line_start: u32,
    /// Index into the returned `defs` of the nearest enclosing definition (a `contains` edge);
    /// `None` for a top-level item. Derived from span nesting, so it is language-agnostic.
    pub parent: Option<usize>,
}

/// A reference / call site: an identifier use that is not itself a definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub name: String,
    /// Reference category, e.g. `call`, `type`, `module`, `implementation`.
    pub kind: String,
    pub line: u32,
    /// Index into `defs` of the definition whose span encloses this reference (the edge source);
    /// `None` if the reference sits at file scope outside any indexed definition.
    pub from: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
}

struct LangEntry {
    name: &'static str,
    config: TagsConfiguration,
}

struct Registry {
    entries: Vec<LangEntry>,
    by_ext: HashMap<&'static str, usize>,
}

fn lang(language: tree_sitter::Language, tags: &str) -> Option<TagsConfiguration> {
    TagsConfiguration::new(language, tags, "").ok()
}

thread_local! {
    // `TagsConfiguration` is not `Sync`, so the compiled-query registry is cached per thread
    // rather than in a global static. Build cost (query compilation) is paid once per thread.
    static REGISTRY: Registry = build_registry();
}

/// Build every supported language's [`TagsConfiguration`]. A grammar whose query fails to compile
/// is skipped (logged) rather than panicking — degrade, don't crash.
fn build_registry() -> Registry {
    {
        // (name, build TagsConfiguration, file extensions)
        let specs: Vec<(
            &'static str,
            Option<TagsConfiguration>,
            &'static [&'static str],
        )> = vec![
            (
                "rust",
                lang(
                    tree_sitter_rust::LANGUAGE.into(),
                    tree_sitter_rust::TAGS_QUERY,
                ),
                &["rs"],
            ),
            (
                "python",
                lang(
                    tree_sitter_python::LANGUAGE.into(),
                    tree_sitter_python::TAGS_QUERY,
                ),
                &["py", "pyi"],
            ),
            (
                "javascript",
                lang(
                    tree_sitter_javascript::LANGUAGE.into(),
                    tree_sitter_javascript::TAGS_QUERY,
                ),
                &["js", "jsx", "mjs", "cjs"],
            ),
            (
                "typescript",
                lang(
                    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                    tree_sitter_typescript::TAGS_QUERY,
                ),
                &["ts", "mts", "cts"],
            ),
            (
                "tsx",
                lang(
                    tree_sitter_typescript::LANGUAGE_TSX.into(),
                    tree_sitter_typescript::TAGS_QUERY,
                ),
                &["tsx"],
            ),
            (
                "go",
                lang(tree_sitter_go::LANGUAGE.into(), tree_sitter_go::TAGS_QUERY),
                &["go"],
            ),
            (
                "java",
                lang(
                    tree_sitter_java::LANGUAGE.into(),
                    tree_sitter_java::TAGS_QUERY,
                ),
                &["java"],
            ),
            (
                "c",
                lang(tree_sitter_c::LANGUAGE.into(), tree_sitter_c::TAGS_QUERY),
                &["c", "h"],
            ),
            (
                "cpp",
                lang(
                    tree_sitter_cpp::LANGUAGE.into(),
                    tree_sitter_cpp::TAGS_QUERY,
                ),
                &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
            ),
            (
                "ruby",
                lang(
                    tree_sitter_ruby::LANGUAGE.into(),
                    tree_sitter_ruby::TAGS_QUERY,
                ),
                &["rb"],
            ),
            (
                "csharp",
                lang(tree_sitter_c_sharp::LANGUAGE.into(), CSHARP_TAGS_QUERY),
                &["cs"],
            ),
            (
                "php",
                lang(
                    tree_sitter_php::LANGUAGE_PHP.into(),
                    tree_sitter_php::TAGS_QUERY,
                ),
                &["php"],
            ),
            (
                "elixir",
                lang(
                    tree_sitter_elixir::LANGUAGE.into(),
                    tree_sitter_elixir::TAGS_QUERY,
                ),
                &["ex", "exs"],
            ),
            (
                "lua",
                lang(
                    tree_sitter_lua::LANGUAGE.into(),
                    tree_sitter_lua::TAGS_QUERY,
                ),
                &["lua"],
            ),
            (
                "ocaml",
                lang(
                    tree_sitter_ocaml::LANGUAGE_OCAML.into(),
                    tree_sitter_ocaml::TAGS_QUERY,
                ),
                &["ml", "mli"],
            ),
            (
                "bash",
                lang(tree_sitter_bash::LANGUAGE.into(), BASH_TAGS_QUERY),
                &["sh", "bash"],
            ),
            (
                "haskell",
                lang(tree_sitter_haskell::LANGUAGE.into(), HASKELL_TAGS_QUERY),
                &["hs"],
            ),
            (
                "scala",
                lang(tree_sitter_scala::LANGUAGE.into(), SCALA_TAGS_QUERY),
                &["scala"],
            ),
            (
                "kotlin",
                lang(tree_sitter_kotlin_ng::LANGUAGE.into(), KOTLIN_TAGS_QUERY),
                &["kt", "kts"],
            ),
        ];
        let mut entries = Vec::new();
        let mut by_ext = HashMap::new();
        for (name, config, exts) in specs {
            let Some(config) = config else {
                tracing::warn!(language = name, "tags query failed to compile; skipping");
                continue;
            };
            let idx = entries.len();
            entries.push(LangEntry { name, config });
            for ext in exts {
                by_ext.insert(*ext, idx);
            }
        }
        Registry { entries, by_ext }
    }
}

/// The language name Lattice will record for a path, or `None` if unsupported.
pub fn lang_for_path(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?;
    REGISTRY.with(|reg| reg.by_ext.get(ext).map(|&i| reg.entries[i].name))
}

/// Every language name with a working grammar (for `status` / diagnostics).
pub fn supported_languages() -> Vec<&'static str> {
    REGISTRY.with(|reg| reg.entries.iter().map(|e| e.name).collect())
}

/// Extract definitions + references from `src`, choosing the grammar by `path`'s extension.
/// Returns an empty [`Parsed`] for unsupported languages or unparseable input — never errors.
pub fn extract(path: &str, src: &str) -> Parsed {
    let Some(ext) = path.rsplit('.').next() else {
        return Parsed::default();
    };
    REGISTRY.with(|reg| {
        let Some(&idx) = reg.by_ext.get(ext) else {
            return Parsed::default();
        };
        extract_with(&reg.entries[idx].config, src)
    })
}

fn extract_with(config: &TagsConfiguration, src: &str) -> Parsed {
    let mut ctx = TagsContext::new();
    let bytes = src.as_bytes();
    let Ok((tags, _had_error)) = ctx.generate_tags(config, bytes, None) else {
        return Parsed::default();
    };

    // Pass 1: collect raw tags (definitions and references) with byte spans + categories.
    struct RawDef {
        kind: String,
        name: String,
        span_start: usize,
        span_end: usize,
        line_start: u32,
    }
    struct RawRef {
        name: String,
        kind: String,
        pos: usize,
        line: u32,
    }
    let mut raw_defs: Vec<RawDef> = Vec::new();
    let mut raw_refs: Vec<RawRef> = Vec::new();

    for tag in tags {
        let Ok(tag) = tag else { continue };
        let Some(name) = src.get(tag.name_range.clone()) else {
            continue;
        };
        let name = name.to_string();
        let category = config.syntax_type_name(tag.syntax_type_id).to_string();
        let line = tag.span.start.row as u32 + 1;
        if tag.is_definition {
            raw_defs.push(RawDef {
                kind: category,
                name,
                span_start: tag.range.start,
                span_end: tag.range.end,
                line_start: line,
            });
        } else {
            raw_refs.push(RawRef {
                name,
                kind: category,
                pos: tag.name_range.start,
                line,
            });
        }
    }

    // Definitions are processed outer-to-inner so a parent is always already present when its
    // children are placed. Sort by start asc, then by end desc (wider span = outer = first).
    raw_defs.sort_by(|a, b| {
        a.span_start
            .cmp(&b.span_start)
            .then(b.span_end.cmp(&a.span_end))
    });

    let mut defs: Vec<Def> = Vec::with_capacity(raw_defs.len());
    for rd in &raw_defs {
        let parent = enclosing(&defs, rd.span_start, rd.span_end);
        let qualname = match parent {
            Some(p) => format!("{}.{}", defs[p].qualname, rd.name),
            None => rd.name.clone(),
        };
        defs.push(Def {
            kind: rd.kind.clone(),
            name: rd.name.clone(),
            qualname,
            signature: signature(src, rd.span_start, rd.span_end),
            span_start: rd.span_start,
            span_end: rd.span_end,
            line_start: rd.line_start,
            parent,
        });
    }

    let refs = raw_refs
        .into_iter()
        .map(|rr| Ref {
            from: enclosing_point(&defs, rr.pos),
            name: rr.name,
            kind: rr.kind,
            line: rr.line,
        })
        .collect();

    Parsed { defs, refs }
}

/// Index of the smallest already-placed definition strictly enclosing `[start, end)` (excluding an
/// identical span, so a def is never its own parent).
fn enclosing(defs: &[Def], start: usize, end: usize) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, d) in defs.iter().enumerate() {
        let encloses = d.span_start <= start
            && d.span_end >= end
            && (d.span_end - d.span_start) > (end - start);
        if encloses {
            match best {
                Some(b)
                    if (defs[b].span_end - defs[b].span_start) <= (d.span_end - d.span_start) => {}
                _ => best = Some(i),
            }
        }
    }
    best
}

/// Index of the smallest definition whose span contains byte offset `pos`.
fn enclosing_point(defs: &[Def], pos: usize) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, d) in defs.iter().enumerate() {
        if d.span_start <= pos && pos < d.span_end {
            match best {
                Some(b)
                    if (defs[b].span_end - defs[b].span_start) <= (d.span_end - d.span_start) => {}
                _ => best = Some(i),
            }
        }
    }
    best
}

/// A one-line signature: the definition's text up to the body delimiter, collapsed to one line.
fn signature(src: &str, start: usize, end: usize) -> Option<String> {
    let full = src.get(start..end)?;
    let head: String = full
        .chars()
        .take_while(|&c| c != '{' && c != '\n')
        .collect();
    let head = head.split_whitespace().collect::<Vec<_>>().join(" ");
    (!head.is_empty()).then_some(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def<'a>(p: &'a Parsed, name: &str) -> &'a Def {
        p.defs.iter().find(|d| d.name == name).expect("def present")
    }

    #[test]
    fn rust_definitions_and_nesting() {
        let src = r#"
pub struct Session { id: String }
impl Session {
    pub fn run_turn(&self, prompt: &str) -> String { helper() }
}
pub fn helper() -> String { String::new() }
"#;
        let p = extract("net.rs", src);
        // Categories are grammar-defined: Rust's tags.scm maps `struct_item` to `class`.
        assert_eq!(def(&p, "Session").kind, "class");
        let m = def(&p, "run_turn");
        assert!(!m.kind.is_empty());
        // Rust's tags.scm doesn't capture `impl` blocks as containers, so a method is a sibling
        // of its struct (no qualname nesting). Nesting is exercised via Python (classes contain
        // their methods lexically) in `python_class_nesting`.
        assert_eq!(m.qualname, "run_turn");
        assert!(def(&p, "helper").parent.is_none());
        // The call to helper() inside run_turn is a reference attributed to run_turn.
        let call = p
            .refs
            .iter()
            .find(|r| r.name == "helper")
            .expect("helper() call captured as a reference");
        let from = call.from.map(|i| p.defs[i].name.as_str());
        assert_eq!(from, Some("run_turn"), "ref attributed to enclosing def");
    }

    #[test]
    fn python_class_nesting() {
        // A Python class lexically contains its methods, so span-nesting gives a dotted qualname.
        let src = "class Greeter:\n    def hi(self):\n        pass\n";
        let p = extract("g.py", src);
        let hi = def(&p, "hi");
        assert_eq!(hi.qualname, "Greeter.hi", "method nests under class");
        assert!(hi.parent.is_some());
    }

    #[test]
    fn python_is_supported() {
        let src = "def greet(name):\n    return hello(name)\n\nclass Greeter:\n    def hi(self):\n        pass\n";
        let p = extract("g.py", src);
        assert!(p.defs.iter().any(|d| d.name == "greet"));
        assert!(p.defs.iter().any(|d| d.name == "Greeter"));
        assert!(p.refs.iter().any(|r| r.name == "hello"));
    }

    #[test]
    fn unsupported_or_empty_is_clean() {
        assert!(extract("notes.txt", "hello world").defs.is_empty());
        assert!(extract("x.rs", "").defs.is_empty());
        assert_eq!(lang_for_path("a.go"), Some("go"));
        assert_eq!(lang_for_path("a.unknownext"), None);
        assert_eq!(lang_for_path("a.cs"), Some("csharp"));
        assert_eq!(lang_for_path("a.php"), Some("php"));
        assert_eq!(lang_for_path("a.ex"), Some("elixir"));
        assert_eq!(lang_for_path("a.lua"), Some("lua"));
        assert_eq!(lang_for_path("a.ml"), Some("ocaml"));
        assert_eq!(lang_for_path("a.sh"), Some("bash"));
        assert_eq!(lang_for_path("a.hs"), Some("haskell"));
        assert_eq!(lang_for_path("a.scala"), Some("scala"));
        assert_eq!(lang_for_path("a.kt"), Some("kotlin"));
    }

    #[test]
    fn csharp_definitions() {
        let src = "public class Greeter { public void Hello() {} }";
        let p = extract("app.cs", src);
        assert!(p.defs.iter().any(|d| d.name == "Greeter"), "class captured");
    }

    #[test]
    fn php_definitions() {
        let src = "<?php\nfunction greet($name) { return $name; }\nclass Foo {}\n";
        let p = extract("app.php", src);
        assert!(
            p.defs.iter().any(|d| d.name == "greet" || d.name == "Foo"),
            "php def captured"
        );
    }

    #[test]
    fn elixir_definitions() {
        let src = "defmodule MyApp do\n  def hello(name) do\n    name\n  end\nend\n";
        let p = extract("app.ex", src);
        assert!(
            p.defs
                .iter()
                .any(|d| d.name == "hello" || d.name == "MyApp"),
            "elixir def captured"
        );
    }

    #[test]
    fn lua_definitions() {
        let src = "function greet(name)\n  return name\nend\n";
        let p = extract("app.lua", src);
        assert!(
            p.defs.iter().any(|d| d.name == "greet"),
            "lua function captured"
        );
    }

    #[test]
    fn ocaml_definitions() {
        let src = "let greet name = print_endline name\n";
        let p = extract("app.ml", src);
        assert!(
            p.defs.iter().any(|d| d.name == "greet"),
            "ocaml let binding captured"
        );
    }

    #[test]
    fn bash_definitions() {
        let src = "greet() {\n  echo hello\n}\n";
        let p = extract("script.sh", src);
        assert!(
            p.defs.iter().any(|d| d.name == "greet"),
            "bash function captured"
        );
    }

    #[test]
    fn haskell_definitions() {
        let src = "greet :: String -> String\ngreet name = name\n";
        let p = extract("app.hs", src);
        assert!(
            p.defs.iter().any(|d| d.name == "greet"),
            "haskell function captured"
        );
    }

    #[test]
    fn scala_definitions() {
        let src = "class Greeter {\n  def hello(): Unit = println(\"hi\")\n}\n";
        let p = extract("app.scala", src);
        assert!(
            p.defs
                .iter()
                .any(|d| d.name == "Greeter" || d.name == "hello"),
            "scala def captured"
        );
    }

    #[test]
    fn kotlin_definitions() {
        let src = "class Greeter {\n  fun hello(): Unit {}\n}\n";
        let p = extract("app.kt", src);
        assert!(
            p.defs
                .iter()
                .any(|d| d.name == "Greeter" || d.name == "hello"),
            "kotlin def captured"
        );
    }
}
