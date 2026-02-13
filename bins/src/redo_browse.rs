use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use redo_core::logs::Log;

use rusqlite::{Connection, OpenFlags};

#[derive(Debug, Clone)]
struct Node {
    id: i64,
    name: String,
    is_generated: bool,
}

#[derive(Debug, Clone, Copy)]
struct Edge {
    other: i64,
    mode: char, // 'm' or 'c'
}

#[derive(Debug)]
struct Graph {
    base: PathBuf,
    nodes_by_id: HashMap<i64, Node>,
    id_by_name: HashMap<String, i64>,
    forward: HashMap<i64, Vec<Edge>>, // target -> deps (sources)
    reverse: HashMap<i64, Vec<Edge>>, // source -> targets
}

#[derive(Debug, Clone, Copy)]
enum Dir {
    Forward,
    Reverse,
    Both,
}

impl Dir {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "forward" | "fwd" => Some(Self::Forward),
            "reverse" | "rev" => Some(Self::Reverse),
            "both" => Some(Self::Both),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Reverse => "reverse",
            Self::Both => "both",
        }
    }
}

impl Graph {
    fn load(base: PathBuf) -> anyhow::Result<Self> {
        let dbfile = base.join(".redo").join("db.sqlite3");
        if !dbfile.exists() {
            anyhow::bail!(
                "redo-browse: no .redo/db.sqlite3 found under {:?} (run a build first?)",
                base
            );
        }

        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY;
        let db = Connection::open_with_flags(&dbfile, flags)?;
        db.busy_timeout(Duration::from_millis(500))?;

        let mut nodes_by_id: HashMap<i64, Node> = HashMap::new();
        let mut id_by_name: HashMap<String, i64> = HashMap::new();
        {
            let mut stmt = db.prepare("select rowid, name, is_generated from Files")?;
            let rows = stmt.query_map([], |r| {
                let id: i64 = r.get(0)?;
                let name: String = r.get(1)?;
                let is_generated: Option<i64> = r.get(2)?;
                Ok(Node {
                    id,
                    name,
                    is_generated: is_generated.unwrap_or(0) != 0,
                })
            })?;
            for row in rows {
                let n = row?;
                id_by_name.insert(n.name.clone(), n.id);
                nodes_by_id.insert(n.id, n);
            }
        }

        let mut forward: HashMap<i64, Vec<Edge>> = HashMap::new();
        let mut reverse: HashMap<i64, Vec<Edge>> = HashMap::new();
        {
            let mut stmt = db.prepare(
                "select target, source, mode, delete_me from Deps",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let target: i64 = r.get(0)?;
                let source: i64 = r.get(1)?;
                let mode_s: String = r.get(2)?;
                let delete_me: Option<i64> = r.get(3)?;
                if delete_me.unwrap_or(0) != 0 {
                    continue;
                }
                let mode = mode_s.chars().next().unwrap_or('m');

                forward.entry(target).or_default().push(Edge { other: source, mode });
                reverse.entry(source).or_default().push(Edge { other: target, mode });
            }
        }

        for v in forward.values_mut() {
            v.sort_by_key(|e| e.other);
            v.dedup_by_key(|e| e.other);
        }
        for v in reverse.values_mut() {
            v.sort_by_key(|e| e.other);
            v.dedup_by_key(|e| e.other);
        }

        Ok(Self {
            base,
            nodes_by_id,
            id_by_name,
            forward,
            reverse,
        })
    }

    fn search(&self, q: &str, limit: usize) -> Vec<&Node> {
        let q = q.trim();
        if q.is_empty() {
            return Vec::new();
        }
        let q_lc = q.to_ascii_lowercase();
        let mut out: Vec<&Node> = Vec::new();
        for n in self.nodes_by_id.values() {
            if n.name.to_ascii_lowercase().contains(&q_lc) {
                out.push(n);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out.truncate(limit);
        out
    }

    fn suggest(&self, limit: usize) -> Vec<&Node> {
        let mut out_ids: Vec<i64> = Vec::new();
        let mut seen: HashSet<i64> = HashSet::new();

        // Common entry points (best-effort).
        for name in ["all", "cmake", "install", "clean"] {
            if let Some(id) = self.id_by_name.get(name).copied() {
                if seen.insert(id) {
                    out_ids.push(id);
                }
            }
        }

        // Fill remaining slots with the highest reverse-degree generated nodes.
        let mut by_degree: Vec<(usize, i64)> = Vec::new();
        by_degree.reserve(self.nodes_by_id.len());
        for (id, n) in &self.nodes_by_id {
            if !n.is_generated {
                continue;
            }
            let deg = self.reverse.get(id).map(|v| v.len()).unwrap_or(0);
            if deg == 0 {
                continue;
            }
            by_degree.push((deg, *id));
        }
        by_degree.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        for (_, id) in by_degree {
            if out_ids.len() >= limit {
                break;
            }
            if seen.insert(id) {
                out_ids.push(id);
            }
        }

        let mut out: Vec<&Node> = Vec::new();
        for id in out_ids {
            if let Some(n) = self.nodes_by_id.get(&id) {
                out.push(n);
            }
        }
        out
    }

    fn subgraph(&self, root_name: &str, depth: usize, dir: Dir) -> anyhow::Result<Subgraph> {
        let root_id = *self
            .id_by_name
            .get(root_name)
            .ok_or_else(|| anyhow::anyhow!("unknown node {:?}", root_name))?;

        let mut nodes: HashSet<i64> = HashSet::new();
        let mut links: Vec<Link> = Vec::new();

        nodes.insert(root_id);

        // Forward traversal: target -> deps
        if matches!(dir, Dir::Forward | Dir::Both) {
            let mut q: VecDeque<(i64, usize)> = VecDeque::new();
            q.push_back((root_id, 0));
            let mut seen: HashSet<i64> = HashSet::new();
            seen.insert(root_id);
            while let Some((id, d)) = q.pop_front() {
                if d >= depth {
                    continue;
                }
                let Some(deps) = self.forward.get(&id) else { continue; };
                for e in deps {
                    nodes.insert(e.other);
                    links.push(Link {
                        source: id,
                        target: e.other,
                        mode: e.mode,
                    });
                    if seen.insert(e.other) {
                        q.push_back((e.other, d + 1));
                    }
                }
            }
        }

        // Reverse traversal: source -> targets
        if matches!(dir, Dir::Reverse | Dir::Both) {
            let mut q: VecDeque<(i64, usize)> = VecDeque::new();
            q.push_back((root_id, 0));
            let mut seen: HashSet<i64> = HashSet::new();
            seen.insert(root_id);
            while let Some((id, d)) = q.pop_front() {
                if d >= depth {
                    continue;
                }
                let Some(deps) = self.reverse.get(&id) else { continue; };
                for e in deps {
                    nodes.insert(e.other);
                    links.push(Link {
                        source: e.other,
                        target: id,
                        mode: e.mode,
                    });
                    if seen.insert(e.other) {
                        q.push_back((e.other, d + 1));
                    }
                }
            }
        }

        links.sort_by(|a, b| (a.source, a.target).cmp(&(b.source, b.target)));
        links.dedup_by(|a, b| a.source == b.source && a.target == b.target);

        let mut node_views: Vec<NodeView> = Vec::new();
        for id in nodes {
            if let Some(n) = self.nodes_by_id.get(&id) {
                node_views.push(NodeView {
                    name: n.name.clone(),
                    generated: n.is_generated,
                });
            }
        }
        node_views.sort_by(|a, b| a.name.cmp(&b.name));

        let mut link_views: Vec<LinkView> = Vec::new();
        for l in links {
            let Some(src) = self.nodes_by_id.get(&l.source) else { continue; };
            let Some(dst) = self.nodes_by_id.get(&l.target) else { continue; };
            link_views.push(LinkView {
                source: src.name.clone(),
                target: dst.name.clone(),
                mode: l.mode,
            });
        }

        Ok(Subgraph {
            root: root_name.to_string(),
            depth,
            dir: dir.as_str().to_string(),
            nodes: node_views,
            links: link_views,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct Link {
    source: i64,
    target: i64,
    mode: char,
}

#[derive(Debug)]
struct Subgraph {
    root: String,
    depth: usize,
    dir: String,
    nodes: Vec<NodeView>,
    links: Vec<LinkView>,
}

#[derive(Debug)]
struct NodeView {
    name: String,
    generated: bool,
}

#[derive(Debug)]
struct LinkView {
    source: String,
    target: String,
    mode: char,
}

fn main() {
    if let Err(e) = real_main() {
        Log::err(&format!("{:?}", e));
        std::process::exit(1);
    }
}

fn real_main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut base_arg: Option<String> = None;
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 8000;
    let mut dump_json: Option<String> = None;
    let mut depth: usize = 2;
    let mut dir = Dir::Both;

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--base" {
            base_arg = args.get(i + 1).cloned();
            i += 2;
            continue;
        }
        if let Some(rest) = a.strip_prefix("--base=") {
            base_arg = Some(rest.to_string());
            i += 1;
            continue;
        }
        if a == "--host" {
            host = args.get(i + 1).cloned().unwrap_or_else(|| host.clone());
            i += 2;
            continue;
        }
        if let Some(rest) = a.strip_prefix("--host=") {
            host = rest.to_string();
            i += 1;
            continue;
        }
        if a == "--port" {
            port = args
                .get(i + 1)
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(port);
            i += 2;
            continue;
        }
        if let Some(rest) = a.strip_prefix("--port=") {
            port = rest.parse::<u16>().unwrap_or(port);
            i += 1;
            continue;
        }
        if a == "--depth" {
            depth = args
                .get(i + 1)
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(depth);
            i += 2;
            continue;
        }
        if let Some(rest) = a.strip_prefix("--depth=") {
            depth = rest.parse::<usize>().unwrap_or(depth);
            i += 1;
            continue;
        }
        if a == "--dir" {
            if let Some(v) = args.get(i + 1) {
                if let Some(d) = Dir::parse(v) {
                    dir = d;
                }
            }
            i += 2;
            continue;
        }
        if let Some(rest) = a.strip_prefix("--dir=") {
            if let Some(d) = Dir::parse(rest) {
                dir = d;
            }
            i += 1;
            continue;
        }
        if a == "--dump-json" {
            dump_json = args.get(i + 1).cloned();
            i += 2;
            continue;
        }
        if let Some(rest) = a.strip_prefix("--dump-json=") {
            dump_json = Some(rest.to_string());
            i += 1;
            continue;
        }
        if a == "--help" || a == "-h" {
            print_help();
            return Ok(());
        }
        // Positional: treat as dump-json root if not explicitly set.
        if dump_json.is_none() {
            dump_json = Some(a.clone());
            i += 1;
            continue;
        }
        i += 1;
    }

    let base = resolve_base(base_arg)?;
    let graph = Arc::new(Graph::load(base)?);

    if let Some(root) = dump_json {
        let sg = graph.subgraph(&root, depth, dir)?;
        print!("{}", sg.to_json());
        return Ok(());
    }

    let (listener, addr) = bind_with_fallback(&host, port)?;
    eprintln!(
        "redo-browse: serving build graph from {:?} at http://{}/",
        graph.base, addr
    );
    eprintln!("redo-browse: press Ctrl-C to stop.");

    loop {
        let (stream, _) = listener.accept()?;
        let g = graph.clone();
        std::thread::spawn(move || {
            let _ = handle_client(stream, &g);
        });
    }
}

fn print_help() {
    println!(
        "usage: redo-browse [options]\n\n\
options:\n  \
--base <dir>       base directory (default: search up for .redo)\n  \
--host <host>      bind host (default: 127.0.0.1)\n  \
--port <port>      bind port (default: 8000)\n  \
--dump-json <name> print a subgraph JSON and exit\n  \
--depth <n>        subgraph depth for --dump-json (default: 2)\n  \
--dir <both|forward|reverse>  direction for --dump-json (default: both)\n"
    );
}

fn resolve_base(base_arg: Option<String>) -> anyhow::Result<PathBuf> {
    if let Some(b) = base_arg {
        return Ok(PathBuf::from(b));
    }
    if let Ok(b) = std::env::var("REDO_BASE") {
        if !b.trim().is_empty() {
            return Ok(PathBuf::from(b));
        }
    }
    let cwd = std::env::current_dir()?;
    find_base_upwards(&cwd).ok_or_else(|| {
        anyhow::anyhow!(
            "redo-browse: could not find .redo/db.sqlite3 (run from a build directory, or pass --base)"
        )
    })
}

fn find_base_upwards(start: &Path) -> Option<PathBuf> {
    let mut cur = start.to_path_buf();
    loop {
        if cur.join(".redo").join("db.sqlite3").exists() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

fn bind_with_fallback(host: &str, port: u16) -> anyhow::Result<(TcpListener, SocketAddr)> {
    for p in port..=port.saturating_add(50) {
        let addr: SocketAddr = format!("{host}:{p}").parse()?;
        match TcpListener::bind(addr) {
            Ok(l) => return Ok((l, addr)),
            Err(_) => continue,
        }
    }
    anyhow::bail!("redo-browse: could not bind to {host}:{port}..{port}+50");
}

fn handle_client(mut stream: TcpStream, graph: &Graph) -> anyhow::Result<()> {
    let req = match read_request(&mut stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    if req.method != "GET" {
        write_response(&mut stream, "405 Method Not Allowed", "text/plain; charset=utf-8", b"")?;
        return Ok(());
    }

    let (path, query) = split_path_query(&req.uri);
    match path.as_str() {
        "/" => {
            write_response(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                index_html().as_bytes(),
            )?;
        }
        "/api/search" => {
            let q = query.get("q").map(|s| s.as_str()).unwrap_or("");
            let limit = query
                .get("limit")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(50);
            let matches = graph.search(q, limit);
            let mut body = String::new();
            body.push_str("{\"matches\":[");
            for (i, n) in matches.iter().enumerate() {
                if i != 0 {
                    body.push(',');
                }
                body.push_str("{\"name\":\"");
                body.push_str(&json_escape(&n.name));
                body.push_str("\",\"generated\":");
                body.push_str(if n.is_generated { "true" } else { "false" });
                body.push('}');
            }
            body.push_str("]}");
            write_response(
                &mut stream,
                "200 OK",
                "application/json; charset=utf-8",
                body.as_bytes(),
            )?;
        }
        "/api/suggest" => {
            let limit = query
                .get("limit")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(30);
            let matches = graph.suggest(limit);
            let mut body = String::new();
            body.push_str("{\"suggestions\":[");
            for (i, n) in matches.iter().enumerate() {
                if i != 0 {
                    body.push(',');
                }
                body.push_str("{\"name\":\"");
                body.push_str(&json_escape(&n.name));
                body.push_str("\",\"generated\":");
                body.push_str(if n.is_generated { "true" } else { "false" });
                body.push('}');
            }
            body.push_str("]}");
            write_response(
                &mut stream,
                "200 OK",
                "application/json; charset=utf-8",
                body.as_bytes(),
            )?;
        }
        "/api/subgraph" => {
            let name = match query.get("name") {
                Some(n) if !n.is_empty() => n.clone(),
                _ => {
                    write_response(
                        &mut stream,
                        "400 Bad Request",
                        "application/json; charset=utf-8",
                        b"{\"error\":\"missing name\"}",
                    )?;
                    return Ok(());
                }
            };
            let depth = query
                .get("depth")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(2);
            let dir = query
                .get("dir")
                .and_then(|v| Dir::parse(v))
                .unwrap_or(Dir::Both);
            match graph.subgraph(&name, depth, dir) {
                Ok(sg) => {
                    let body = sg.to_json();
                    write_response(
                        &mut stream,
                        "200 OK",
                        "application/json; charset=utf-8",
                        body.as_bytes(),
                    )?;
                }
                Err(e) => {
                    let body = format!("{{\"error\":\"{}\"}}", json_escape(&format!("{e}")));
                    write_response(
                        &mut stream,
                        "404 Not Found",
                        "application/json; charset=utf-8",
                        body.as_bytes(),
                    )?;
                }
            }
        }
        "/api/stats" => {
            let body = format!(
                "{{\"nodes\":{},\"deps\":{}}}",
                graph.nodes_by_id.len(),
                graph.forward.values().map(|v| v.len()).sum::<usize>()
            );
            write_response(
                &mut stream,
                "200 OK",
                "application/json; charset=utf-8",
                body.as_bytes(),
            )?;
        }
        _ => {
            write_response(&mut stream, "404 Not Found", "text/plain; charset=utf-8", b"")?;
        }
    }
    Ok(())
}

#[derive(Debug)]
struct Request {
    method: String,
    uri: String,
}

fn read_request(stream: &mut TcpStream) -> anyhow::Result<Option<Request>> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut buf = [0u8; 8192];
    let mut data: Vec<u8> = Vec::new();
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => return Ok(None),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => return Err(anyhow::anyhow!(e)),
        };
        data.extend_from_slice(&buf[..n]);
        if data.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if data.len() > 64 * 1024 {
            break;
        }
    }
    let s = String::from_utf8_lossy(&data);
    let line = match s.lines().next() {
        Some(l) => l,
        None => return Ok(None),
    };
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let uri = parts.next().unwrap_or("/").to_string();
    Ok(Some(Request { method, uri }))
}

fn split_path_query(uri: &str) -> (String, HashMap<String, String>) {
    let (path, query) = match uri.split_once('?') {
        Some((p, q)) => (p, q),
        None => (uri, ""),
    };
    (path.to_string(), parse_query(query))
}

fn parse_query(qs: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for part in qs.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k, v),
            None => (part, ""),
        };
        out.insert(url_decode(k), url_decode(v));
    }
    out
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(a), Some(b)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                    out.push((a << 4) | b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let hdr = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        content_type,
        body.len()
    );
    stream.write_all(hdr.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn json_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

impl Subgraph {
    fn to_json(&self) -> String {
        let mut s = String::new();
        s.push('{');
        s.push_str("\"root\":\"");
        s.push_str(&json_escape(&self.root));
        s.push_str("\",\"depth\":");
        s.push_str(&self.depth.to_string());
        s.push_str(",\"dir\":\"");
        s.push_str(&json_escape(&self.dir));
        s.push_str("\",\"nodes\":[");
        for (i, n) in self.nodes.iter().enumerate() {
            if i != 0 {
                s.push(',');
            }
            s.push_str("{\"name\":\"");
            s.push_str(&json_escape(&n.name));
            s.push_str("\",\"generated\":");
            s.push_str(if n.generated { "true" } else { "false" });
            s.push('}');
        }
        s.push_str("],\"links\":[");
        for (i, l) in self.links.iter().enumerate() {
            if i != 0 {
                s.push(',');
            }
            s.push_str("{\"source\":\"");
            s.push_str(&json_escape(&l.source));
            s.push_str("\",\"target\":\"");
            s.push_str(&json_escape(&l.target));
            s.push_str("\",\"mode\":\"");
            s.push(l.mode);
            s.push_str("\"}");
        }
        s.push_str("]}");
        s
    }
}

fn index_html() -> &'static str {
    // D3 is loaded from a CDN. The viewer still shows lists if D3 fails to load.
    r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>redo browse</title>
  <style>
    body { font-family: system-ui, -apple-system, sans-serif; margin: 0; }
    header { padding: 12px 16px; border-bottom: 1px solid #ddd; display: flex; gap: 12px; align-items: center; }
    header input { flex: 1; padding: 8px 10px; font-size: 14px; }
    header select, header input[type="number"] { padding: 6px 8px; font-size: 14px; }
    #main { display: grid; grid-template-columns: 360px 1fr; height: calc(100vh - 54px); }
    #sidebar { border-right: 1px solid #ddd; overflow: auto; padding: 12px 16px; }
    #graph { position: relative; overflow: hidden; }
    #matches button { display: block; width: 100%; text-align: left; padding: 6px 8px; margin: 4px 0; border: 1px solid #eee; background: #fafafa; cursor: pointer; }
    #matches button:hover { background: #f0f0f0; }
    .meta { color: #666; font-size: 12px; margin-top: 6px; }
    .mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; }
    svg { width: 100%; height: 100%; }
  </style>
  <script src="https://d3js.org/d3.v7.min.js"></script>
</head>
<body>
  <header>
    <input id="q" placeholder="Search targets/sources…" autocomplete="off" />
    <label class="meta">depth <input id="depth" type="number" min="1" max="8" value="2" style="width:64px"/></label>
    <select id="dir">
      <option value="both">both</option>
      <option value="forward">deps</option>
      <option value="reverse">rdeps</option>
    </select>
  </header>
  <div id="main">
    <div id="sidebar">
      <div class="meta">Selected</div>
      <div id="selected" class="mono">(none)</div>
      <div class="meta" id="stats"></div>
      <div class="meta" style="margin-top: 10px;">Matches</div>
      <div id="matches"></div>
    </div>
    <div id="graph"></div>
  </div>
<script>
const $ = (id) => document.getElementById(id);
let current = decodeURIComponent(location.hash.replace(/^#/, '')) || '';
let lastMatches = [];

async function api(path) {
  const r = await fetch(path);
  let data = {};
  try { data = await r.json(); } catch (e) { data = {}; }
  if (!r.ok) {
    const msg = (data && data.error) ? data.error : ('HTTP ' + r.status);
    throw new Error(msg);
  }
  return data;
}

function setSelected(name) {
  current = name || '';
  $('selected').textContent = current || '(none)';
  if (current) {
    location.hash = encodeURIComponent(current);
  } else {
    history.replaceState(null, '', location.pathname);
  }
}

function renderMatches(matches) {
  const div = $('matches');
  div.innerHTML = '';
  if (!matches || matches.length === 0) {
    const p = document.createElement('div');
    p.className = 'meta';
    p.textContent = 'No matches.';
    div.appendChild(p);
    lastMatches = [];
    return;
  }
  for (const m of matches) {
    const b = document.createElement('button');
    b.textContent = m.name + (m.generated ? '' : '  [src]');
    b.onclick = () => loadNode(m.name);
    div.appendChild(b);
  }
  lastMatches = matches;
}

async function loadSuggestions() {
  try {
    const data = await api('/api/suggest?limit=30');
    renderMatches(data.suggestions || []);
  } catch (e) {
    renderMatches([]);
  }
}

async function search() {
  const q = $('q').value.trim();
  if (!q) { await loadSuggestions(); return; }
  try {
    const data = await api('/api/search?q=' + encodeURIComponent(q) + '&limit=50');
    renderMatches(data.matches || []);
  } catch (e) {
    renderMatches([]);
  }
}

function clearGraph() {
  $('graph').innerHTML = '';
}

function showMessage(title, body) {
  clearGraph();
  const d = document.createElement('div');
  d.style.padding = '16px';
  d.innerHTML = `<div class="meta">${title}</div><div style="margin-top:8px">${body}</div>`;
  $('graph').appendChild(d);
}

function drawGraph(nodes, links, root) {
  clearGraph();
  if (!window.d3) {
    showMessage('Graph view unavailable', 'd3 could not be loaded (offline?). Try again with internet, or use <span class="mono">redo-browse --dump-json &lt;target&gt;</span>.');
    return;
  }
  const width = $('graph').clientWidth;
  const height = $('graph').clientHeight;
  const svg = d3.select('#graph').append('svg')
    .attr('width', width)
    .attr('height', height);

  // Background rectangle to capture pan/zoom gestures.
  svg.append('rect')
    .attr('width', width)
    .attr('height', height)
    .attr('fill', 'white')
    .attr('pointer-events', 'all');

  // Zoom/pan: wheel to zoom; drag background to pan.
  const g = svg.append('g');
  const zoom = d3.zoom()
    .scaleExtent([0.05, 8])
    .on('zoom', (event) => {
      g.attr('transform', event.transform);
    });
  svg.call(zoom);

  const nodeByName = new Map(nodes.map(n => [n.name, n]));
  const links2 = links.filter(l => nodeByName.has(l.source) && nodeByName.has(l.target));

  const sim = d3.forceSimulation(nodes)
    .force('link', d3.forceLink(links2).id(d => d.name).distance(80))
    .force('charge', d3.forceManyBody().strength(-300))
    .force('center', d3.forceCenter(width / 2, height / 2))
    .force('collide', d3.forceCollide(18));

  const gLinks = g.append('g').attr('stroke', '#999').attr('stroke-opacity', 0.6);
  const gNodes = g.append('g');

  const link = gLinks.selectAll('line')
    .data(links2)
    .enter().append('line')
    .attr('stroke-dasharray', d => d.mode === 'c' ? '3,3' : null);

  const node = gNodes.selectAll('g')
    .data(nodes)
    .enter().append('g')
    .call(d3.drag()
      .on('start', (event) => { if (event.sourceEvent) event.sourceEvent.stopPropagation(); if (!event.active) sim.alphaTarget(0.3).restart(); event.subject.fx = event.subject.x; event.subject.fy = event.subject.y; })
      .on('drag', (event) => { event.subject.fx = event.x; event.subject.fy = event.y; })
      .on('end', (event) => { if (!event.active) sim.alphaTarget(0); event.subject.fx = null; event.subject.fy = null; })
    );

  node.append('circle')
    .attr('r', 8)
    .attr('fill', d => d.name === root ? '#2563eb' : (d.generated ? '#16a34a' : '#6b7280'))
    .attr('stroke', '#111')
    .attr('stroke-width', 0.5)
    .on('click', (event, d) => loadNode(d.name));

  node.append('title').text(d => d.name);
  node.append('text')
    .attr('x', 10)
    .attr('y', 4)
    .attr('font-size', 10)
    .attr('font-family', 'ui-monospace, Menlo, monospace')
    .text(d => d.name.split('/').slice(-1)[0]);

  sim.on('tick', () => {
    link
      .attr('x1', d => d.source.x)
      .attr('y1', d => d.source.y)
      .attr('x2', d => d.target.x)
      .attr('y2', d => d.target.y);
    node.attr('transform', d => `translate(${d.x},${d.y})`);
  });
}

async function loadNode(name) {
  setSelected(name);
  if (!name) {
    showMessage('No node selected', 'Type in the search box, then click a match to render a subgraph.');
    return;
  }
  const depth = Math.max(1, Math.min(8, parseInt($('depth').value || '2', 10)));
  const dir = $('dir').value || 'both';
  try {
    const data = await api('/api/subgraph?name=' + encodeURIComponent(name) + '&depth=' + depth + '&dir=' + encodeURIComponent(dir));
    $('stats').textContent = `${data.nodes.length} nodes, ${data.links.length} edges`;
    drawGraph(data.nodes, data.links, data.root);
  } catch (e) {
    $('stats').textContent = `Error: ${e.message}`;
    showMessage('Could not load subgraph', e.message);
  }
}

$('q').addEventListener('input', () => { clearTimeout(window._t); window._t = setTimeout(search, 120); });
$('depth').addEventListener('change', () => { if (current) loadNode(current); });
$('dir').addEventListener('change', () => { if (current) loadNode(current); });

if (current) {
  $('q').value = current;
  loadNode(current);
} else {
  showMessage('No node selected', 'Type in the search box, then click a match to render a subgraph. Tip: try <span class="mono">cmake</span>, <span class="mono">all</span>, or a filename like <span class="mono">cmcmd.cxx</span>.');
  api('/api/stats').then(s => $('stats').textContent = `${s.nodes} nodes, ${s.deps} deps recorded`).catch(() => {});
  loadSuggestions();
  $('q').focus();
}
</script>
</body>
</html>
"#
}

