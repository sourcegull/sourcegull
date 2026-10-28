use super::*;

/// The symbols of `src` with the query run over the whole tree, as
/// `find_defs` does with pruning.
fn unpruned(lang: &str, src: &str) -> Vec<Symbol> {
    let grammar: &Grammar = &GRAMMARS[lang];
    let mut parser = Parser::new();
    parser.set_language(&grammar.language).unwrap();
    let tree = parser.parse(src, None).unwrap();
    let mut defs = Defs::default();
    add_matches(
        grammar,
        &mut QueryCursor::new(),
        tree.root_node(),
        src.as_bytes(),
        &mut defs,
    );
    nest(defs, src.as_bytes())
}

/// (name, kind, container) per symbol.
fn syms(lang: &str, src: &str) -> Vec<(String, &'static str, Option<String>)> {
    let out = extract(lang, src.as_bytes());
    let (full, _) = extract_traced(lang, src.as_bytes(), false);
    assert_eq!(out, full, "{lang}: skipping bodies changed the symbols");
    assert_eq!(
        full,
        unpruned(lang, src),
        "{lang}: pruning changed the symbols"
    );
    for s in &out {
        assert_eq!(
            &src[s.range.start as usize..s.range.end as usize],
            s.name,
            "{lang}: name range"
        );
    }
    out.into_iter()
        .map(|s| (s.name, s.kind.name(), s.container))
        .collect()
}

fn row(
    name: &str,
    kind: &'static str,
    container: Option<&str>,
) -> (String, &'static str, Option<String>) {
    (name.to_owned(), kind, container.map(str::to_owned))
}

#[test]
fn every_grammar_compiles() {
    for (id, grammar) in GRAMMARS.iter() {
        LazyLock::force(grammar);
        assert!(
            sourcegull_languages::by_id(id).is_some(),
            "{id} is a language id"
        );
        assert!(supported(id));
    }
    assert!(!supported("markdown"));
    assert!(extract("markdown", b"# x").is_empty());
}

#[test]
fn rust() {
    let src = r#"
mod net {
    pub struct Conn { addr: String }
    pub enum State { Open, Closed(u8) }
    pub trait Dial { fn dial(&self); }
    impl Dial for Conn { fn dial(&self) {} }
    impl<T> Wrapper<T> { fn get(&self) {} }
    const MAX: usize = 3;
    static NAME: &str = "x";
    type Id = u64;
    macro_rules! m { () => {} }
}
fn main() { fn inner() {} }
"#;
    assert_eq!(
        syms("rust", src),
        [
            row("net", "MODULE", None),
            row("Conn", "STRUCT", Some("net")),
            row("addr", "FIELD", Some("Conn")),
            row("State", "ENUM", Some("net")),
            row("Open", "ENUMMEMBER", Some("State")),
            row("Closed", "ENUMMEMBER", Some("State")),
            row("Dial", "INTERFACE", Some("net")),
            row("dial", "METHOD", Some("Dial")),
            row("dial", "METHOD", Some("Conn")),
            row("get", "METHOD", Some("Wrapper")),
            row("MAX", "CONSTANT", Some("net")),
            row("NAME", "VARIABLE", Some("net")),
            row("Id", "TYPEPARAMETER", Some("net")),
            row("m", "FUNCTION", Some("net")),
            row("main", "FUNCTION", None),
        ]
    );
}

#[test]
fn go() {
    let src = r#"package srv

const Max = 3

const (
	A = iota
	B
)

var (
	Default = New()
)

type Server struct {
	Addr string
}

type Handler interface {
	Serve(w int)
}

type ID int

func New() *Server { var local = 1; return nil }

func (s *Server) Run() error { return nil }

func (m Map[K]) Len() int { return 0 }
"#;
    assert_eq!(
        syms("go", src),
        [
            row("srv", "PACKAGE", None),
            row("Max", "CONSTANT", None),
            row("A", "CONSTANT", None),
            row("B", "CONSTANT", None),
            row("Default", "VARIABLE", None),
            row("Server", "STRUCT", None),
            row("Addr", "FIELD", Some("Server")),
            row("Handler", "INTERFACE", None),
            row("Serve", "METHOD", Some("Handler")),
            row("ID", "TYPEPARAMETER", None),
            row("New", "FUNCTION", None),
            row("Run", "METHOD", Some("Server")),
            row("Len", "METHOD", Some("Map")),
        ]
    );
}

#[test]
fn python() {
    let src = r#"
LIMIT = 10

class Cache:
    size = 3
    def get(self, key):
        def helper():
            pass

def main():
    local = 1
"#;
    assert_eq!(
        syms("python", src),
        [
            row("LIMIT", "VARIABLE", None),
            row("Cache", "CLASS", None),
            row("get", "METHOD", Some("Cache")),
            row("main", "FUNCTION", None),
        ]
    );
}

#[test]
fn java() {
    let src = r#"
package com.example.app;

public class App {
    private int count;
    public App() {}
    public void run() {}
    enum Mode { FAST, SLOW }
    interface Hook { void call(); }
}
"#;
    assert_eq!(
        syms("java", src),
        [
            row("com.example.app", "PACKAGE", None),
            row("App", "CLASS", None),
            row("count", "FIELD", Some("App")),
            row("App", "CONSTRUCTOR", Some("App")),
            row("run", "METHOD", Some("App")),
            row("Mode", "ENUM", Some("App")),
            row("FAST", "ENUMMEMBER", Some("Mode")),
            row("SLOW", "ENUMMEMBER", Some("Mode")),
            row("Hook", "INTERFACE", Some("App")),
            row("call", "METHOD", Some("Hook")),
        ]
    );
}

#[test]
fn javascript_and_typescript() {
    let js = r#"
const LIMIT = 3;
export const handler = (req) => req;
function main() {}
class Store {
  items = [];
  add(x) {}
}
"#;
    assert_eq!(
        syms("javascript", js),
        [
            row("LIMIT", "VARIABLE", None),
            row("handler", "FUNCTION", None),
            row("main", "FUNCTION", None),
            row("Store", "CLASS", None),
            row("items", "FIELD", Some("Store")),
            row("add", "METHOD", Some("Store")),
        ]
    );
    let ts = r#"
interface Shape { area(): number; name: string }
type Id = string;
enum Color { Red, Green = 2 }
namespace Geo { export function dist(): number { return 0 } }
abstract class Base { abstract size(): number; private n = 1; }
"#;
    let want = [
        row("Shape", "INTERFACE", None),
        row("area", "METHOD", Some("Shape")),
        row("name", "PROPERTY", Some("Shape")),
        row("Id", "TYPEPARAMETER", None),
        row("Color", "ENUM", None),
        row("Red", "ENUMMEMBER", Some("Color")),
        row("Green", "ENUMMEMBER", Some("Color")),
        row("Geo", "NAMESPACE", None),
        row("dist", "FUNCTION", Some("Geo")),
        row("Base", "CLASS", None),
        row("size", "METHOD", Some("Base")),
        row("n", "FIELD", Some("Base")),
    ];
    assert_eq!(syms("typescript", ts), want);
    assert_eq!(syms("tsx", ts), want);
}

#[test]
fn c_and_cpp() {
    let c = r#"
#define MAX 10
#define SQ(x) ((x)*(x))
typedef unsigned long id_t;
struct point { int x; int *y; };
enum color { RED, GREEN };
int counter = 0;
static char *name(void) { return 0; }
int main(void) { int local = 1; return 0; }
"#;
    assert_eq!(
        syms("c", c),
        [
            row("MAX", "CONSTANT", None),
            row("SQ", "FUNCTION", None),
            row("id_t", "TYPEPARAMETER", None),
            row("point", "STRUCT", None),
            row("x", "FIELD", Some("point")),
            row("y", "FIELD", Some("point")),
            row("color", "ENUM", None),
            row("RED", "ENUMMEMBER", Some("color")),
            row("GREEN", "ENUMMEMBER", Some("color")),
            row("counter", "VARIABLE", None),
            row("name", "FUNCTION", None),
            row("main", "FUNCTION", None),
        ]
    );
    let cpp = r#"
namespace net {
class Conn {
  public:
    void open();
    int fd;
    int size() { return 0; }
};
void Conn::open() {}
using Port = int;
}
"#;
    assert_eq!(
        syms("cpp", cpp),
        [
            row("net", "NAMESPACE", None),
            row("Conn", "CLASS", Some("net")),
            row("open", "METHOD", Some("Conn")),
            row("fd", "FIELD", Some("Conn")),
            row("size", "METHOD", Some("Conn")),
            row("open", "METHOD", Some("Conn")),
            row("Port", "TYPEPARAMETER", Some("net")),
        ]
    );
}

#[test]
fn c_macros_the_grammar_does_not_know() {
    // Linux style: attribute macros, a macro call that declares, a function
    // definition whose parameters hold a struct. A full parse recovers from
    // the macros with made-up names; leaving them out parses cleanly.
    let src = r#"
static int __init foo_init(struct device *dev, int n) { return 0; }
static DEFINE_PER_CPU(struct cpu_data *, cpu_data);
struct s { int a; } __packed;
int __read_mostly flag;
void __printf(1, 2) log(const char *fmt, ...);
module_init(foo_init);
"#;
    let got: Vec<_> = extract("c", src.as_bytes())
        .into_iter()
        .map(|s| (s.name, s.kind.name(), s.container))
        .collect();
    assert_eq!(
        got,
        [
            row("foo_init", "FUNCTION", None),
            row("s", "STRUCT", None),
            row("a", "FIELD", Some("s")),
            row("flag", "VARIABLE", None),
        ]
    );
    let (full, _) = extract_traced("c", src.as_bytes(), false);
    assert!(
        full.iter()
            .any(|s| s.name == "__packed" || s.name == "__read_mostly")
    );
}

#[test]
fn shell() {
    let src = "#!/bin/sh\nsetup() { :; }\nfunction run { setup; }\n";
    assert_eq!(
        syms("shell", src),
        [row("setup", "FUNCTION", None), row("run", "FUNCTION", None)]
    );
}

#[test]
fn bad_input_is_not_an_error() {
    assert!(
        extract("rust", b"fn (((")
            .iter()
            .all(|s| !s.name.is_empty())
    );
    assert!(extract("go", &[0xff, 0xfe, 0x00]).is_empty());
    let big = vec![b'a'; MAX_BYTES + 1];
    assert!(extract("rust", &big).is_empty());
}

#[test]
fn function_bodies_are_local() {
    // Skipped bodies and full parses give the same symbols.
    let cases = [
        (
            "go",
            "package p\nvar F = func() { type Inner struct{} }\nfunc f() struct{ x int } { type L int; return struct{ x int }{} }\n",
        ),
        (
            "javascript",
            "(function () { function hidden() {} })();\nif (x) { var y = 1; }\nconst o = { m() { function no() {} } };\n",
        ),
        (
            "rust",
            "fn main() { struct Local; impl Local { fn m() {} } }\nstruct S;\n",
        ),
        (
            "c",
            "#if A\nint f(int a) {\n#else\nint f(void) {\n#endif\n  struct x { int y; };\n  return 0;\n}\n",
        ),
    ];
    for (lang, src) in cases {
        let (skipped, how) = extract_traced(lang, src.as_bytes(), true);
        let (full, _) = extract_traced(lang, src.as_bytes(), false);
        assert_eq!(skipped, full, "{lang}: {how:?}");
    }
    let go = syms("go", cases[0].1);
    assert_eq!(
        go,
        [
            row("p", "PACKAGE", None),
            row("F", "VARIABLE", None),
            row("f", "FUNCTION", None),
        ]
    );
    assert_eq!(
        syms("javascript", cases[1].1),
        [row("o", "VARIABLE", None), row("m", "METHOD", Some("o"))]
    );
    assert_eq!(
        extract_traced("c", cases[3].1.as_bytes(), true).1,
        Parsed::LexerGaveUp
    );
    assert_eq!(
        extract_traced("rust", cases[2].1.as_bytes(), true).1,
        Parsed::BodiesSkipped
    );
}

#[test]
fn csharp() {
    let src = r#"using System;
namespace App.Core {
    public interface IShape { double Area(); string Name { get; } }
    public enum Color { Red, Green = 2 }
    public struct Point { public int X; public int Y; }
    public record Person(string First, string Last);
    public delegate void Handler(int x);
    public class Store<T> : IShape {
        private int count = 0;
        public const int Max = 10;
        public string Name { get; set; }
        public event Handler Changed;
        public Store() { }
        public double Area() { int local = 1; return 0; }
        ~Store() { }
        public static Store<T> operator +(Store<T> a, Store<T> b) => a;
        public T this[int i] => default;
    }
    assert_eq!(extract_traced("csharp", src.as_bytes(), true).1, Parsed::BodiesSkipped);
}
namespace Flat;
class B { void M() {} }
"#;
    assert_eq!(
        syms("csharp", src),
        [
            row("App.Core", "NAMESPACE", None),
            row("IShape", "INTERFACE", Some("App.Core")),
            row("Area", "METHOD", Some("IShape")),
            row("Name", "PROPERTY", Some("IShape")),
            row("Color", "ENUM", Some("App.Core")),
            row("Red", "ENUMMEMBER", Some("Color")),
            row("Green", "ENUMMEMBER", Some("Color")),
            row("Point", "STRUCT", Some("App.Core")),
            row("X", "FIELD", Some("Point")),
            row("Y", "FIELD", Some("Point")),
            row("Person", "CLASS", Some("App.Core")),
            row("Handler", "TYPEPARAMETER", Some("App.Core")),
            row("Store", "CLASS", Some("App.Core")),
            row("count", "FIELD", Some("Store")),
            row("Max", "CONSTANT", Some("Store")),
            row("Name", "PROPERTY", Some("Store")),
            row("Changed", "EVENT", Some("Store")),
            row("Store", "CONSTRUCTOR", Some("Store")),
            row("Area", "METHOD", Some("Store")),
            row("Flat", "NAMESPACE", None),
            row("B", "CLASS", None),
            row("M", "METHOD", Some("B")),
        ]
    );
}

#[test]
fn kotlin() {
    let src = r#"package com.example.app

import kotlin.math.max

const val LIMIT = 3
val top = 1
var mutableTop = "x"
typealias Id = String

interface Shape { fun area(): Double; val name: String }
enum class Color { RED, GREEN }
data class Point(val x: Int, val y: Int)
object Registry {
    fun register() {}
}
class Store(private val size: Int) : Shape {
    val items = mutableListOf<Int>()
    constructor(a: String) : this(1)
    override fun area(): Double { val local = 1; return 0.0 }
    override val name: String get() = "s"
    companion object { fun create() = Store(1) }
    inner class Inner
}
fun main() { fun nested() {} }
fun String.ext(): Int = length
"#;
    assert_eq!(
        syms("kotlin", src),
        [
            row("com.example.app", "PACKAGE", None),
            row("LIMIT", "CONSTANT", None),
            row("top", "VARIABLE", None),
            row("mutableTop", "VARIABLE", None),
            row("Id", "TYPEPARAMETER", None),
            row("Shape", "INTERFACE", None),
            row("area", "METHOD", Some("Shape")),
            row("name", "PROPERTY", Some("Shape")),
            row("Color", "ENUM", None),
            row("RED", "ENUMMEMBER", Some("Color")),
            row("GREEN", "ENUMMEMBER", Some("Color")),
            row("Point", "CLASS", None),
            row("x", "PROPERTY", Some("Point")),
            row("y", "PROPERTY", Some("Point")),
            row("Registry", "CLASS", None),
            row("register", "METHOD", Some("Registry")),
            row("Store", "CLASS", None),
            row("size", "PROPERTY", Some("Store")),
            row("items", "PROPERTY", Some("Store")),
            row("constructor", "CONSTRUCTOR", Some("Store")),
            row("area", "METHOD", Some("Store")),
            row("name", "PROPERTY", Some("Store")),
            row("create", "METHOD", Some("Store")),
            row("Inner", "CLASS", Some("Store")),
            row("main", "FUNCTION", None),
            row("ext", "FUNCTION", None),
        ]
    );
}

#[test]
fn ruby() {
    let src = r#"module Net
  VERSION = "1.0"
  class Conn < Base
    attr_reader :addr
    MAX = 3
    def initialize(addr)
      @addr = addr
    end
    def self.open(x) = new(x)
    def call; end
    class << self
      def build; end
    end
  end
end
def main
  def inner; end
end
Point = Struct.new(:x, :y)
"#;
    assert_eq!(
        syms("ruby", src),
        [
            row("Net", "MODULE", None),
            row("VERSION", "CONSTANT", Some("Net")),
            row("Conn", "CLASS", Some("Net")),
            row("MAX", "CONSTANT", Some("Conn")),
            row("initialize", "METHOD", Some("Conn")),
            row("open", "METHOD", Some("Conn")),
            row("call", "METHOD", Some("Conn")),
            row("build", "METHOD", Some("Conn")),
            row("main", "FUNCTION", None),
            row("Point", "CONSTANT", None),
        ]
    );
}

#[test]
fn scala() {
    let src = r#"package com.example

object Main extends App {
  val limit = 3
  var count = 0
  def run(x: Int): Int = { val local = 1; x }
  type Id = String
}
trait Shape { def area: Double; val name: String }
class Store(size: Int) extends Shape {
  def area: Double = 0
  val name = "s"
  class Inner
}
case class Point(x: Int, y: Int)
sealed abstract class Expr
enum Color { case Red, Green }
given intOrd: Ordering[Int] = ???
def topLevel(): Unit = ()
"#;
    assert_eq!(
        syms("scala", src),
        [
            row("com.example", "PACKAGE", None),
            row("Main", "CLASS", None),
            row("limit", "VARIABLE", Some("Main")),
            row("count", "VARIABLE", Some("Main")),
            row("run", "METHOD", Some("Main")),
            row("Id", "TYPEPARAMETER", Some("Main")),
            row("Shape", "INTERFACE", None),
            row("area", "METHOD", Some("Shape")),
            row("name", "VARIABLE", Some("Shape")),
            row("Store", "CLASS", None),
            row("area", "METHOD", Some("Store")),
            row("name", "VARIABLE", Some("Store")),
            row("Inner", "CLASS", Some("Store")),
            row("Point", "CLASS", None),
            row("Expr", "CLASS", None),
            row("Color", "ENUM", None),
            row("Red", "ENUMMEMBER", Some("Color")),
            row("Green", "ENUMMEMBER", Some("Color")),
            row("intOrd", "VARIABLE", None),
            row("topLevel", "FUNCTION", None),
        ]
    );
}

#[test]
fn php() {
    let src = r#"<?php
namespace App\Models;

const LIMIT = 3;
function helper($x) { function inner() {} return $x; }
interface Shape { public function area(): float; }
trait Logs { public function log() {} }
enum Suit: string { case Hearts = 'H'; case Spades = 'S'; }
abstract class Base { }
final class Store extends Base implements Shape {
    const MAX = 10;
    private int $count = 0;
    public static $shared;
    public function __construct() {}
    public function area(): float { $local = 1; return 0.0; }
}
$fn = function() { };
?>
<html><?php echo 1; ?></html>
"#;
    assert_eq!(
        syms("php", src),
        [
            row("App\\Models", "NAMESPACE", None),
            row("LIMIT", "CONSTANT", None),
            row("helper", "FUNCTION", None),
            row("Shape", "INTERFACE", None),
            row("area", "METHOD", Some("Shape")),
            row("Logs", "INTERFACE", None),
            row("log", "METHOD", Some("Logs")),
            row("Suit", "ENUM", None),
            row("Hearts", "ENUMMEMBER", Some("Suit")),
            row("Spades", "ENUMMEMBER", Some("Suit")),
            row("Base", "CLASS", None),
            row("Store", "CLASS", None),
            row("MAX", "CONSTANT", Some("Store")),
            row("count", "PROPERTY", Some("Store")),
            row("shared", "PROPERTY", Some("Store")),
            row("__construct", "METHOD", Some("Store")),
            row("area", "METHOD", Some("Store")),
        ]
    );
}

#[test]
fn swift() {
    let src = r#"import Foundation

let limit = 3
var counter = 0
typealias Id = String

protocol Shape { func area() -> Double; var name: String { get } }
enum Color { case red, green; case blue(Int) }
struct Point { var x: Int; let y: Int }
class Store: Shape {
    var items: [Int] = []
    init(size: Int) { }
    deinit { }
    func area() -> Double { let local = 1; return 0 }
    var name: String { "s" }
    subscript(i: Int) -> Int { 0 }
    static func make() -> Store { Store(size: 1) }
}
extension Store { func extra() {} }
actor Counter { func inc() {} }
func main() { func nested() {} }
"#;
    assert_eq!(
        syms("swift", src),
        [
            row("limit", "VARIABLE", None),
            row("counter", "VARIABLE", None),
            row("Id", "TYPEPARAMETER", None),
            row("Shape", "INTERFACE", None),
            row("area", "METHOD", Some("Shape")),
            row("name", "PROPERTY", Some("Shape")),
            row("Color", "ENUM", None),
            row("red", "ENUMMEMBER", Some("Color")),
            row("green", "ENUMMEMBER", Some("Color")),
            row("blue", "ENUMMEMBER", Some("Color")),
            row("Point", "STRUCT", None),
            row("x", "PROPERTY", Some("Point")),
            row("y", "PROPERTY", Some("Point")),
            row("Store", "CLASS", None),
            row("items", "PROPERTY", Some("Store")),
            row("init", "CONSTRUCTOR", Some("Store")),
            row("area", "METHOD", Some("Store")),
            row("name", "PROPERTY", Some("Store")),
            row("make", "METHOD", Some("Store")),
            row("extra", "METHOD", Some("Store")),
            row("Counter", "CLASS", None),
            row("inc", "METHOD", Some("Counter")),
            row("main", "FUNCTION", None),
        ]
    );
}

#[test]
fn lua() {
    let src = r#"local M = {}
local limit = 3
Global = 1
function M.add(a, b) return a + b end
function M:method() end
local function helper() local function inner() end end
function top() end
M.field = function() end
local anon = function() end
return M
"#;
    assert_eq!(
        syms("lua", src),
        [
            row("M", "VARIABLE", None),
            row("limit", "VARIABLE", None),
            row("Global", "VARIABLE", None),
            row("add", "FUNCTION", Some("M")),
            row("method", "METHOD", Some("M")),
            row("helper", "FUNCTION", None),
            row("top", "FUNCTION", None),
            row("field", "FUNCTION", Some("M")),
            row("anon", "FUNCTION", None),
        ]
    );
}

#[test]
fn zig() {
    let src = r#"const std = @import("std");
pub const Limit = 3;
var counter: u32 = 0;
const Point = struct {
    x: i32,
    y: i32,
    pub fn len(self: Point) i32 { const local = 1; return local; }
};
const Color = enum { red, green };
const Value = union(enum) { int: i64, float: f64 };
pub fn main() !void { }
fn helper() void {}
test "basic" { }
"#;
    assert_eq!(
        syms("zig", src),
        [
            row("std", "CONSTANT", None),
            row("Limit", "CONSTANT", None),
            row("counter", "VARIABLE", None),
            row("Point", "STRUCT", None),
            row("x", "FIELD", Some("Point")),
            row("y", "FIELD", Some("Point")),
            row("len", "METHOD", Some("Point")),
            row("Color", "ENUM", None),
            row("red", "ENUMMEMBER", Some("Color")),
            row("green", "ENUMMEMBER", Some("Color")),
            row("Value", "STRUCT", None),
            row("int", "FIELD", Some("Value")),
            row("float", "FIELD", Some("Value")),
            row("main", "FUNCTION", None),
            row("helper", "FUNCTION", None),
        ]
    );
}

#[test]
fn elixir() {
    let src = r#"defmodule App.Store do
  @moduledoc "x"
  defstruct [:items]
  @limit 3
  def add(store, item), do: store
  def run(x) do
    y = 1
    x
  end
  defp helper, do: :ok
  defmacro m(x), do: x
  defguard is_even(x) when rem(x, 2) == 0
  defprotocol Shape do
    def area(s)
  end
  defimpl String.Chars, for: App.Store do
    def to_string(_), do: ""
  end
  defmodule Inner do
  end
end
"#;
    assert_eq!(
        syms("elixir", src),
        [
            row("App.Store", "MODULE", None),
            row("add", "FUNCTION", Some("App.Store")),
            row("run", "FUNCTION", Some("App.Store")),
            row("helper", "FUNCTION", Some("App.Store")),
            row("m", "FUNCTION", Some("App.Store")),
            row("is_even", "FUNCTION", Some("App.Store")),
            row("Shape", "INTERFACE", Some("App.Store")),
            row("area", "METHOD", Some("Shape")),
            row("to_string", "METHOD", Some("String.Chars")),
            row("Inner", "MODULE", Some("App.Store")),
        ]
    );
}
