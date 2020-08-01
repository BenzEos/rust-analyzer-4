//! Resolves and rewrites links in markdown documentation for hovers/completion windows.

use std::iter::once;

use itertools::Itertools;
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag};
use pulldown_cmark_to_cmark::cmark;
use url::Url;

use hir::{Adt, AsName, AttrDef, Crate, Hygiene, ItemInNs, ModPath, ModuleDef};
use ra_hir_def::db::DefDatabase;
use ra_ide_db::{defs::Definition, RootDatabase};
use ra_syntax::ast::Path;
use ra_tt::{Ident, Leaf, Literal, TokenTree};

/// Rewrite documentation links in markdown to point to an online host (e.g. docs.rs)
pub fn rewrite_links(db: &RootDatabase, markdown: &str, definition: &Definition) -> String {
    let doc = Parser::new_with_broken_link_callback(
        markdown,
        Options::empty(),
        Some(&|label, _| Some((/*url*/ label.to_string(), /*title*/ label.to_string()))),
    );

    let doc = map_links(doc, |target, title: &str| {
        // This check is imperfect, there's some overlap between valid intra-doc links
        // and valid URLs so we choose to be too eager to try to resolve what might be
        // a URL.
        if target.contains("://") {
            (target.to_string(), title.to_string())
        } else {
            // Two posibilities:
            // * path-based links: `../../module/struct.MyStruct.html`
            // * module-based links (AKA intra-doc links): `super::super::module::MyStruct`
            let resolved = try_resolve_intra(db, definition, title, &target).or_else(|| {
                try_resolve_path(db, definition, &target).map(|target| (target, title.to_string()))
            });

            match resolved {
                Some((target, title)) => (target, title),
                None => (target.to_string(), title.to_string()),
            }
        }
    });
    let mut out = String::new();
    cmark(doc, &mut out, None).ok();
    out
}

// Rewrites a markdown document, resolving links using `callback` and additionally striping prefixes/suffixes on link titles.
fn map_links<'e>(
    events: impl Iterator<Item = Event<'e>>,
    callback: impl Fn(&str, &str) -> (String, String),
) -> impl Iterator<Item = Event<'e>> {
    let mut in_link = false;
    let mut link_target: Option<CowStr> = None;

    events.map(move |evt| match evt {
        Event::Start(Tag::Link(_link_type, ref target, _)) => {
            in_link = true;
            link_target = Some(target.clone());
            evt
        }
        Event::End(Tag::Link(link_type, _target, _)) => {
            in_link = false;
            Event::End(Tag::Link(link_type, link_target.take().unwrap(), CowStr::Borrowed("")))
        }
        Event::Text(s) if in_link => {
            let (link_target_s, link_name) = callback(&link_target.take().unwrap(), &s);
            link_target = Some(CowStr::Boxed(link_target_s.into()));
            Event::Text(CowStr::Boxed(link_name.into()))
        }
        Event::Code(s) if in_link => {
            let (link_target_s, link_name) = callback(&link_target.take().unwrap(), &s);
            link_target = Some(CowStr::Boxed(link_target_s.into()));
            Event::Code(CowStr::Boxed(link_name.into()))
        }
        _ => evt,
    })
}

#[derive(PartialEq, Eq, Hash, Copy, Clone, Debug)]
enum Namespace {
    Types,
    Values,
    Macros,
}

static TYPES: ([&str; 7], [&str; 0]) =
    (["type", "struct", "enum", "mod", "trait", "union", "module"], []);
static VALUES: ([&str; 8], [&str; 1]) =
    (["value", "function", "fn", "method", "const", "static", "mod", "module"], ["()"]);
static MACROS: ([&str; 1], [&str; 1]) = (["macro"], ["!"]);

impl Namespace {
    /// Extract the specified namespace from an intra-doc-link if one exists.
    ///
    /// # Examples
    ///
    /// * `struct MyStruct` -> `Namespace::Types`
    /// * `panic!` -> `Namespace::Macros`
    /// * `fn@from_intra_spec` -> `Namespace::Values`
    fn from_intra_spec(s: &str) -> Option<Self> {
        [
            (Namespace::Types, (TYPES.0.iter(), TYPES.1.iter())),
            (Namespace::Values, (VALUES.0.iter(), VALUES.1.iter())),
            (Namespace::Macros, (MACROS.0.iter(), MACROS.1.iter())),
        ]
        .iter()
        .filter(|(_ns, (prefixes, suffixes))| {
            prefixes
                .clone()
                .map(|prefix| {
                    s.starts_with(*prefix)
                        && s.chars()
                            .nth(prefix.len() + 1)
                            .map(|c| c == '@' || c == ' ')
                            .unwrap_or(false)
                })
                .any(|cond| cond)
                || suffixes
                    .clone()
                    .map(|suffix| {
                        s.starts_with(*suffix)
                            && s.chars()
                                .nth(suffix.len() + 1)
                                .map(|c| c == '@' || c == ' ')
                                .unwrap_or(false)
                    })
                    .any(|cond| cond)
        })
        .map(|(ns, (_, _))| *ns)
        .next()
    }
}

// Strip prefixes, suffixes, and inline code marks from the given string.
fn strip_prefixes_suffixes(mut s: &str) -> &str {
    s = s.trim_matches('`');

    [
        (TYPES.0.iter(), TYPES.1.iter()),
        (VALUES.0.iter(), VALUES.1.iter()),
        (MACROS.0.iter(), MACROS.1.iter()),
    ]
    .iter()
    .for_each(|(prefixes, suffixes)| {
        prefixes.clone().for_each(|prefix| s = s.trim_start_matches(*prefix));
        suffixes.clone().for_each(|suffix| s = s.trim_end_matches(*suffix));
    });
    s.trim_start_matches("@").trim()
}

/// Try to resolve path to local documentation via intra-doc-links (i.e. `super::gateway::Shard`).
///
/// See [RFC1946](https://github.com/rust-lang/rfcs/blob/master/text/1946-intra-rustdoc-links.md).
fn try_resolve_intra(
    db: &RootDatabase,
    definition: &Definition,
    link_text: &str,
    link_target: &str,
) -> Option<(String, String)> {
    // Set link_target for implied shortlinks
    let link_target =
        if link_target.is_empty() { link_text.trim_matches('`') } else { link_target };

    // Namespace disambiguation
    let namespace = Namespace::from_intra_spec(link_target);

    // Strip prefixes/suffixes
    let link_target = strip_prefixes_suffixes(link_target);

    // Parse link as a module path
    let path = Path::parse(link_target).ok()?;
    let modpath = ModPath::from_src(path, &Hygiene::new_unhygienic()).unwrap();

    // Resolve it relative to symbol's location (according to the RFC this should consider small scopes)
    let resolver = definition.resolver(db)?;

    let resolved = resolver.resolve_module_path_in_items(db, &modpath);
    let (defid, namespace) = match namespace {
        // FIXME: .or(resolved.macros)
        None => resolved
            .types
            .map(|t| (t.0, Namespace::Types))
            .or(resolved.values.map(|t| (t.0, Namespace::Values)))?,
        Some(ns @ Namespace::Types) => (resolved.types?.0, ns),
        Some(ns @ Namespace::Values) => (resolved.values?.0, ns),
        // FIXME:
        Some(Namespace::Macros) => None?,
    };

    // Get the filepath of the final symbol
    let def: ModuleDef = defid.into();
    let module = def.module(db)?;
    let krate = module.krate();
    let ns = match namespace {
        Namespace::Types => ItemInNs::Types(defid),
        Namespace::Values => ItemInNs::Values(defid),
        // FIXME:
        Namespace::Macros => None?,
    };
    let import_map = db.import_map(krate.into());
    let path = import_map.path_of(ns)?;

    Some((
        get_doc_url(db, &krate)?
            .join(&format!("{}/", krate.display_name(db)?))
            .ok()?
            .join(&path.segments.iter().map(|name| name.to_string()).join("/"))
            .ok()?
            .join(&get_symbol_filename(db, &Definition::ModuleDef(def))?)
            .ok()?
            .into_string(),
        strip_prefixes_suffixes(link_text).to_string(),
    ))
}

/// Try to resolve path to local documentation via path-based links (i.e. `../gateway/struct.Shard.html`).
fn try_resolve_path(db: &RootDatabase, definition: &Definition, link: &str) -> Option<String> {
    if !link.contains("#") && !link.contains(".html") {
        return None;
    }
    let ns = if let Definition::ModuleDef(moddef) = definition {
        ItemInNs::Types(moddef.clone().into())
    } else {
        return None;
    };
    let module = definition.module(db)?;
    let krate = module.krate();
    let import_map = db.import_map(krate.into());
    let base = once(format!("{}", krate.display_name(db)?))
        .chain(import_map.path_of(ns)?.segments.iter().map(|name| format!("{}", name)))
        .join("/");

    get_doc_url(db, &krate)
        .and_then(|url| url.join(&base).ok())
        .and_then(|url| {
            get_symbol_filename(db, definition).as_deref().map(|f| url.join(f).ok()).flatten()
        })
        .and_then(|url| url.join(link).ok())
        .map(|url| url.into_string())
}

/// Try to get the root URL of the documentation of a crate.
fn get_doc_url(db: &RootDatabase, krate: &Crate) -> Option<Url> {
    // Look for #![doc(html_root_url = "...")]
    let attrs = db.attrs(AttrDef::from(krate.root_module(db)?).into());
    let doc_attr_q = attrs.by_key("doc");

    let doc_url = if doc_attr_q.exists() {
        doc_attr_q.tt_values().map(|tt| {
            let name = tt.token_trees.iter()
                .skip_while(|tt| !matches!(tt, TokenTree::Leaf(Leaf::Ident(Ident{text: ref ident, ..})) if ident == "html_root_url"))
                .skip(2)
                .next();

            match name {
                Some(TokenTree::Leaf(Leaf::Literal(Literal{ref text, ..}))) => Some(text),
                _ => None
            }
        }).flat_map(|t| t).next().map(|s| s.to_string())
    } else {
        // Fallback to docs.rs
        // FIXME: Specify an exact version here (from Cargo.lock)
        Some(format!("https://docs.rs/{}/*", krate.display_name(db)?))
    };

    doc_url
        .map(|s| s.trim_matches('"').trim_end_matches("/").to_owned() + "/")
        .and_then(|s| Url::parse(&s).ok())
}

/// Get the filename and extension generated for a symbol by rustdoc.
///
/// Example: `struct.Shard.html`
fn get_symbol_filename(db: &RootDatabase, definition: &Definition) -> Option<String> {
    Some(match definition {
        Definition::ModuleDef(def) => match def {
            ModuleDef::Adt(adt) => match adt {
                Adt::Struct(s) => format!("struct.{}.html", s.name(db)),
                Adt::Enum(e) => format!("enum.{}.html", e.name(db)),
                Adt::Union(u) => format!("union.{}.html", u.name(db)),
            },
            ModuleDef::Module(_) => "index.html".to_string(),
            ModuleDef::Trait(t) => format!("trait.{}.html", t.name(db)),
            ModuleDef::TypeAlias(t) => format!("type.{}.html", t.name(db)),
            ModuleDef::BuiltinType(t) => format!("primitive.{}.html", t.as_name()),
            ModuleDef::Function(f) => format!("fn.{}.html", f.name(db)),
            ModuleDef::EnumVariant(ev) => {
                format!("enum.{}.html#variant.{}", ev.parent_enum(db).name(db), ev.name(db))
            }
            ModuleDef::Const(c) => format!("const.{}.html", c.name(db)?),
            ModuleDef::Static(s) => format!("static.{}.html", s.name(db)?),
        },
        Definition::Macro(m) => format!("macro.{}.html", m.name(db)?),
        _ => None?,
    })
}
