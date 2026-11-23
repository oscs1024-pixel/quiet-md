//! One CLI invocation or one `quiet run`: the library open, documents read
//! in as needed, every change held in memory until `commit`. Nothing is
//! written if the script fails; at commit, a document changed on disk since
//! it was read (the app saved it, say) stops the whole commit.
//!
//! Every operation is `call(op, args)` with JSON in and out, so the shell
//! commands and the script API are the same code.
//!
//! The library is the app's 工作区: every root folder it has open
//! (`Workspace`, the same list in the same order), each a library of its
//! own. Documents are found across them; ids are unique across roots (W14).
//! `--root` narrows a run to one, `--library DIR` works on that folder alone.

use std::path::{Path, PathBuf};

use fdl_core::editor::core::{Core, DropPlace};
use fdl_core::model::{Block, BlockId, BlockKind, Document, RichText};
use fdl_core::store::{DocKind, Entry, Store};
use fdl_core::workspace::{RootEntry, Workspace};
use serde_json::{Value, json};

use crate::blocks;
use crate::fail::{R, fail};

pub struct Session {
    pub ws: Workspace,
    /// `--root`: the one root (its key) this run sees.
    pub only: Option<String>,
    /// Every document and folder of the roots this run sees, each with its root's key.
    pub(crate) entries: Vec<Entry>,
    pub(crate) docs: Vec<Open>,
    pub(crate) tables: Vec<crate::tables::OpenTable>,
    pub(crate) pending: Vec<Pending>,
    /// Pictures from outside the library new blocks show: copied into their
    /// root's assets at commit, before the documents that point at them are saved.
    pub(crate) assets: Vec<(PathBuf, PathBuf)>,
    /// In `quiet run`: nothing outside the library (no `import`, no local pictures).
    pub sandbox: bool,
}

pub(crate) struct Open {
    pub(crate) key: String,
    /// The root it is in (or goes to).
    pub(crate) root: String,
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
    CreateTable { root: String, title: String, folder: Option<String>, csv: String },
    Mkdir { root: String, name: String, folder: Option<String> },
    Rename { id: String, title: String },
    Move { id: String, folder: Option<String> },
    Trash { id: String },
    Untrash { root: String, id: String, title: String },
    Import { root: String, path: PathBuf, folder: Option<String> },
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

/// A root's library as it is: one whose folder is gone is an error, never
/// made anew, and copies a crash left stay for the app to restore and report.
fn open_root(e: &RootEntry) -> anyhow::Result<Store> {
    anyhow::ensure!(e.path.is_dir(), "the folder is not there");
    anyhow::ensure!(!Store::flat_at(&e.path), "it still has the old layout: open it in Quiet.md once");
    Store::open_for_tools(e.path.clone())
}

/// Root `key`'s library, made the one image and 数据表 paths resolve
/// against (as the app does for the document in front).
fn lib_in<'a>(ws: &'a mut Workspace, key: &str) -> R<&'a mut Store> {
    let r = ws.roots.iter_mut().find(|r| r.key == key).ok_or_else(|| fail("no_root", "that root is no longer open"))?;
    let (path, store) = (&r.path, r.store.as_mut());
    let s = store.ok_or_else(|| fail("unavailable", format!("{} cannot be opened", path.display())))?;
    Store::set_active_root(path);
    Ok(s)
}

impl Session {
    /// `library`: that folder alone; else the roots the app has open here
    /// (`QUIET_MD_HOME`: that folder alone). `root`: narrow to one of them.
    pub fn open(library: Option<&str>, root: Option<&str>) -> R<Session> {
        let ws = match library {
            Some(dir) => {
                let dir = PathBuf::from(dir);
                if !Store::is_library(&dir) {
                    return Err(fail("no_library", format!("{} is not a Quiet.md library", dir.display())).hint("pass a folder Quiet.md has open, or leave out --library to use every folder it has open"));
                }
                if Store::flat_at(&dir) {
                    return Err(fail("old_library", "this library still has the old layout").hint("open it in Quiet.md once to move it to real folders"));
                }
                Workspace::open_with(vec![RootEntry { key: "library".into(), path: dir, bookmark: None, added: 0 }], true, open_root)
            }
            // `fixed`: roots.json stays the app's to write.
            None => Workspace::open_with(Workspace::entries().0, true, open_root),
        };
        if ws.roots.is_empty() {
            return Err(fail("no_library", "Quiet.md has no folders open on this computer")).map_err(|f| f.hint("add a folder in Quiet.md (the + beside the sidebar's folders), or pass --library DIR"));
        }
        let mut s = Session { ws, only: None, entries: vec![], docs: vec![], tables: vec![], pending: vec![], assets: vec![], sandbox: false };
        if let Some(spec) = root { s.only = Some(s.pick_root(spec)?); }
        s.refresh();
        Ok(s)
    }

    /// The root `spec` names: its name (any case), its folder's path, or its key.
    fn pick_root(&self, spec: &str) -> R<String> {
        let roots = &self.ws.roots;
        let path = PathBuf::from(spec);
        let same = |p: &Path| p == path || (path.is_absolute() && p.canonicalize().ok().zip(path.canonicalize().ok()).is_some_and(|(a, b)| a == b));
        let mut hits: Vec<&fdl_core::workspace::Root> = roots.iter().filter(|r| r.key == spec || same(&r.path)).collect();
        if hits.is_empty() { hits = roots.iter().filter(|r| r.name().to_lowercase() == spec.trim().to_lowercase()).collect(); }
        let r = match hits.as_slice() {
            [r] => r,
            [] => return Err(fail("no_root", format!("no root {spec:?}")).hint(format!("roots: {} (`quiet roots` lists them)", self.root_names()))),
            many => return Err(fail("ambiguous", format!("{} roots are called {spec:?}: {}", many.len(), many.iter().map(|r| r.path.display().to_string()).collect::<Vec<_>>().join(", "))).hint("pass --root with the folder's path or its key (`quiet roots`)")),
        };
        if let Some(e) = &r.error { return Err(fail("unavailable", format!("{} ({}) cannot be opened: {e}", r.name(), r.path.display())).hint("`quiet roots` shows every root and whether it opens")); }
        Ok(r.key.clone())
    }

    fn root_names(&self) -> String { self.ws.roots.iter().map(|r| r.name()).collect::<Vec<_>>().join(", ") }

    pub(crate) fn refresh(&mut self) {
        self.entries = self.ws.list();
        if let Some(k) = &self.only { self.entries.retain(|e| &e.root == k); }
    }

    /// Whether this run sees more than one root (and says which a document is in).
    pub fn multi(&self) -> bool { self.only.is_none() && self.ws.roots.len() > 1 }

    /// What the sidebar calls root `key`.
    pub(crate) fn root_name(&self, key: &str) -> String { self.ws.root(key).map(|r| r.name()).unwrap_or_default() }

    /// Root `key`'s folder.
    pub(crate) fn root_dir(&self, key: &str) -> PathBuf { self.ws.root(key).map(|r| r.path.clone()).unwrap_or_default() }

    /// The root document or folder `id` is in.
    pub(crate) fn root_of(&self, id: &str) -> R<String> {
        self.entry_by_id(id).map(|e| e.root.clone()).ok_or_else(|| fail("not_found", format!("no document or folder {id}")))
    }

    pub(crate) fn lib(&mut self, key: &str) -> R<&mut Store> { lib_in(&mut self.ws, key) }

    /// The library `id` is in.
    pub(crate) fn lib_of(&mut self, id: &str) -> R<&mut Store> { let k = self.root_of(id)?; self.lib(&k) }

    /// Where something new goes: into folder `spec` (and its root) when given;
    /// else `--root`, else the first root that opens, as in the app.
    pub(crate) fn landing(&self, spec: Option<&str>) -> R<(String, Option<String>)> {
        if let Some(id) = self.find_folder(spec)? { return Ok((self.root_of(&id)?, Some(id))); }
        let key = self.only.clone().or_else(|| self.ws.first().map(|r| r.key.clone()));
        Ok((key.ok_or_else(|| fail("no_library", "none of Quiet.md's folders opens")).map_err(|f| f.hint("`quiet roots` says why; or pass --library DIR"))?, None))
    }

    /// `rel` of entry `e`, with its root when this run sees several.
    pub(crate) fn label(&self, e: &Entry) -> String {
        let rel = e.meta.rel.trim_end_matches('/');
        if self.multi() { format!("{rel} (in {})", self.root_name(&e.root)) } else { rel.to_string() }
    }

    /// `ambiguous` for several entries matching `s`: in several roots, `--root` tells them apart.
    fn ambiguous(&self, what: &str, s: &str, many: &[&Entry]) -> crate::fail::Fail {
        let mut roots: Vec<String> = many.iter().map(|e| self.root_name(&e.root)).collect();
        roots.dedup();
        let list = many.iter().map(|e| if roots.len() > 1 { format!("{} (in {})", e.meta.rel.trim_end_matches('/'), self.root_name(&e.root)) } else { e.meta.rel.trim_end_matches('/').to_string() }).collect::<Vec<_>>().join(", ");
        let f = fail("ambiguous", format!("{} {what} match {s:?}: {list}", many.len()));
        if roots.len() > 1 { f.hint(format!("they are in different roots: add --root ROOT (e.g. --root {:?}), or pass the document's id", roots[0])) } else { f.hint("pass the path") }
    }

    fn docs_list(&self) -> impl Iterator<Item = &Entry> { self.entries.iter().filter(|e| e.meta.folder.is_none() && !e.meta.local()) }

    pub(crate) fn entry_by_id(&self, id: &str) -> Option<&Entry> { self.entries.iter().find(|e| e.id == id) }

    /// A document by path (`工作/周报.xml`, with or without the extension) or title.
    pub(crate) fn find_entry(&self, spec: &str) -> R<String> {
        let s = spec.trim().trim_start_matches("./").trim_start_matches('/');
        // An id finds its document in whichever root it is (W14).
        if let Some(e) = self.docs_list().find(|e| e.id == s) { return Ok(e.id.clone()); }
        let by_path: Vec<&Entry> = self.docs_list().filter(|e| e.meta.rel == s).collect();
        if by_path.len() == 1 { return Ok(by_path[0].id.clone()); }
        if by_path.len() > 1 { return Err(self.ambiguous("documents", s, &by_path)); }
        let stem = |rel: &str| rel.rsplit_once('.').map_or(rel.to_string(), |(a, _)| a.to_string());
        let by_stem: Vec<&Entry> = self.docs_list().filter(|e| stem(&e.meta.rel) == s).collect();
        if by_stem.len() == 1 { return Ok(by_stem[0].id.clone()); }
        // The title, or the file's name without folder and extension (titles
        // with characters a file name cannot hold differ from it).
        let by_title: Vec<&Entry> = self.docs_list().filter(|e| e.title == s || stem(e.meta.rel.rsplit('/').next().unwrap_or("")) == s).collect();
        if by_title.len() == 1 { return Ok(by_title[0].id.clone()); }
        let many = if by_stem.len() > 1 { by_stem } else { by_title };
        if many.len() > 1 { return Err(self.ambiguous("documents", s, &many)); }
        let low = s.to_lowercase();
        let near: Vec<String> = self.docs_list().filter(|e| e.meta.rel.to_lowercase().contains(&low) || e.title.to_lowercase().contains(&low)).take(5).map(|e| self.label(e)).collect();
        let f = fail("not_found", format!("no document {s:?}"));
        Err(if near.is_empty() { f.hint("`quiet ls -r` lists every document") } else { f.hint(format!("did you mean: {}", near.join(", "))) })
    }

    /// A folder by path or name, in any root this run sees; `""` / `/` the top.
    pub(crate) fn find_folder(&self, spec: Option<&str>) -> R<Option<String>> { self.find_folder_in(None, spec) }

    /// `find_folder` in root `root` only (when given).
    pub(crate) fn find_folder_in(&self, root: Option<&str>, spec: Option<&str>) -> R<Option<String>> {
        let s = spec.unwrap_or("").trim().trim_matches('/');
        if s.is_empty() { return Ok(None); }
        let folders: Vec<&Entry> = self.entries.iter().filter(|e| e.meta.folder.is_some() && !e.meta.local() && root.is_none_or(|r| e.root == r)).collect();
        let at: Vec<&Entry> = folders.iter().copied().filter(|e| e.meta.rel.trim_end_matches('/') == s).collect();
        let named: Vec<&Entry> = if at.is_empty() { folders.iter().copied().filter(|e| e.title == s || e.meta.rel.trim_end_matches('/').rsplit('/').next() == Some(s)).collect() } else { at };
        match named.len() {
            1 => Ok(Some(named[0].id.clone())),
            0 => Err(fail("not_found", format!("no folder {s:?}")).hint("`quiet ls -r --kind folder` lists folders; `quiet mkdir` makes one")),
            _ => Err(self.ambiguous("folders", s, &named)),
        }
    }

    pub(crate) fn folder_rel(&self, id: Option<&str>) -> String { id.and_then(|i| self.entry_by_id(i)).map(|e| e.meta.rel.trim_end_matches('/').to_string()).unwrap_or_default() }

    /// The open document a spec names (its session key, its path, its title), read in on first use.
    fn doc(&mut self, spec: &str) -> R<usize> {
        // A document made in this run is known by its path until commit.
        if let Some(i) = self.docs.iter().position(|d| d.key == spec || (d.id.is_none() || !self.multi()) && d.path == spec) { return Ok(i); }
        let id = self.find_entry(spec)?;
        if let Some(i) = self.docs.iter().position(|d| d.id.as_deref() == Some(id.as_str())) { return Ok(i); }
        let e = self.entry_by_id(&id).ok_or_else(|| fail("not_found", spec.to_string()))?;
        let (path, title, kind, root) = (e.meta.rel.clone(), e.title.clone(), kind_of(e), e.root.clone());
        let store = self.lib(&root)?;
        let file = store.file_path(&id).ok_or_else(|| fail("not_found", spec.to_string()))?;
        let disk = std::fs::read(&file)?;
        // As an editor would: the store then keeps the text a save replaces as a version.
        let doc = store.load_for_edit(&id, false).map_err(|e| fail("parse", format!("{path}: {e:#}")))?;
        let key = format!("d{}", self.docs.len() + 1);
        self.docs.push(Open { key, root, id: Some(id), path, title, kind, orig: doc.clone(), doc, rev: rev_of(&disk), disk, dirty: false, keep_version: false });
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
        json!({ "key": d.key, "path": d.path, "root": self.root_name(&d.root), "title": d.title, "kind": d.kind, "rev": d.rev, "block_count": blocks::items(&d.doc).len(), "new": d.id.is_none() })
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
            "mv" => {
                let i = self.doc_arg(a)?;
                let id = self.saved_id(i)?;
                let root = self.docs[i].root.clone();
                // Within its root: each root is a library of its own (its 回收站, its versions).
                let folder = match self.find_folder_in(Some(&root), arg(a, "folder")) {
                    Err(e) if e.code == "not_found" && self.find_folder(arg(a, "folder")).is_ok_and(|f| f.is_some()) => return Err(fail("cross_root", format!("{} is in {}; that folder is in another root", self.docs[i].path, self.root_name(&root))).hint("moving between roots is not supported yet: create the document there (`quiet read` then `quiet new --in`) and `quiet rm` this one")),
                    r => r?,
                };
                self.pending.push(Pending::Move { id, folder });
                Ok(json!({ "path": self.docs[i].path, "to": arg(a, "folder").unwrap_or("/") }))
            }
            "trash" => { let i = self.doc_arg(a)?; let id = self.saved_id(i)?; self.pending.push(Pending::Trash { id }); Ok(json!({ "path": self.docs[i].path, "trash": true })) }
            "links" => self.links(a),
            "table" => self.table(a),
            "check" => self.check(),
            "roots" => self.roots(),
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
        let in_root = folder.as_deref().and_then(|f| self.entry_by_id(f)).map(|e| e.root.clone());
        let (deep, kind) = (flag(a, "recursive"), arg(a, "kind"));
        let mut out: Vec<&Entry> = self.entries.iter().filter(|e| !e.meta.local() && in_root.as_ref().is_none_or(|r| &e.root == r))
            .filter(|e| { let rel = e.meta.rel.trim_end_matches('/'); let rest = if base.is_empty() { Some(rel) } else { rel.strip_prefix(&format!("{base}/")) }; rest.is_some_and(|r| !r.is_empty() && (deep || !r.contains('/'))) })
            .filter(|e| kind.is_none_or(|k| k == kind_of(e))).collect();
        // Root by root, in the sidebar's order.
        let order = |e: &Entry| self.ws.roots.iter().position(|r| r.key == e.root).unwrap_or(0);
        out.sort_by(|a, b| (order(a), a.meta.folder.is_none(), &a.meta.rel).cmp(&(order(b), b.meta.folder.is_none(), &b.meta.rel)));
        Ok(json!(out.iter().map(|e| {
            let mut v = json!({ "path": e.meta.rel.trim_end_matches('/'), "root": self.root_name(&e.root), "id": e.id, "title": e.title, "kind": kind_of(e), "modified": date(e.meta.modified) });
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
        let in_root = folder.as_deref().and_then(|f| self.entry_by_id(f)).map(|e| e.root.clone());
        let mut hits: Vec<(usize, String, String)> = self.docs_list()
            .filter(|e| (base.is_empty() || e.meta.rel.starts_with(&format!("{base}/"))) && in_root.as_ref().is_none_or(|r| &e.root == r))
            .filter_map(|e| {
                let (title, body) = (e.title.to_lowercase(), e.body.to_lowercase());
                if !words.iter().all(|w| title.contains(w) || body.contains(w)) { return None; }
                let often: usize = words.iter().map(|w| body.matches(w).count().min(50)).sum();
                let score = if title.contains(&q) { 4000 } else if words.iter().all(|w| title.contains(w)) { 3000 } else if body.contains(&q) { 2000 } else { 1000 } + often;
                Some((score, e.id.clone(), e.meta.rel.clone()))
            }).collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(&b.2)));
        let total = hits.len();
        let mut out = vec![];
        for (_, id, _) in hits.into_iter().take(limit) {
            let i = match self.doc(&id) { Ok(i) => i, Err(_) => continue };
            let (path, title, root) = (self.docs[i].path.clone(), self.docs[i].title.clone(), self.root_name(&self.docs[i].root));
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
            let mut r = json!({ "path": path, "root": root, "title": title, "rev": self.docs[i].rev, "hits": shown });
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
            "md" | "markdown" => blocks::wiki_back(&fdl_core::markdown::clipboard(None, &chosen, &self.root_dir(&d.root)), &names),
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
        let (markdown, root) = (self.docs[i].kind == "md", self.docs[i].root.clone());
        self.link(&mut b, markdown, &root)?;
        Ok(b)
    }

    /// `[[…]]` and links to library files in new blocks as mentions, and
    /// pictures given by a path outside the library copied in (at commit).
    /// `markdown`: for a Markdown file, which keeps `[[…]]` as it is written
    /// (its links are by name), so they are only checked.
    /// `root`: the root the document is in, whose assets take the pictures.
    fn link(&mut self, list: &mut [Block], markdown: bool, root: &str) -> R<()> {
        let dir = self.root_dir(root);
        let assets = Store::assets_in(&dir);
        let mut err = None;
        blocks::each_block(list, &mut |b| {
            let BlockKind::Image { src, .. } = &mut b.kind else { return };
            let Some((from, to)) = crate::library::picture_copy(&dir, &assets, src, self.assets.len()) else { return };
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
            Some(t) => { let mut probe = vec![Block::text("")]; probe[0].text = blocks::inline_text(t, false); let (md, root) = (self.docs[i].kind == "md", self.docs[i].root.clone()); self.link(&mut probe, md, &root)?; Some((t.to_string(), probe.remove(0).text)) }
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
        let made = self.pending.iter().find_map(|p| match p { Pending::Mkdir { root, name, folder: f } if folder.as_deref().map(|s| s.trim_matches('/')) == Some(&join(&self.folder_rel(f.as_deref()), name)) => Some(root.clone()), _ => None });
        let (root, base) = match made {
            Some(root) => (root, folder.clone().unwrap_or_default().trim_matches('/').to_string()),
            None => { let (root, f) = self.landing(folder.as_deref())?; let base = self.folder_rel(f.as_deref()); (root, base) }
        };
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
            let name = self.root_name(&root);
            self.pending.push(Pending::CreateTable { root, title, folder, csv });
            return Ok(json!({ "path": path, "root": name, "kind": "table", "new": true }));
        }
        let doc = if kind == "board" { fdl_core::whiteboard::file_doc(fdl_core::whiteboard::Board::default(), &title) } else {
            let mut doc = Document { title: RichText::plain(&title), ..Default::default() };
            if let Some(md) = arg(a, "md") { let mut b = blocks::from_markdown(md); self.link(&mut b, kind == "md", &root)?; if !b.is_empty() { doc.blocks = b; } }
            doc
        };
        let key = format!("d{}", self.docs.len() + 1);
        self.docs.push(Open { key: key.clone(), root, id: None, path, title, kind, orig: Document::default(), doc, disk: vec![], rev: String::new(), dirty: true, keep_version: false });
        self.pending.push(Pending::Create { key, folder });
        Ok(self.info(self.docs.len() - 1))
    }

    fn mkdir(&mut self, a: &Value) -> R<Value> {
        let name = need(a, "name")?.trim().to_string();
        let (root, f) = self.landing(arg(a, "folder"))?;
        let path = join(&self.folder_rel(f.as_deref()), &fdl_core::store::file_name_for(&name));
        let in_root = self.root_name(&root);
        self.pending.push(Pending::Mkdir { root, name, folder: f });
        Ok(json!({ "path": path, "root": in_root, "mkdir": true }))
    }

    fn links(&mut self, a: &Value) -> R<Value> {
        let i = self.doc_arg(a)?;
        let id = self.docs[i].id.clone();
        let doc = &self.docs[i].doc;
        let mut out: Vec<Value> = doc.doc_links().iter().map(|l| {
            let to = self.entry_by_id(&l.target).map(|e| self.label(e));
            json!({ "to": to, "broken": to.is_none(), "text": line_text(&l.line) })
        }).collect();
        // `[[…]]` as a Markdown file keeps them: links by name.
        for (target, text) in blocks::wiki_links(doc) {
            let to = self.find_entry(&target).ok().and_then(|id| self.entry_by_id(&id)).map(|e| self.label(e));
            out.push(json!({ "to": to, "broken": to.is_none(), "text": text }));
        }
        let back: Vec<Value> = match &id {
            Some(id) => self.docs_list().filter(|e| e.links.iter().any(|l| &l.target == id)).map(|e| json!({ "from": self.label(e), "title": e.title, "text": e.links.iter().filter(|l| &l.target == id).map(|l| line_text(&l.line)).collect::<Vec<_>>() })).collect(),
            None => vec![],
        };
        Ok(json!({ "path": self.docs[i].path, "out": out, "back": back }))
    }

    /// Whole-library health: files that do not parse, broken @mentions, empty pages, unused assets.
    fn check(&mut self) -> R<Value> {
        let mut issues = vec![];
        let mut live: std::collections::HashMap<String, Vec<String>> = Default::default();
        let ids: Vec<(String, String, String, &'static str)> = self.docs_list().map(|e| (e.id.clone(), e.root.clone(), self.label(e), kind_of(e))).collect();
        for (id, root, rel, kind) in ids {
            let live = live.entry(root.clone()).or_default();
            match self.lib(&root)?.load(&id) {
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
        // Each root's assets against its own documents.
        let roots: Vec<String> = self.ws.roots.iter().filter(|r| r.store.is_some() && self.only.as_ref().is_none_or(|k| k == &r.key)).map(|r| r.key.clone()).collect();
        for root in roots {
            let at = if self.multi() { format!(" (in {})", self.root_name(&root)) } else { String::new() };
            match Store::unused_assets_at(self.root_dir(&root), live.remove(&root).unwrap_or_default(), vec![]) {
                Ok(list) => for (name, size) in list { issues.push(json!({ "path": format!("{}/{name}{at}", fdl_core::store::ASSETS), "code": "unused_asset", "message": format!("not used by any document ({size} bytes)") })); },
                Err(e) => issues.push(json!({ "path": format!("{}{at}", fdl_core::store::ASSETS), "code": "assets_unchecked", "message": format!("{e:#}") })),
            }
        }
        Ok(json!({ "documents": self.docs_list().count(), "issues": issues }))
    }

    pub fn changed(&self) -> bool { !self.assets.is_empty() || !self.pending.is_empty() || self.docs.iter().any(|d| d.dirty) || self.tables.iter().any(|t| t.dirty()) }

    /// Write what changed: documents first, then creations, folders, renames,
    /// moves and trashing in the order asked. `dry`: only say what would happen.
    pub fn commit(&mut self, dry: bool) -> R<Value> {
        let mut report = vec![];
        for d in self.docs.iter().filter(|d| d.dirty && d.id.is_some()) {
            let file = lib_in(&mut self.ws, &d.root)?.file_path(d.id.as_deref().unwrap()).ok_or_else(|| fail("conflict", format!("{} is gone", d.path)))?;
            if std::fs::read(&file).ok().as_deref() != Some(&d.disk[..]) {
                return Err(fail("conflict", format!("{} changed on disk while this ran; nothing was written", d.path)).hint("read it again and redo the edit"));
            }
        }
        self.check_tables()?;
        if dry {
            for (from, to) in &self.assets { report.push(json!({ "path": self.in_root(to), "action": "would copy picture", "source": from.display().to_string() })); }
            for d in self.docs.iter().filter(|d| d.dirty) {
                let mut v = json!({ "path": d.path, "action": if d.id.is_some() { "would save" } else { "would create" }, "diff": diff(&d.orig, &d.doc, &self.root_dir(&d.root)) });
                if d.id.is_none() { v["root"] = json!(self.root_name(&d.root)); }
                report.push(v);
            }
            report.extend(self.describe_tables());
            for p in &self.pending { if let Some(v) = self.describe(p) { report.push(v); } }
            return Ok(json!(report));
        }
        for (from, to) in std::mem::take(&mut self.assets) {
            if let Some(dir) = to.parent() { std::fs::create_dir_all(dir)?; }
            std::fs::copy(&from, &to).map_err(|e| fail("io", format!("{}: {e}", from.display())))?;
            report.push(json!({ "path": self.in_root(&to), "action": "copied picture", "source": from.display().to_string() }));
        }
        for i in 0..self.docs.len() {
            if !(self.docs[i].dirty && self.docs[i].id.is_some()) { continue; }
            let id = self.docs[i].id.clone().unwrap();
            let store = lib_in(&mut self.ws, &self.docs[i].root)?;
            if self.docs[i].keep_version { let _ = store.save_version_now(&id); }
            store.save(&id, &self.docs[i].doc).map_err(|e| fail("io", format!("{}: {e:#}", self.docs[i].path)))?;
            let disk = store.file_path(&id).and_then(|f| std::fs::read(f).ok()).unwrap_or_default();
            let d = &mut self.docs[i];
            let before = std::mem::replace(&mut d.rev, rev_of(&disk));
            (d.disk, d.dirty, d.orig) = (disk, false, d.doc.clone());
            report.push(json!({ "path": d.path, "action": "saved", "rev_before": before, "rev": d.rev }));
        }
        report.extend(self.save_tables()?);
        for p in std::mem::take(&mut self.pending) {
            match p {
                Pending::Create { key, folder } => {
                    let i = self.docs.iter().position(|d| d.key == key).unwrap();
                    let root = self.docs[i].root.clone();
                    let parent = self.find_folder_in(Some(&root), folder.as_deref())?;
                    let d = &self.docs[i];
                    let store = lib_in(&mut self.ws, &root)?;
                    let id = match d.kind {
                        "md" => store.create_markdown_in(&d.doc, "", parent.as_deref()),
                        "board" => store.create_board_in(&d.title, fdl_core::whiteboard::board_of(&d.doc).unwrap_or(&Default::default()), parent.as_deref()),
                        _ => store.create_from_in(&d.doc, parent.as_deref()),
                    }.map_err(|e| fail("io", format!("{e:#}")))?;
                    let disk = store.file_path(&id).and_then(|f| std::fs::read(f).ok()).unwrap_or_default();
                    self.refresh();
                    let e = self.entry_by_id(&id).cloned();
                    let name = self.root_name(&root);
                    let d = &mut self.docs[i];
                    if let Some(e) = e { d.path = e.meta.rel.clone(); }
                    (d.id, d.rev, d.disk, d.dirty, d.orig) = (Some(id), rev_of(&disk), disk, false, d.doc.clone());
                    report.push(json!({ "path": d.path, "root": name, "action": "created", "rev": d.rev }));
                }
                Pending::CreateTable { root, title, folder, csv } => {
                    let parent = self.find_folder_in(Some(&root), folder.as_deref())?;
                    let mut base = fdl_core::table::Base::default();
                    let t = fdl_core::table::csv::import(&mut base, &title, &csv);
                    base.tables.push(t);
                    let id = self.lib(&root)?.create_table_in(&title, &base, parent.as_deref()).map_err(|e| fail("io", format!("{e:#}")))?;
                    self.refresh();
                    report.push(json!({ "path": self.entry_by_id(&id).map(|e| e.meta.rel.clone()).unwrap_or_default(), "root": self.root_name(&root), "action": "created" }));
                }
                Pending::Mkdir { root, name, folder } => {
                    let id = self.lib(&root)?.create_folder(&name, folder.as_deref())?;
                    self.refresh();
                    report.push(json!({ "path": self.folder_rel(Some(&id)), "root": self.root_name(&root), "action": "folder created" }));
                }
                Pending::Rename { id, title } => {
                    let from = self.entry_by_id(&id).map(|e| e.meta.rel.clone()).unwrap_or_default();
                    let m = self.lib_of(&id)?.rename(&id, &title)?;
                    self.refresh();
                    report.push(json!({ "path": m.rel, "action": "renamed", "from": from }));
                }
                Pending::Move { id, folder } => {
                    let from = self.entry_by_id(&id).map(|e| e.meta.rel.clone()).unwrap_or_default();
                    let m = self.lib_of(&id)?.set_parent(&id, folder.as_deref())?;
                    self.refresh();
                    report.push(json!({ "path": m.rel, "action": "moved", "from": from }));
                }
                Pending::Trash { id } => {
                    let from = self.entry_by_id(&id).map(|e| e.meta.rel.clone()).unwrap_or_default();
                    self.lib_of(&id)?.delete(&id, None)?;
                    self.refresh();
                    report.push(json!({ "path": from, "action": "moved to trash" }));
                }
                Pending::Untrash { root, id, title } => {
                    self.lib(&root)?.restore(&id).map_err(|e| fail("not_found", format!("{title}: {e:#}")))?;
                    self.refresh();
                    let path = self.entry_by_id(&id).map(|e| e.meta.rel.clone()).unwrap_or(title);
                    report.push(json!({ "path": path, "action": "restored from trash" }));
                }
                Pending::Import { root, path, folder } => {
                    let parent = self.find_folder_in(Some(&root), folder.as_deref())?;
                    let dir = self.root_dir(&root);
                    let id = crate::library::import_file(self.lib(&root)?, &dir, &path, parent.as_deref())?;
                    self.refresh();
                    let rel = self.entry_by_id(&id).map(|e| e.meta.rel.clone()).unwrap_or_default();
                    report.push(json!({ "path": rel, "root": self.root_name(&root), "action": "imported", "source": path.display().to_string() }));
                }
                Pending::RestoreTable { id, version } => {
                    self.lib_of(&id)?.restore_version(&id, &version).map_err(|e| fail("io", format!("{e:#}")))?;
                    let path = self.entry_by_id(&id).map(|e| e.meta.rel.clone()).unwrap_or_default();
                    report.push(json!({ "path": path, "action": "restored version", "to": version }));
                }
            }
        }
        Ok(json!(report))
    }

    /// `path` inside one of the roots, as that root sees it.
    fn in_root(&self, path: &Path) -> String {
        self.ws.roots.iter().find_map(|r| path.strip_prefix(&r.path).ok()).unwrap_or(path).to_string_lossy().replace('\\', "/")
    }

    /// The root folders, in the sidebar's order, whether they open or not.
    fn roots(&mut self) -> R<Value> {
        let first = self.ws.first().map(|r| r.key.clone());
        let mut out = vec![];
        for r in self.ws.roots.iter_mut() {
            let docs = r.store.as_mut().map(|s| s.list().iter().filter(|e| e.meta.folder.is_none() && !e.meta.local()).count());
            let mut v = json!({ "name": r.name(), "path": r.path.display().to_string(), "key": r.key, "available": r.store.is_some() });
            match (docs, &r.error) {
                (Some(n), _) => v["documents"] = json!(n),
                (None, e) => v["error"] = json!(e.clone().unwrap_or_default()),
            }
            if first.as_ref() == Some(&r.key) { v["default"] = json!(true); }
            if self.only.as_ref() == Some(&r.key) { v["selected"] = json!(true); }
            out.push(v);
        }
        Ok(json!(out))
    }

    fn describe(&self, p: &Pending) -> Option<Value> {
        let rel = |id: &str| self.entry_by_id(id).map(|e| e.meta.rel.clone()).unwrap_or_default();
        Some(match p {
            Pending::Create { .. } => return None,
            Pending::CreateTable { root, title, folder, .. } => json!({ "path": join(folder.as_deref().unwrap_or(""), title), "root": self.root_name(root), "action": "would create 数据表" }),
            Pending::Mkdir { root, name, folder } => json!({ "path": join(&self.folder_rel(folder.as_deref()), name), "root": self.root_name(root), "action": "would create folder" }),
            Pending::Rename { id, title } => json!({ "path": rel(id), "action": "would rename", "to": title }),
            Pending::Move { id, folder } => json!({ "path": rel(id), "action": "would move", "to": self.folder_rel(folder.as_deref()) }),
            Pending::Trash { id } => json!({ "path": rel(id), "action": "would move to trash" }),
            Pending::Untrash { title, .. } => json!({ "path": title, "action": "would restore from trash" }),
            Pending::Import { root, path, folder } => json!({ "path": path.display().to_string(), "root": self.root_name(root), "action": "would import", "to": folder.clone().unwrap_or_else(|| "/".into()) }),
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
