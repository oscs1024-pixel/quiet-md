//! One CLI invocation or one `quiet run`: the library open, documents read
//! in as needed, every change held in memory until `commit`. Nothing is
//! written if the script fails; at commit, a document changed on disk since
//! it was read (the app saved it, say) stops the whole commit.
//!
//! Every operation is `call(op, args)` with JSON in and out, so the shell
//! commands and the script API are the same code.

use std::path::{Path, PathBuf};

use fdl_core::editor::core::{Core, DropPlace};
use fdl_core::model::{Block, BlockId, BlockKind, Document, RichText};
use fdl_core::store::{DocKind, Entry, Store};
use serde_json::{Value, json};

use crate::blocks;
use crate::fail::{R, fail};

pub struct Session {
    pub store: Store,
    pub root: PathBuf,
    pub(crate) entries: Vec<Entry>,
    pub(crate) docs: Vec<Open>,
    pub(crate) tables: Vec<crate::tables::OpenTable>,
    pub(crate) pending: Vec<Pending>,
    /// Pictures from outside the library new blocks show: copied into its
    /// assets at commit, before the documents that point at them are saved.
    pub(crate) assets: Vec<(PathBuf, PathBuf)>,
    /// In `quiet run`: nothing outside the library (no `import`, no local pictures).
    pub sandbox: bool,
}

pub(crate) struct Open {
    pub(crate) key: String,
    /// None: created in this session, written at commit.
    pub(crate) id: Option<String>,
    pub(crate) path: String,
    pub(crate) title: String,
    pub(crate) kind: &'static str,
    pub(crate) doc: Document,
    pub(crate) orig: Document,
    pub(crate) disk: Vec<u8>,
    pub(crate) rev: String,
    pub(crate) dirty: bool,
    /// Keep what the file holds now as a version before saving, however
    /// recent the last one (a restore, which can then be undone in turn).
    pub(crate) keep_version: bool,
}

pub(crate) enum Pending {
    /// A document, Markdown file or board opened in this run (`key`).
    Create { key: String, folder: Option<String> },
    CreateTable { title: String, folder: Option<String>, csv: String },
    Mkdir { name: String, folder: Option<String> },
    Rename { id: String, title: String },
    Move { id: String, folder: Option<String> },
    Trash { id: String },
    Untrash { id: String, title: String },
    Import { path: PathBuf, folder: Option<String> },
    RestoreTable { id: String, version: String },
}

pub(crate) fn date_ms(ms: u64) -> String { date(ms / 1000) }

pub fn rev_of(bytes: &[u8]) -> String { sha1_smol::Sha1::from(bytes).digest().to_string()[..12].to_string() }

fn date(secs: u64) -> String { let (y, m, d, h, mi) = fdl_core::time::civil(secs); format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}") }

fn kind_of(e: &Entry) -> &'static str {
    if e.meta.folder.is_some() { return "folder"; }
    match e.meta.kind { DocKind::Doc if e.meta.markdown => "md", DocKind::Doc => "doc", DocKind::Board => "board", DocKind::Table => "table" }
}

pub(crate) fn arg<'a>(a: &'a Value, k: &str) -> Option<&'a str> { a.get(k).and_then(Value::as_str) }
pub(crate) fn need<'a>(a: &'a Value, k: &str) -> R<&'a str> { arg(a, k).ok_or_else(|| fail("bad_args", format!("missing `{k}`"))) }
pub(crate) fn flag(a: &Value, k: &str) -> bool { a.get(k).and_then(Value::as_bool).unwrap_or(false) }
pub(crate) fn num(a: &Value, k: &str) -> Option<usize> { a.get(k).and_then(Value::as_u64).map(|n| n as usize) }

/// The library to work on: `--library`, else `QUIET_MD_HOME`, else the one the app uses.
pub fn library_dir(given: Option<&str>) -> PathBuf { given.map(PathBuf::from).unwrap_or_else(Store::root_dir) }

impl Session {
    pub fn open(root: PathBuf) -> R<Session> {
        if !Store::is_library(&root) {
            return Err(fail("no_library", format!("{} is not a Quiet.md library", root.display())).hint("pass --library <folder>, set QUIET_MD_HOME, or open the folder in Quiet.md once"));
        }
        if Store::flat_at(&root) {
            return Err(fail("old_library", "this library still has the old layout").hint("open it in Quiet.md once to move it to real folders"));
        }
        let mut store = Store::open_for_tools(root.clone())?;
        let entries = store.list();
        Ok(Session { store, root, entries, docs: vec![], tables: vec![], pending: vec![], assets: vec![], sandbox: false })
    }

    pub(crate) fn refresh(&mut self) { self.entries = self.store.list(); }

    fn docs_list(&self) -> impl Iterator<Item = &Entry> { self.entries.iter().filter(|e| e.meta.folder.is_none() && !e.meta.local()) }

    pub(crate) fn entry_by_id(&self, id: &str) -> Option<&Entry> { self.entries.iter().find(|e| e.id == id) }

    /// A document by path (`工作/周报.xml`, with or without the extension) or title.
    pub(crate) fn find_entry(&self, spec: &str) -> R<String> {
        let s = spec.trim().trim_start_matches("./").trim_start_matches('/');
        if let Some(e) = self.docs_list().find(|e| e.meta.rel == s) { return Ok(e.id.clone()); }
        let stem = |rel: &str| rel.rsplit_once('.').map_or(rel.to_string(), |(a, _)| a.to_string());
        let by_stem: Vec<&Entry> = self.docs_list().filter(|e| stem(&e.meta.rel) == s).collect();
        if by_stem.len() == 1 { return Ok(by_stem[0].id.clone()); }
        // The title, or the file's name without folder and extension (titles
        // with characters a file name cannot hold differ from it).
        let by_title: Vec<&Entry> = self.docs_list().filter(|e| e.title == s || stem(e.meta.rel.rsplit('/').next().unwrap_or("")) == s).collect();
        if by_title.len() == 1 { return Ok(by_title[0].id.clone()); }
        let many = if by_stem.len() > 1 { by_stem } else { by_title };
        if many.len() > 1 {
            return Err(fail("ambiguous", format!("{} documents match {s:?}: {}", many.len(), many.iter().map(|e| e.meta.rel.as_str()).collect::<Vec<_>>().join(", "))).hint("pass the path"));
        }
        let low = s.to_lowercase();
        let near: Vec<&str> = self.docs_list().filter(|e| e.meta.rel.to_lowercase().contains(&low) || e.title.to_lowercase().contains(&low)).take(5).map(|e| e.meta.rel.as_str()).collect();
        let f = fail("not_found", format!("no document {s:?}"));
        Err(if near.is_empty() { f.hint("`quiet ls -r` lists every document") } else { f.hint(format!("did you mean: {}", near.join(", "))) })
    }

    /// A folder by path or name; `""` / `/` the top of the library.
    pub(crate) fn find_folder(&self, spec: Option<&str>) -> R<Option<String>> {
        let s = spec.unwrap_or("").trim().trim_matches('/');
        if s.is_empty() { return Ok(None); }
        let folders: Vec<&Entry> = self.entries.iter().filter(|e| e.meta.folder.is_some() && !e.meta.local()).collect();
        if let Some(e) = folders.iter().find(|e| e.meta.rel.trim_end_matches('/') == s) { return Ok(Some(e.id.clone())); }
        let named: Vec<&&Entry> = folders.iter().filter(|e| e.title == s || e.meta.rel.rsplit('/').next() == Some(s)).collect();
        match named.len() {
            1 => Ok(Some(named[0].id.clone())),
            0 => Err(fail("not_found", format!("no folder {s:?}")).hint("`quiet ls -r --kind folder` lists folders; `quiet mkdir` makes one")),
            _ => Err(fail("ambiguous", format!("several folders are called {s:?}: {}", named.iter().map(|e| e.meta.rel.as_str()).collect::<Vec<_>>().join(", "))).hint("pass the path")),
        }
    }

    pub(crate) fn folder_rel(&self, id: Option<&str>) -> String { id.and_then(|i| self.entry_by_id(i)).map(|e| e.meta.rel.trim_end_matches('/').to_string()).unwrap_or_default() }

    /// The open document a spec names (its session key, its path, its title), read in on first use.
    fn doc(&mut self, spec: &str) -> R<usize> {
        if let Some(i) = self.docs.iter().position(|d| d.key == spec || d.path == spec) { return Ok(i); }
        let id = self.find_entry(spec)?;
        if let Some(i) = self.docs.iter().position(|d| d.id.as_deref() == Some(id.as_str())) { return Ok(i); }
        let e = self.entry_by_id(&id).ok_or_else(|| fail("not_found", spec.to_string()))?;
        let (path, title, kind) = (e.meta.rel.clone(), e.title.clone(), kind_of(e));
        let file = self.store.file_path(&id).ok_or_else(|| fail("not_found", spec.to_string()))?;
        let disk = std::fs::read(&file)?;
        // As an editor would: the store then keeps the text a save replaces as a version.
        let doc = self.store.load_for_edit(&id, false).map_err(|e| fail("parse", format!("{path}: {e:#}")))?;
        let key = format!("d{}", self.docs.len() + 1);
        self.docs.push(Open { key, id: Some(id), path, title, kind, orig: doc.clone(), doc, rev: rev_of(&disk), disk, dirty: false, keep_version: false });
        Ok(self.docs.len() - 1)
    }

    pub(crate) fn doc_arg(&mut self, a: &Value) -> R<usize> {
        let i = self.doc(need(a, "doc")?)?;
        if let Some(want) = arg(a, "rev") && want != self.docs[i].rev {
            return Err(fail("stale_rev", format!("{} is at rev {}, not {want}", self.docs[i].path, self.docs[i].rev)).hint("it changed since you read it: read it again, then redo the edit"));
        }
        Ok(i)
    }

    fn editable(&self, i: usize) -> R<()> {
        match self.docs[i].kind {
            "doc" | "md" => Ok(()),
            k => Err(fail("read_only", format!("{} is a {k}; only documents can be edited from the CLI so far", self.docs[i].path))),
        }
    }

    fn info(&self, i: usize) -> Value {
        let d = &self.docs[i];
        json!({ "key": d.key, "path": d.path, "title": d.title, "kind": d.kind, "rev": d.rev, "block_count": blocks::items(&d.doc).len(), "new": d.id.is_none() })
    }

    /// Run `f` on the document through the editing core, as one edit.
    fn edit<T>(&mut self, i: usize, f: impl FnOnce(&mut Core) -> R<T>) -> R<T> {
        self.editable(i)?;
        let mut core = Core::new(std::mem::take(&mut self.docs[i].doc));
        let r = f(&mut core);
        let d = &mut self.docs[i];
        if r.is_ok() && core.doc != d.doc { d.dirty = true; }
        d.doc = core.doc;
        if d.doc.blocks.is_empty() { d.doc.blocks.push(Block::text("")); }
        r
    }

    fn target(&self, i: usize, a: &Value, key: &str) -> R<Vec<BlockId>> {
        let id = blocks::resolve(&self.docs[i].doc, need(a, key)?)?;
        if !flag(a, "section") { return Ok(vec![id]); }
        let mut ids = blocks::section(&self.docs[i].doc, id);
        // `heading: false`: what is under the heading, without it.
        if a.get("heading").and_then(Value::as_bool) == Some(false) && ids.len() > 1 { ids.remove(0); }
        Ok(ids)
    }

    fn refs(&self, i: usize, ids: &[BlockId]) -> Value {
        let doc = &self.docs[i].doc;
        json!(ids.iter().filter_map(|id| blocks::pin_of(doc, *id).map(|p| json!({ "ref": p, "id": blocks::id_of(*id) }))).collect::<Vec<_>>())
    }

    /// Where `insert` and `move` put blocks: `{after|before|into: anchor}`,
    /// `end_of` a heading's section, or `to: "start"|"end"`.
    fn place(&self, i: usize, a: &Value) -> R<(Option<BlockId>, DropPlace)> {
        let doc = &self.docs[i].doc;
        if let Some(anchor) = arg(a, "end_of") {
            let id = blocks::resolve(doc, anchor)?;
            return Ok((blocks::section(doc, id).last().copied(), DropPlace::After));
        }
        for (k, p) in [("after", DropPlace::After), ("before", DropPlace::Before), ("into", DropPlace::FirstChild)] {
            if let Some(anchor) = arg(a, k) {
                let id = blocks::resolve(doc, anchor)?;
                if p == DropPlace::FirstChild {
                    let b = blocks::find(doc, id).unwrap();
                    if !(b.kind.nests() || matches!(b.kind, BlockKind::Quote | BlockKind::Callout { .. } | BlockKind::Column { .. })) {
                        return Err(fail("bad_place", format!("a {} block holds no children", blocks::kind_name(&b.kind))).hint("text, headings, lists, todos, quotes, callouts and columns do"));
                    }
                    // Into: after its last child.
                    if let Some(last) = b.children.last() { return Ok((Some(last.id), DropPlace::After)); }
                }
                return Ok((Some(id), p));
            }
        }
        match arg(a, "to").unwrap_or("end") {
            "start" => Ok((doc.blocks.first().map(|b| b.id), DropPlace::Before)),
            "end" => Ok((doc.blocks.last().map(|b| b.id), DropPlace::After)),
            x => Err(fail("bad_args", format!("to: {x:?}; use start or end"))),
        }
    }

    /// A command line's `DOC#REF`: the whole of it if a document is called
    /// that, else split at the `#` (from the right) after which the rest
    /// names a document; `ref_is_after`: insert's ref is where to insert.
    fn split_doc(&self, a: &Value) -> R<Value> {
        let mut a = a.clone();
        let spec = need(&a, "doc")?.to_string();
        let o = a.as_object_mut().unwrap();
        let (doc, r) = if !spec.contains('#') || self.find_entry(&spec).is_ok() { (spec.clone(), None) } else {
            let cuts: Vec<usize> = spec.match_indices('#').map(|(i, _)| i).collect();
            let hit = cuts.iter().rev().find(|&&i| self.find_entry(&spec[..i]).is_ok()).or(cuts.first()).copied().unwrap();
            (spec[..hit].to_string(), Some(spec[hit + 1..].to_string()).filter(|r| !r.is_empty()))
        };
        o.insert("doc".into(), json!(doc));
        if let Some(r) = r && !o.contains_key("at") { o.insert("at".into(), json!(r)); }
        if o.remove("ref_is_after").and_then(|v| v.as_bool()) == Some(true) && let Some(r) = o.remove("at") { o.insert("after".into(), r); }
        if o.remove("need_ref").is_some() && !o.contains_key("at") {
            return Err(fail("usage", "this command needs a block: DOC#REF").hint("refs come from `quiet outline DOC` or `quiet blocks DOC`"));
        }
        o.remove("split");
        Ok(a)
    }

    pub fn call(&mut self, op: &str, a: &Value) -> R<Value> {
        if flag(a, "split") { let a = self.split_doc(a)?; return self.call(op, &a); }
        match op {
            "ls" => self.ls(a),
            "find" => self.find(a),
            "open" => { let i = self.doc_arg(a)?; Ok(self.info(i)) }
            "outline" => self.outline(a),
            "read" => self.read(a),
            "blocks" => self.blocks(a),
            "insert" => self.insert(a),
            "replace" => self.replace(a),
            "remove" => self.remove(a),
            "move" => self.move_blocks(a),
            "set" => self.set(a),
            "create" => self.create(a),
            "mkdir" => self.mkdir(a),
            "rename" => { let i = self.doc_arg(a)?; let id = self.saved_id(i)?; self.pending.push(Pending::Rename { id, title: need(a, "title")?.to_string() }); Ok(json!({ "path": self.docs[i].path, "rename": need(a, "title")? })) }
            "mv" => { let i = self.doc_arg(a)?; let id = self.saved_id(i)?; let folder = self.find_folder(arg(a, "folder"))?; self.pending.push(Pending::Move { id, folder }); Ok(json!({ "path": self.docs[i].path, "to": arg(a, "folder").unwrap_or("/") })) }
            "trash" => { let i = self.doc_arg(a)?; let id = self.saved_id(i)?; self.pending.push(Pending::Trash { id }); Ok(json!({ "path": self.docs[i].path, "trash": true })) }
            "links" => self.links(a),
            "table" => self.table(a),
            "check" => self.check(),
            "history" => self.history(a),
            "restore" => self.restore(a),
            "trash_list" => self.trash_list(),
            "untrash" => self.untrash(a),
            "import" => self.import(a),
            "table_add" => self.table_add(a),
            "table_update" => self.table_update(a),
            "table_delete" => self.table_delete(a),
            "board" => self.board(a),
            "board_add" => self.board_add(a),
            "board_remove" => self.board_remove(a),
            _ => Err(fail("usage", format!("unknown operation {op:?}")).hint("`quiet help` lists the commands")),
        }
    }

    pub(crate) fn saved_id(&self, i: usize) -> R<String> { self.docs[i].id.clone().ok_or_else(|| fail("not_yet", "this document is created when the script ends; move or rename it in the next run")) }

    fn ls(&mut self, a: &Value) -> R<Value> {
        let folder = self.find_folder(arg(a, "folder"))?;
        let base = self.folder_rel(folder.as_deref());
        let (deep, kind) = (flag(a, "recursive"), arg(a, "kind"));
        let mut out: Vec<&Entry> = self.entries.iter().filter(|e| !e.meta.local())
            .filter(|e| { let rel = e.meta.rel.trim_end_matches('/'); let rest = if base.is_empty() { Some(rel) } else { rel.strip_prefix(&format!("{base}/")) }; rest.is_some_and(|r| !r.is_empty() && (deep || !r.contains('/'))) })
            .filter(|e| kind.is_none_or(|k| k == kind_of(e))).collect();
        out.sort_by(|a, b| (a.meta.folder.is_none(), &a.meta.rel).cmp(&(b.meta.folder.is_none(), &b.meta.rel)));
        Ok(json!(out.iter().map(|e| {
            let mut v = json!({ "path": e.meta.rel.trim_end_matches('/'), "title": e.title, "kind": kind_of(e), "modified": date(e.meta.modified) });
            if e.meta.starred > 0 { v["starred"] = json!(true); }
            if e.meta.pinned { v["pinned"] = json!(true); }
            v
        }).collect::<Vec<_>>()))
    }

    /// Documents holding every word of `query` (any case) in their title or
    /// text, best first: the whole phrase in the title, then every word in
    /// the title, then the phrase in the text, then by how often the words
    /// occur. Each comes with its best-matching blocks.
    fn find(&mut self, a: &Value) -> R<Value> {
        let q = need(a, "query")?.trim().to_lowercase();
        let words: Vec<&str> = q.split_whitespace().collect();
        if words.is_empty() { return Err(fail("bad_args", "empty query")); }
        let folder = self.find_folder(arg(a, "folder"))?;
        let base = self.folder_rel(folder.as_deref());
        let limit = num(a, "limit").unwrap_or(20);
        let mut hits: Vec<(usize, String, String)> = self.docs_list()
            .filter(|e| base.is_empty() || e.meta.rel.starts_with(&format!("{base}/")))
            .filter_map(|e| {
                let (title, body) = (e.title.to_lowercase(), e.body.to_lowercase());
                if !words.iter().all(|w| title.contains(w) || body.contains(w)) { return None; }
                let often: usize = words.iter().map(|w| body.matches(w).count().min(50)).sum();
                let score = if title.contains(&q) { 4000 } else if words.iter().all(|w| title.contains(w)) { 3000 } else if body.contains(&q) { 2000 } else { 1000 } + often;
                Some((score, e.meta.rel.clone(), e.title.clone()))
            }).collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let total = hits.len();
        let mut out = vec![];
        for (_, path, title) in hits.into_iter().take(limit) {
            let i = match self.doc(&path) { Ok(i) => i, Err(_) => continue };
            let all = blocks::items(&self.docs[i].doc);
            // A block's own text (containers hold none), scored: the phrase 3, every word 2, some word 1.
            let mut found: Vec<(usize, usize, &blocks::Item)> = all.iter().enumerate().filter_map(|(n, it)| {
                if matches!(it.block.kind, BlockKind::Quote | BlockKind::Callout { .. } | BlockKind::Grid | BlockKind::Column { .. }) { return None; }
                let t = blocks::text_of(it.block).to_lowercase();
                let s = if t.contains(&q) { 3 } else if words.iter().all(|w| t.contains(w)) { 2 } else if words.iter().any(|w| t.contains(w)) { 1 } else { 0 };
                (s > 0).then_some((s, n, it))
            }).collect();
            found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            let more = found.len().saturating_sub(5);
            let shown: Vec<Value> = found.iter().take(5).map(|(_, _, it)| {
                let t = blocks::text_of(it.block);
                let low = t.to_lowercase();
                let key = if low.contains(&q) { q.as_str() } else { words.iter().copied().find(|w| low.contains(w)).unwrap_or(&q) };
                json!({ "ref": blocks::pin(it), "kind": blocks::kind_name(&it.block.kind), "text": snippet(&t, key) })
            }).collect();
            let mut r = json!({ "path": path, "title": title, "rev": self.docs[i].rev, "hits": shown });
            if more > 0 { r["more_hits"] = json!(more); }
            out.push(r);
        }
        Ok(json!({ "total": total, "results": out }))
    }

    fn outline(&mut self, a: &Value) -> R<Value> {
        let i = self.doc_arg(a)?;
        let doc = &self.docs[i].doc;
        let all = blocks::items(doc);
        let heads: Vec<Value> = all.iter().filter_map(|it| {
            let level = blocks::heading_level(it.block)?;
            let chars: usize = blocks::section(doc, it.block.id).iter().filter_map(|id| blocks::find(doc, *id)).map(sub_chars).sum();
            Some(json!({ "ref": blocks::pin(it), "level": level, "text": blocks::text_of(it.block), "chars": chars }))
        }).collect();
        let mut v = self.info(i);
        v["chars"] = json!(doc.blocks.iter().map(sub_chars).sum::<usize>());
        if heads.is_empty() {
            v["top"] = json!(all.iter().filter(|it| it.path.len() == 1).take(40).map(|it| json!({ "ref": blocks::pin(it), "kind": blocks::kind_name(&it.block.kind), "text": blocks::clip(&blocks::text_of(it.block), 80) })).collect::<Vec<_>>());
        }
        v["headings"] = json!(heads);
        Ok(v)
    }

    fn read(&mut self, a: &Value) -> R<Value> {
        let i = self.doc_arg(a)?;
        let d = &self.docs[i];
        let as_ = arg(a, "as").unwrap_or("md");
        let picked: Option<Vec<BlockId>> = match arg(a, "at") { Some(_) => Some(self.target(i, a, "at")?), None => None };
        let mut chosen: Vec<Block> = match &picked { Some(ids) => ids.iter().filter_map(|id| blocks::find(&d.doc, *id).cloned()).collect(), None => d.doc.blocks.clone() };
        let mut names = vec![];
        if matches!(as_, "md" | "markdown") {
            let stem = |rel: &str| rel.rsplit_once('.').map_or(rel.to_string(), |(a, _)| a.to_string());
            names = blocks::mentions_as_wiki(&mut chosen, &|id| self.entry_by_id(id).map(|e| stem(&e.meta.rel)));
        }
        let text = match as_ {
            // The body only: the title is the document's name, not a line of it.
            "md" | "markdown" => blocks::wiki_back(&fdl_core::markdown::clipboard(None, &chosen, &self.root), &names),
            "text" => chosen.iter().map(sub_text).collect::<Vec<_>>().join("\n"),
            "xml" => {
                if picked.is_some() { return Err(fail("bad_args", "--as xml reads whole documents only")); }
                if d.kind != "doc" { return Err(fail("bad_args", format!("{} is not a DocxXML file", d.path))); }
                String::from_utf8_lossy(&d.disk).into_owned()
            }
            "json" => {
                let roots: Option<Vec<Vec<usize>>> = picked.as_ref().map(|ids| ids.iter().filter_map(|id| blocks::path_of(&d.doc, *id)).collect());
                let list: Vec<Value> = blocks::items(&d.doc).iter().filter(|it| roots.as_ref().is_none_or(|r| r.iter().any(|p| it.path.starts_with(p)))).map(blocks::block_json).collect();
                return Ok(json!({ "path": d.path, "rev": d.rev, "blocks": list }));
            }
            x => return Err(fail("bad_args", format!("--as {x}: use md, text, xml, json (or the outline command)"))),
        };
        let (text, cut) = match num(a, "max_chars") { Some(n) if text.chars().count() > n => (text.chars().take(n).collect::<String>(), true), _ => (text, false) };
        let mut v = json!({ "path": d.path, "title": d.title, "rev": d.rev, "text": text });
        if cut { v["truncated"] = json!(true); }
        Ok(v)
    }

    fn blocks(&mut self, a: &Value) -> R<Value> {
        let i = self.doc_arg(a)?;
        let doc = &self.docs[i].doc;
        let within: Option<Vec<usize>> = match arg(a, "at") { Some(x) => Some(blocks::path_of(doc, blocks::resolve(doc, x)?).unwrap()), None => None };
        let kind = arg(a, "kind");
        let grep = arg(a, "grep").map(str::to_lowercase);
        let done = a.get("done").and_then(Value::as_bool);
        let list: Vec<Value> = blocks::items(doc).iter()
            .filter(|it| within.as_ref().is_none_or(|w| it.path.starts_with(w)))
            .filter(|it| kind.is_none_or(|k| { let n = blocks::kind_name(&it.block.kind); n == k || (k == "heading" && n.starts_with('h') && n.len() == 2) }))
            .filter(|it| grep.as_ref().is_none_or(|g| blocks::text_of(it.block).to_lowercase().contains(g)))
            .filter(|it| done.is_none_or(|want| matches!(it.block.kind, BlockKind::Todo { done } if done == want)))
            .map(blocks::block_json).collect();
        Ok(json!({ "path": self.docs[i].path, "rev": self.docs[i].rev, "blocks": list }))
    }

    /// New blocks from `md` for document `i`.
    fn new_blocks(&mut self, i: usize, a: &Value) -> R<Vec<Block>> {
        let md = need(a, "md")?;
        let mut b = blocks::from_markdown(md);
        if b.is_empty() { return Err(fail("bad_args", "the Markdown holds no blocks")); }
        let markdown = self.docs[i].kind == "md";
        self.link(&mut b, markdown)?;
        Ok(b)
    }

    /// `[[…]]` and links to library files in new blocks as mentions, and
    /// pictures given by a path outside the library copied in (at commit).
    /// `markdown`: for a Markdown file, which keeps `[[…]]` as it is written
    /// (its links are by name), so they are only checked.
    fn link(&mut self, list: &mut [Block], markdown: bool) -> R<()> {
        let assets = Store::assets_in(&self.root);
        let mut err = None;
        blocks::each_block(list, &mut |b| {
            let BlockKind::Image { src, .. } = &mut b.kind else { return };
            let Some((from, to)) = crate::library::picture_copy(&self.root, &assets, src, self.assets.len()) else { return };
            if self.sandbox { err.get_or_insert_with(|| fail("sandbox", format!("a script cannot bring in {}", from.display())).hint("insert the picture with `quiet insert` from the shell")); return; }
            *src = to.to_string_lossy().into_owned();
            self.assets.push((from, to));
        });
        if let Some(e) = err { return Err(e); }
        let missing = blocks::link_mentions(list, !markdown, &|t| {
            let id = self.find_entry(t).ok()?;
            self.entry_by_id(&id).map(|e| (id, e.title.clone()))
        });
        match missing.first() {
            None => Ok(()),
            Some(t) => Err(fail("not_found", format!("[[{t}]]: no such document{}", if missing.len() > 1 { format!(" (nor {})", missing[1..].join(", ")) } else { String::new() }))
                .hint("[[…]] takes a document's path or title (`quiet ls -r`); a document made in this script can be mentioned from the next run")),
        }
    }

    fn insert(&mut self, a: &Value) -> R<Value> {
        let i = self.doc_arg(a)?;
        let new = self.new_blocks(i, a)?;
        let ids: Vec<BlockId> = new.iter().map(|b| b.id).collect();
        let (target, place) = self.place(i, a)?;
        // A document that is one empty paragraph: the new blocks replace it.
        let blank = { let d = &self.docs[i].doc; d.blocks.len() == 1 && d.blocks[0].kind == BlockKind::Text && d.blocks[0].text.is_empty() && d.blocks[0].children.is_empty() };
        self.edit(i, |core| {
            let t = target.unwrap_or(core.doc.blocks[0].id);
            if blank { let old = core.doc.blocks[0].id; core.insert_blocks_at(new, t, DropPlace::Before); core.delete_block(old); } else { core.insert_blocks_at(new, t, place); }
            Ok(())
        })?;
        Ok(json!({ "path": self.docs[i].path, "inserted": self.refs(i, &ids) }))
    }

    fn replace(&mut self, a: &Value) -> R<Value> {
        let i = self.doc_arg(a)?;
        let old = self.target(i, a, "at")?;
        let new = self.new_blocks(i, a)?;
        let ids: Vec<BlockId> = new.iter().map(|b| b.id).collect();
        self.edit(i, |core| { core.insert_blocks_at(new, old[0], DropPlace::Before); for id in &old { core.delete_block(*id); } Ok(()) })?;
        Ok(json!({ "path": self.docs[i].path, "replaced": old.len(), "inserted": self.refs(i, &ids) }))
    }

    fn remove(&mut self, a: &Value) -> R<Value> {
        let i = self.doc_arg(a)?;
        let old = self.target(i, a, "at")?;
        self.edit(i, |core| { for id in &old { core.delete_block(*id); } Ok(()) })?;
        Ok(json!({ "path": self.docs[i].path, "removed": old.len() }))
    }

    fn move_blocks(&mut self, a: &Value) -> R<Value> {
        let i = self.doc_arg(a)?;
        let ids = self.target(i, a, "at")?;
        if ["after", "before", "into", "end_of", "to"].iter().all(|k| arg(a, k).is_none()) {
            return Err(fail("bad_args", "move needs a place: --after, --before, --into or --end-of REF, or --start / --end"));
        }
        let (target, place) = self.place(i, a)?;
        let t = target.ok_or_else(|| fail("bad_place", "no place to move to"))?;
        if ids.contains(&t) { return Err(fail("bad_place", "a block cannot move next to itself")); }
        let moved = self.edit(i, |core| Ok(core.move_blocks(&ids, t, place)))?;
        if !moved { return Err(fail("bad_place", "that move changes nothing or would put a block inside itself")); }
        Ok(json!({ "path": self.docs[i].path, "moved": self.refs(i, &ids) }))
    }

    fn set(&mut self, a: &Value) -> R<Value> {
        let i = self.doc_arg(a)?;
        let id = blocks::resolve(&self.docs[i].doc, need(a, "at")?)?;
        let text = match arg(a, "text") {
            Some(t) => { let mut probe = vec![Block::text("")]; probe[0].text = blocks::inline_text(t, false); let md = self.docs[i].kind == "md"; self.link(&mut probe, md)?; Some((t.to_string(), probe.remove(0).text)) }
            None => None,
        };
        let done = a.get("done").and_then(Value::as_bool);
        let kind = match arg(a, "kind") { Some(k) => Some(blocks::parse_kind(k).ok_or_else(|| fail("bad_args", format!("kind {k:?}: use text, h1…h6, bullet, ordered, todo, quote, code")))?), None => None };
        if text.is_none() && done.is_none() && kind.is_none() { return Err(fail("bad_args", "set needs text, done or kind")); }
        self.edit(i, |core| {
            if let Some(k) = kind {
                let b = core.block(id).unwrap();
                if !b.kind.has_text() { return Err(fail("bad_kind", format!("a {} block cannot be turned into another kind", blocks::kind_name(&b.kind)))); }
                core.turn_into(&[id], k);
            }
            if let Some((raw, linked)) = &text {
                let b = core.block_mut(id).unwrap();
                if !b.kind.has_text() { return Err(fail("bad_kind", format!("a {} block has no text of its own", blocks::kind_name(&b.kind)))); }
                b.text = if matches!(b.kind, BlockKind::Code { .. }) { blocks::inline_text(raw, true) } else { linked.clone() };
            }
            if let Some(want) = done {
                match &mut core.block_mut(id).unwrap().kind { BlockKind::Todo { done } => *done = want, k => return Err(fail("bad_kind", format!("only todos are done or not; this is a {}", blocks::kind_name(k)))) }
            }
            Ok(())
        })?;
        Ok(json!({ "path": self.docs[i].path, "set": self.refs(i, &[id]) }))
    }

    fn create(&mut self, a: &Value) -> R<Value> {
        let title = need(a, "title")?.trim().to_string();
        if title.is_empty() { return Err(fail("bad_args", "a new document needs a title")); }
        let folder = arg(a, "folder").map(str::to_string);
        let kind: &'static str = match arg(a, "kind") {
            None if flag(a, "markdown") => "md",
            None | Some("doc") => "doc",
            Some("md" | "markdown") => "md",
            Some("board") => "board",
            Some("table") => "table",
            Some(k) => return Err(fail("bad_args", format!("kind {k:?}: a new file is a doc, md, board or table"))),
        };
        // A folder made earlier in this run exists only by its path until commit.
        let made = self.pending.iter().any(|p| matches!(p, Pending::Mkdir { name, folder: f } if folder.as_deref().map(|s| s.trim_matches('/')) == Some(&join(&self.folder_rel(f.as_deref()), name))));
        let base = if made { folder.clone().unwrap_or_default().trim_matches('/').to_string() } else { let f = self.find_folder(folder.as_deref())?; self.folder_rel(f.as_deref()) };
        let ext = match kind { "md" => "md", "board" => "excalidraw", "table" => "sqlite", _ => "xml" };
        let path = join(&base, &format!("{}.{ext}", fdl_core::store::file_name_for(&title)));
        let folder = Some(base).filter(|b| !b.is_empty());
        if kind == "table" {
            // Columns from a CSV (its first row names them), or just their names.
            let csv = match (arg(a, "csv"), a.get("fields")) {
                (Some(c), _) => c.to_string(),
                (None, Some(Value::Array(l))) => l.iter().filter_map(Value::as_str).map(|f| if f.contains([',', '"']) { format!("{:?}", f) } else { f.to_string() }).collect::<Vec<_>>().join(",") + "\n",
                (None, Some(Value::String(f))) => format!("{f}\n"),
                _ => return Err(fail("bad_args", "a new 数据表 needs its columns: csv (with a header row) or fields")),
            };
            self.pending.push(Pending::CreateTable { title, folder, csv });
            return Ok(json!({ "path": path, "kind": "table", "new": true }));
        }
        let doc = if kind == "board" { fdl_core::whiteboard::file_doc(fdl_core::whiteboard::Board::default(), &title) } else {
            let mut doc = Document { title: RichText::plain(&title), ..Default::default() };
            if let Some(md) = arg(a, "md") { let mut b = blocks::from_markdown(md); self.link(&mut b, kind == "md")?; if !b.is_empty() { doc.blocks = b; } }
            doc
        };
        let key = format!("d{}", self.docs.len() + 1);
        self.docs.push(Open { key: key.clone(), id: None, path, title, kind, orig: Document::default(), doc, disk: vec![], rev: String::new(), dirty: true, keep_version: false });
        self.pending.push(Pending::Create { key, folder });
        Ok(self.info(self.docs.len() - 1))
    }

    fn mkdir(&mut self, a: &Value) -> R<Value> {
        let name = need(a, "name")?.trim().to_string();
        let f = self.find_folder(arg(a, "folder"))?;
        let path = join(&self.folder_rel(f.as_deref()), &fdl_core::store::file_name_for(&name));
        self.pending.push(Pending::Mkdir { name, folder: f });
        Ok(json!({ "path": path, "mkdir": true }))
    }

    fn links(&mut self, a: &Value) -> R<Value> {
        let i = self.doc_arg(a)?;
        let id = self.docs[i].id.clone();
        let doc = &self.docs[i].doc;
        let mut out: Vec<Value> = doc.doc_links().iter().map(|l| {
            let to = self.entry_by_id(&l.target).map(|e| e.meta.rel.clone());
            json!({ "to": to, "broken": to.is_none(), "text": line_text(&l.line) })
        }).collect();
        // `[[…]]` as a Markdown file keeps them: links by name.
        for (target, text) in blocks::wiki_links(doc) {
            let to = self.find_entry(&target).ok().and_then(|id| self.entry_by_id(&id)).map(|e| e.meta.rel.clone());
            out.push(json!({ "to": to, "broken": to.is_none(), "text": text }));
        }
        let back: Vec<Value> = match &id {
            Some(id) => self.docs_list().filter(|e| e.links.iter().any(|l| &l.target == id)).map(|e| json!({ "from": e.meta.rel, "title": e.title, "text": e.links.iter().filter(|l| &l.target == id).map(|l| line_text(&l.line)).collect::<Vec<_>>() })).collect(),
            None => vec![],
        };
        Ok(json!({ "path": self.docs[i].path, "out": out, "back": back }))
    }

    /// Whole-library health: files that do not parse, broken @mentions, empty pages, unused assets.
    fn check(&mut self) -> R<Value> {
        let mut issues = vec![];
        let mut live = vec![];
        let ids: Vec<(String, String, &'static str)> = self.docs_list().map(|e| (e.id.clone(), e.meta.rel.clone(), kind_of(e))).collect();
        for (id, rel, kind) in ids {
            match self.store.load(&id) {
                Err(e) => issues.push(json!({ "path": rel, "code": "parse", "message": format!("{e:#}") })),
                Ok(doc) => {
                    live.extend(doc.asset_sources());
                    doc.for_each_block(|b| if let BlockKind::DataTable(t) = &b.kind { live.push(t.src.clone()) });
                    if kind == "board" || kind == "table" { continue; }
                    for l in doc.doc_links() { if self.entry_by_id(&l.target).is_none() { issues.push(json!({ "path": rel, "code": "broken_link", "message": format!("mentions a document that is not in the library: {}", line_text(&l.line)) })); } }
                    if doc.is_blank() { issues.push(json!({ "path": rel, "code": "empty", "message": "the document is empty" })); }
                }
            }
        }
        match Store::unused_assets_at(self.root.clone(), live, vec![]) {
            Ok(list) => for (name, size) in list { issues.push(json!({ "path": format!("{}/{name}", fdl_core::store::ASSETS), "code": "unused_asset", "message": format!("not used by any document ({size} bytes)") })); },
            Err(e) => issues.push(json!({ "path": fdl_core::store::ASSETS, "code": "assets_unchecked", "message": format!("{e:#}") })),
        }
        Ok(json!({ "documents": self.docs_list().count(), "issues": issues }))
    }

    pub fn changed(&self) -> bool { !self.assets.is_empty() || !self.pending.is_empty() || self.docs.iter().any(|d| d.dirty) || self.tables.iter().any(|t| t.dirty()) }

    /// Write what changed: documents first, then creations, folders, renames,
    /// moves and trashing in the order asked. `dry`: only say what would happen.
    pub fn commit(&mut self, dry: bool) -> R<Value> {
        let mut report = vec![];
        for d in self.docs.iter().filter(|d| d.dirty && d.id.is_some()) {
            let file = self.store.file_path(d.id.as_deref().unwrap()).ok_or_else(|| fail("conflict", format!("{} is gone", d.path)))?;
            if std::fs::read(&file).ok().as_deref() != Some(&d.disk[..]) {
                return Err(fail("conflict", format!("{} changed on disk while this ran; nothing was written", d.path)).hint("read it again and redo the edit"));
            }
        }
        self.check_tables()?;
        if dry {
            for (from, to) in &self.assets { report.push(json!({ "path": to.strip_prefix(&self.root).unwrap_or(to).to_string_lossy().replace('\\', "/"), "action": "would copy picture", "source": from.display().to_string() })); }
            for d in self.docs.iter().filter(|d| d.dirty) { report.push(json!({ "path": d.path, "action": if d.id.is_some() { "would save" } else { "would create" }, "diff": diff(&d.orig, &d.doc, &self.root) })); }
            report.extend(self.describe_tables());
            for p in &self.pending { if let Some(v) = self.describe(p) { report.push(v); } }
            return Ok(json!(report));
        }
        for (from, to) in std::mem::take(&mut self.assets) {
            if let Some(dir) = to.parent() { std::fs::create_dir_all(dir)?; }
            std::fs::copy(&from, &to).map_err(|e| fail("io", format!("{}: {e}", from.display())))?;
            report.push(json!({ "path": to.strip_prefix(&self.root).unwrap_or(&to).to_string_lossy().replace('\\', "/"), "action": "copied picture", "source": from.display().to_string() }));
        }
        for i in 0..self.docs.len() {
            if !(self.docs[i].dirty && self.docs[i].id.is_some()) { continue; }
            let id = self.docs[i].id.clone().unwrap();
            if self.docs[i].keep_version { let _ = self.store.save_version_now(&id); }
            self.store.save(&id, &self.docs[i].doc).map_err(|e| fail("io", format!("{}: {e:#}", self.docs[i].path)))?;
            let disk = self.store.file_path(&id).and_then(|f| std::fs::read(f).ok()).unwrap_or_default();
            let d = &mut self.docs[i];
            let before = std::mem::replace(&mut d.rev, rev_of(&disk));
            (d.disk, d.dirty, d.orig) = (disk, false, d.doc.clone());
            report.push(json!({ "path": d.path, "action": "saved", "rev_before": before, "rev": d.rev }));
        }
        report.extend(self.save_tables()?);
        for p in std::mem::take(&mut self.pending) {
            match p {
                Pending::Create { key, folder } => {
                    let parent = self.find_folder(folder.as_deref())?;
                    let i = self.docs.iter().position(|d| d.key == key).unwrap();
                    let d = &self.docs[i];
                    let id = match d.kind {
                        "md" => self.store.create_markdown_in(&d.doc, "", parent.as_deref()),
                        "board" => self.store.create_board_in(&d.title, fdl_core::whiteboard::board_of(&d.doc).unwrap_or(&Default::default()), parent.as_deref()),
                        _ => self.store.create_from_in(&d.doc, parent.as_deref()),
                    }.map_err(|e| fail("io", format!("{e:#}")))?;
                    self.refresh();
                    let e = self.entry_by_id(&id).cloned();
                    let disk = self.store.file_path(&id).and_then(|f| std::fs::read(f).ok()).unwrap_or_default();
                    let d = &mut self.docs[i];
                    if let Some(e) = e { d.path = e.meta.rel.clone(); }
                    (d.id, d.rev, d.disk, d.dirty, d.orig) = (Some(id), rev_of(&disk), disk, false, d.doc.clone());
                    report.push(json!({ "path": d.path, "action": "created", "rev": d.rev }));
                }
                Pending::CreateTable { title, folder, csv } => {
                    let parent = self.find_folder(folder.as_deref())?;
                    let mut base = fdl_core::table::Base::default();
                    let t = fdl_core::table::csv::import(&mut base, &title, &csv);
                    base.tables.push(t);
                    let id = self.store.create_table_in(&title, &base, parent.as_deref()).map_err(|e| fail("io", format!("{e:#}")))?;
                    self.refresh();
                    report.push(json!({ "path": self.entry_by_id(&id).map(|e| e.meta.rel.clone()).unwrap_or_default(), "action": "created" }));
                }
                Pending::Mkdir { name, folder } => {
                    let id = self.store.create_folder(&name, folder.as_deref())?;
                    self.refresh();
                    report.push(json!({ "path": self.folder_rel(Some(&id)), "action": "folder created" }));
                }
                Pending::Rename { id, title } => {
                    let from = self.entry_by_id(&id).map(|e| e.meta.rel.clone()).unwrap_or_default();
                    let m = self.store.rename(&id, &title)?;
                    self.refresh();
                    report.push(json!({ "path": m.rel, "action": "renamed", "from": from }));
                }
                Pending::Move { id, folder } => {
                    let from = self.entry_by_id(&id).map(|e| e.meta.rel.clone()).unwrap_or_default();
                    let m = self.store.set_parent(&id, folder.as_deref())?;
                    self.refresh();
                    report.push(json!({ "path": m.rel, "action": "moved", "from": from }));
                }
                Pending::Trash { id } => {
                    let from = self.entry_by_id(&id).map(|e| e.meta.rel.clone()).unwrap_or_default();
                    self.store.delete(&id, None)?;
                    self.refresh();
                    report.push(json!({ "path": from, "action": "moved to trash" }));
                }
                Pending::Untrash { id, title } => {
                    self.store.restore(&id).map_err(|e| fail("not_found", format!("{title}: {e:#}")))?;
                    self.refresh();
                    let path = self.entry_by_id(&id).map(|e| e.meta.rel.clone()).unwrap_or(title);
                    report.push(json!({ "path": path, "action": "restored from trash" }));
                }
                Pending::Import { path, folder } => {
                    let parent = self.find_folder(folder.as_deref())?;
                    let id = crate::library::import_file(&mut self.store, &self.root, &path, parent.as_deref())?;
                    self.refresh();
                    let rel = self.entry_by_id(&id).map(|e| e.meta.rel.clone()).unwrap_or_default();
                    report.push(json!({ "path": rel, "action": "imported", "source": path.display().to_string() }));
                }
                Pending::RestoreTable { id, version } => {
                    self.store.restore_version(&id, &version).map_err(|e| fail("io", format!("{e:#}")))?;
                    let path = self.entry_by_id(&id).map(|e| e.meta.rel.clone()).unwrap_or_default();
                    report.push(json!({ "path": path, "action": "restored version", "to": version }));
                }
            }
        }
        Ok(json!(report))
    }

    fn describe(&self, p: &Pending) -> Option<Value> {
        let rel = |id: &str| self.entry_by_id(id).map(|e| e.meta.rel.clone()).unwrap_or_default();
        Some(match p {
            Pending::Create { .. } => return None,
            Pending::CreateTable { title, folder, .. } => json!({ "path": join(folder.as_deref().unwrap_or(""), title), "action": "would create 数据表" }),
            Pending::Mkdir { name, folder } => json!({ "path": join(&self.folder_rel(folder.as_deref()), name), "action": "would create folder" }),
            Pending::Rename { id, title } => json!({ "path": rel(id), "action": "would rename", "to": title }),
            Pending::Move { id, folder } => json!({ "path": rel(id), "action": "would move", "to": self.folder_rel(folder.as_deref()) }),
            Pending::Trash { id } => json!({ "path": rel(id), "action": "would move to trash" }),
            Pending::Untrash { title, .. } => json!({ "path": title, "action": "would restore from trash" }),
            Pending::Import { path, folder } => json!({ "path": path.display().to_string(), "action": "would import", "to": folder.clone().unwrap_or_else(|| "/".into()) }),
            Pending::RestoreTable { id, version } => json!({ "path": rel(id), "action": "would restore version", "to": version }),
        })
    }
}

pub(crate) fn join(dir: &str, name: &str) -> String { if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") } }

pub(crate) fn line_text(line: &[(String, Option<String>)]) -> String { line.iter().map(|(s, _)| s.as_str()).collect::<String>().trim().to_string() }


/// Around the first match, about 120 characters.
fn snippet(text: &str, q: &str) -> String {
    let flat = text.replace('\n', " ");
    let low = flat.to_lowercase();
    let Some(at) = low.find(q) else { return blocks::clip(&flat, 120) };
    let chars: Vec<char> = flat.chars().collect();
    let at = low[..at].chars().count();
    let (from, to) = (at.saturating_sub(50), (at + q.chars().count() + 70).min(chars.len()));
    format!("{}{}{}", if from > 0 { "…" } else { "" }, chars[from..to].iter().collect::<String>(), if to < chars.len() { "…" } else { "" })
}

fn sub_text(b: &Block) -> String {
    let mut s = blocks::text_of(b);
    for c in &b.children { s.push('\n'); s.push_str(&sub_text(c)); }
    s
}

fn sub_chars(b: &Block) -> usize { sub_text(b).chars().count() }

/// What a save would change, as Markdown lines added and removed.
pub(crate) fn diff(old: &Document, new: &Document, root: &Path) -> Vec<String> {
    let md = |d: &Document| fdl_core::markdown::clipboard(Some(&d.title.text), &d.blocks, root);
    diff_lines(&fdl_core::diff::diff(&md(old), &md(new), true))
}

/// Changed lines as `+ added` / `- removed`, at most 200.
pub(crate) fn diff_lines(lines: &[fdl_core::diff::Line]) -> Vec<String> {
    let mut out: Vec<String> = lines.iter().filter(|l| l.kind != fdl_core::diff::Kind::Same).map(|l| format!("{} {}", if l.kind == fdl_core::diff::Kind::Added { "+" } else { "-" }, l.text)).collect();
    if out.len() > 200 { let n = out.len() - 200; out.truncate(200); out.push(format!("… {n} more lines")); }
    out
}
