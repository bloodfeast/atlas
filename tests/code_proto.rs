//! gRPC across languages: a `.proto` rpc linked to its generated stubs, the
//! exact calls made through them and the server types implementing them
//! (`proto::link_rows`), in a root with a Go module indexed from a
//! synthetic prebuilt SCIP file and a Python folder the scanner reads. No
//! indexer runs.

mod common;

use std::fs;
use std::path::Path;

use atlas::code::{CodeSetup, Indexers};
use atlas::code_index::{self, IndexReport, RustSource};
use atlas::json;
use atlas::json::Value;
use atlas::protobuf::Writer;
use atlas::tools::{CodeState, Server};
use common::*;

const PROTO: &str = "syntax = \"proto3\";
package shop;

service CartService {
  rpc GetCart(GetCartRequest) returns (Cart);
}

message GetCartRequest {}
message Cart {}
";

const GEN_GO: &str = "package genproto

type CartServiceClient interface {
\tGetCart(ctx context.Context, in *GetCartRequest) (*Cart, error)
}

type CartServiceServer interface {
\tGetCart(context.Context, *GetCartRequest) (*Cart, error)
}

type UnimplementedCartServiceServer struct{}

func (UnimplementedCartServiceServer) GetCart(ctx context.Context, in *GetCartRequest) (*Cart, error) { return nil, nil }
";

const MAIN_GO: &str = "package main

import pb \"example.com/svc/genproto\"

type server struct {
\tpb.UnimplementedCartServiceServer
}

// GetCart answers from memory.
func (s *server) GetCart(ctx context.Context, in *pb.GetCartRequest) (*pb.Cart, error) {
\treturn nil, nil
}

func checkout(c pb.CartServiceClient) {
\tc.GetCart(nil, nil) // GetCart in a comment
}
";

const GEN_PY: &str = "class CartServiceStub(object):
    def __init__(self, channel):
        self.GetCart = channel.unary_unary('/shop.CartService/GetCart')


class CartServiceServicer(object):
    def GetCart(self, request, context):
        raise NotImplementedError()
";

const SERVER_PY: &str = "import demo_pb2_grpc


class Cart(demo_pb2_grpc.CartServiceServicer):
    def GetCart(self, request, context):
        return None
";

const CLIENT_PY: &str = "def run(stub):
    # GetCart is called below
    return stub.GetCart(None)
";

const G: &str = "scip-go gomod example.com/svc v0 ";

fn occ(w: &mut Writer, range: &[i32], sym: &str, roles: i64) {
    let mut o = Writer::default();
    o.packed(1, range).string(2, sym);
    if roles != 0 {
        o.int(3, roles);
    }
    w.message(2, &o);
}

/// A symbol's information with one implementation relationship.
fn implements(w: &mut Writer, sym: &str, base: &str) {
    let mut r = Writer::default();
    r.string(1, base).int(3, 1);
    let mut i = Writer::default();
    i.string(1, sym).message(4, &r);
    w.message(3, &i);
}

/// The SCIP file scip-go would write for the module (0-based lines).
fn go_index() -> Vec<u8> {
    let gen = |x: &str| format!("{G}`example.com/svc/genproto`/{x}");
    let main = |x: &str| format!("{G}`example.com/svc`/{x}");
    let mut g = Writer::default();
    g.string(1, "genproto/demo_grpc.pb.go").string(4, "go");
    occ(&mut g, &[2, 5, 22], &gen("CartServiceClient#"), 1);
    occ(&mut g, &[3, 1, 8], &gen("CartServiceClient#GetCart()."), 1);
    occ(&mut g, &[6, 5, 22], &gen("CartServiceServer#"), 1);
    occ(&mut g, &[7, 1, 8], &gen("CartServiceServer#GetCart()."), 1);
    occ(&mut g, &[10, 5, 35], &gen("UnimplementedCartServiceServer#"), 1);
    occ(&mut g, &[12, 38, 45], &gen("UnimplementedCartServiceServer#GetCart()."), 1);
    implements(&mut g, &gen("UnimplementedCartServiceServer#"), &gen("CartServiceServer#"));
    implements(&mut g, &gen("UnimplementedCartServiceServer#GetCart()."), &gen("CartServiceServer#GetCart()."));
    let mut m = Writer::default();
    m.string(1, "main.go").string(4, "go");
    occ(&mut m, &[4, 5, 11], &main("server#"), 1);
    occ(&mut m, &[5, 4, 34], &gen("UnimplementedCartServiceServer#"), 0);
    occ(&mut m, &[9, 17, 24], &main("server#GetCart()."), 1);
    occ(&mut m, &[13, 5, 13], &main("checkout()."), 1);
    occ(&mut m, &[13, 19, 36], &gen("CartServiceClient#"), 0);
    occ(&mut m, &[14, 3, 10], &gen("CartServiceClient#GetCart()."), 0);
    let mut tool = Writer::default();
    tool.string(1, "scip-go").string(2, "0.2.9");
    let mut meta = Writer::default();
    meta.message(2, &tool);
    let mut ix = Writer::default();
    ix.message(1, &meta).message(2, &g).message(2, &m);
    ix.buf
}

fn vaults(db: &Path) -> atlas::paths::Vaults {
    let d = db.parent().unwrap().join("notes");
    fs::create_dir_all(&d).unwrap();
    atlas::paths::Vaults::new(&[("notes".to_string(), d)]).unwrap()
}

fn indexed(t: &Temp) -> (IndexReport, Server) {
    let root = t.path().join("shop");
    write(&root.join("protos/cart.proto"), PROTO);
    write(&root.join("svc/go.mod"), "module example.com/svc\n\ngo 1.22\n");
    write(&root.join("svc/genproto/demo_grpc.pb.go"), GEN_GO);
    write(&root.join("svc/main.go"), MAIN_GO);
    fs::write(root.join("svc/index.scip"), go_index()).unwrap();
    write(&root.join("py/demo_pb2_grpc.py"), GEN_PY);
    write(&root.join("py/server.py"), SERVER_PY);
    write(&root.join("py/client.py"), CLIENT_PY);
    let db = t.path().join("state/atlas.sqlite");
    let spec = atlas::config::CodeSpec {
        roots: vec![atlas::config::CodeRootSpec { name: "shop".into(), root, build_scripts: false, scip: Default::default() }],
        debounce_s: Some(0),
        ..Default::default()
    };
    let mut code = CodeSetup::from_spec(&spec, &db, false);
    code.defer_on_cargo = false;
    code.indexers = Indexers::Off("off in tests".into());
    drop(atlas::store::Store::open(&db, vaults(&db)).unwrap());
    let mut conn = rusqlite::Connection::open(&db).unwrap();
    let rep = code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::ScanOnly("test".into())).unwrap();
    let mut s = Server::new(atlas::store::Store::open(&db, vaults(&db)).unwrap());
    s.set_code(CodeState { setup: code, indexer: None });
    (rep, s)
}

/// `path:line kind container` of a section's rows (`within` for refs).
fn rows(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            let what = if key == "refs" { &h["within"] } else { &h["container"] };
            format!("{}:{} {} {}", h["path"].as_str().unwrap(), h["line"], h["kind"].as_str().unwrap(), what.as_str().unwrap())
        })
        .collect()
}

#[test]
fn an_rpc_answers_its_stubs_exact_calls_and_implementations_in_each_language() {
    let t = Temp::new("code-proto");
    let (rep, mut s) = indexed(&t);
    assert!(rep.rust.contains("prebuilt scip-go"), "{}", rep.rust);
    let v = s.call("symbol", &json!({"name": "GetCart"})).unwrap();
    let defs = rows(&v, "defs");
    // The rpc first, then hand-written code, then generated code.
    assert!(defs[0].starts_with("protos/cart.proto:5 rpc (GetCartRequest) returns (Cart)"), "{defs:?}");
    let last_own = defs.iter().rposition(|d| !d.contains("demo_grpc.pb.go") && !d.contains("_pb2")).unwrap();
    let first_gen = defs.iter().position(|d| d.contains("demo_grpc.pb.go") || d.contains("_pb2")).unwrap();
    assert!(last_own < first_gen, "{defs:?}");
    // The call through the Go client stub is exact and has its caller; the
    // words in comments are no call sites, and the Python client (no
    // index) keeps its word match.
    assert_eq!(rows(&v, "refs"), ["svc/main.go:15 rpc svc::checkout", "py/client.py:3 call (mention) "], "{v}");
    let impls = rows(&v, "impls");
    for want in [
        "svc/main.go:10 rpc server CartService for server::GetCart",
        "py/server.py:5 rpc server CartService for Cart::GetCart",
        "svc/genproto/demo_grpc.pb.go:4 rpc stub client stub CartServiceClient::GetCart",
        "svc/genproto/demo_grpc.pb.go:8 rpc stub server stub CartServiceServer::GetCart",
        "py/demo_pb2_grpc.py:7 rpc stub server stub CartServiceServicer::GetCart",
    ] {
        assert!(impls.iter().any(|i| i == want), "missing {want}: {impls:?}");
    }
    // One stub per file and side: the Unimplemented server's method is the
    // server side's second; the generated impl edge under the rpc's name
    // is the stubs' own plumbing.
    assert!(!impls.iter().any(|i| i.contains("Unimplemented")), "{impls:?}");
    // Implementations before stubs.
    let last_server = impls.iter().rposition(|i| i.contains("rpc server")).unwrap();
    assert!(impls.iter().position(|i| i.contains("rpc stub")).unwrap() > last_server, "{impls:?}");
    assert_eq!((rep.rows.rpc_calls, rep.rows.rpc_servers), (1, 2), "{:?}", rep.rows);
    let text = s.answer("symbol", &json!({"name": "GetCart"})).unwrap().into_text();
    assert!(text.contains("shop:svc/main.go:15\tsvc::checkout (rpc)\t"), "{text}");
}
