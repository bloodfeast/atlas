//! The references cost spike of PR 1 (LSP-Q1, journal:ATLAS-ANYWHERE:0014):
//! what `textDocument/references` and `callHierarchy/incomingCalls` would
//! cost per definition on a warm server. A measurement, not a check: it is
//! ignored and does nothing unless `ATLAS_LSP_REFS` is set to
//! `LANG|ROOT|TOOL|PATH|EXT`, e.g.
//! `python|H:/corpora/flask|basedpyright-langserver|H:/tools/node_modules/basedpyright|py`,
//! and run with `-- --ignored --nocapture`.

use super::*;

/// The definitions references would be asked for: types and top-level
/// callables.
fn sampled_kind(s: &Sym) -> Option<bool> {
    if !s.container.is_empty() {
        return None;
    }
    match s.kind.as_str() {
        "class" | "struct" | "interface" | "enum" | "type" => Some(false),
        "fn" => Some(true),
        _ => None,
    }
}

fn walk(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name.starts_with('.') || name == "node_modules" || name == "__pycache__" {
            continue;
        }
        if p.is_dir() {
            walk(&p, ext, out);
        } else if p.extension().is_some_and(|e| e == ext) {
            out.push(p);
        }
    }
}

/// Sends one request and waits for its answer: milliseconds and the
/// result.
fn timed(s: &mut Session<Out>, method: &str, params: Value, deadline: Instant) -> Result<(f64, Value), Stop> {
    let t = Instant::now();
    let id = s.client.request(method, params)?;
    loop {
        let (i, r) = s.client.response(deadline)?;
        if i == id {
            return Ok((t.elapsed().as_secs_f64() * 1000.0, r.unwrap_or(Value::Null)));
        }
    }
}

fn pctl(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

#[test]
#[ignore = "a measurement: set ATLAS_LSP_REFS"]
fn references_cost() {
    let Ok(spec) = std::env::var("ATLAS_LSP_REFS") else { return };
    let parts: Vec<&str> = spec.split('|').collect();
    let [lang, root, tool, path, ext] = parts[..] else { panic!("ATLAS_LSP_REFS is LANG|ROOT|TOOL|PATH|EXT") };
    let row = crate::langs::by_name(lang).expect("a language row");
    let server = row.lsp.iter().find(|s| s.tool == tool).expect("a server of the row");
    let launch = server.configured(Path::new(path));
    let root = PathBuf::from(root);
    let mut paths = Vec::new();
    walk(&root, ext, &mut paths);
    let files: Vec<(PathBuf, String)> = paths.into_iter().filter_map(|p| std::fs::read_to_string(&p).ok().map(|t| (p, t))).collect();
    let deadline = Instant::now() + Duration::from_secs(1800);
    let a = Ask { root: &root, language_id: server.language_id, init_options: server.init_options, deadline };
    let log = std::env::temp_dir().join(format!("atlas-lsp-refs-{}.log", std::process::id()));
    let mut report = String::new();
    let ran = run_server(&launch, server.args_for(), &root, &log, &a, |s| {
        let t0 = Instant::now();
        let mut defs: Vec<(usize, Sym)> = Vec::new();
        s.document_symbols(&files, deadline, |i, r| {
            if let Ok(syms) = r {
                defs.extend(syms.into_iter().filter(|y| sampled_kind(y).is_some()).map(|y| (i, y)));
            }
        })?;
        let symbols_s = t0.elapsed().as_secs_f64();
        // Every file stays open while references are asked, as an editor
        // would have them.
        for (p, t) in &files {
            s.client.notify(
                "textDocument/didOpen",
                crate::json!({"textDocument": {"uri": file_uri(p), "languageId": s.language_id.clone(), "version": 1, "text": t.clone()}}),
            )?;
        }
        let step = (defs.len() / 200).max(1);
        let sample: Vec<&(usize, Sym)> = defs.iter().step_by(step).take(200).collect();
        let at = |i: usize, y: &Sym| {
            crate::json!({"textDocument": {"uri": file_uri(&files[i].0)}, "position": {"line": y.line - 1, "character": y.col}})
        };
        let refs_at = |i: usize, y: &Sym| {
            let mut p = at(i, y);
            if let Value::Object(m) = &mut p {
                m.insert("context".into(), crate::json!({"includeDeclaration": false}));
            }
            p
        };
        // Warm-up: the first call builds the server's program; timed apart.
        let (first, _) = timed(s, "textDocument/references", refs_at(sample[0].0, &sample[0].1), deadline)?;
        let (mut refs_ms, mut hier_ms, mut refs_n) = (Vec::new(), Vec::new(), 0usize);
        for (i, y) in &sample {
            let (ms, r) = timed(s, "textDocument/references", refs_at(*i, y), deadline)?;
            refs_ms.push(ms);
            refs_n += r.as_array().map_or(0, Vec::len);
            if sampled_kind(y) == Some(true) {
                let (ms1, items) = timed(s, "textDocument/prepareCallHierarchy", at(*i, y), deadline)?;
                let item = items.as_array().and_then(|v| v.first()).cloned();
                let ms2 = match item {
                    Some(item) => timed(s, "callHierarchy/incomingCalls", crate::json!({"item": item}), deadline)?.0,
                    None => 0.0,
                };
                hier_ms.push(ms1 + ms2);
            }
        }
        let callables = defs.iter().filter(|(_, y)| sampled_kind(y) == Some(true)).count();
        let total: f64 = refs_ms.iter().sum();
        let hier_total: f64 = hier_ms.iter().sum();
        let mean = total / refs_ms.len().max(1) as f64;
        let hier_mean = hier_total / hier_ms.len().max(1) as f64;
        report = format!(
            "server {} {}; {} files, documentSymbol {:.1} s; {} types and top-level callables ({} callables)\n\
             first references call (warm-up) {:.0} ms\n\
             references: {} sampled, p50 {:.1} ms, p95 {:.1} ms, total {:.1} s, {} locations; projected all {} defs {:.1} s\n\
             callHierarchy (prepare + incomingCalls): {} callables sampled, p50 {:.1} ms, p95 {:.1} ms, total {:.1} s; projected all {} callables {:.1} s\n",
            s.info.0,
            s.info.1,
            files.len(),
            symbols_s,
            defs.len(),
            callables,
            first,
            refs_ms.len(),
            pctl(&mut refs_ms.clone(), 0.5),
            pctl(&mut refs_ms.clone(), 0.95),
            total / 1000.0,
            refs_n,
            defs.len(),
            mean * defs.len() as f64 / 1000.0,
            hier_ms.len(),
            pctl(&mut hier_ms.clone(), 0.5),
            pctl(&mut hier_ms.clone(), 0.95),
            hier_total / 1000.0,
            callables,
            hier_mean * callables as f64 / 1000.0,
        );
        Ok(())
    })
    .expect("the server starts");
    println!("{report}ran: {:.1} s, stop {:?}, killed {}", ran.seconds, ran.stop, ran.killed);
    assert!(ran.stop.is_none(), "log {}", log.display());
}
