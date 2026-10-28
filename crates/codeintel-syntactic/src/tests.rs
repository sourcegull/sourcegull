use super::*;

/// Each symbol as `name: line[d] line ...` (0-based lines, `d` marks definitions), in symbol
/// order.
fn symbols(lang: &str, src: &str) -> Vec<String> {
    let locals = analyze(lang, src.as_bytes()).expect("supported");
    let lines: Vec<&str> = src.split('\n').collect();
    let mut by: BTreeMap<u32, (String, Vec<String>)> = BTreeMap::new();
    for o in &locals.occurrences {
        let line: Vec<u16> = lines[o.line as usize].encode_utf16().collect();
        let name = String::from_utf16_lossy(&line[o.start as usize..o.end as usize]);
        let e = by.entry(o.symbol).or_insert_with(|| (name, Vec::new()));
        e.1.push(format!("{}{}", o.line, if o.definition { "d" } else { "" }));
    }
    by.into_values()
        .map(|(n, l)| format!("{n}: {}", l.join(" ")))
        .collect()
}

#[test]
fn every_query_compiles() {
    for lang in languages() {
        assert!(analyze(lang, b"").is_some(), "{lang}");
    }
    assert_eq!(languages().len(), 9);
    assert!(!supported("shell"));
}

#[test]
fn go() {
    let src = "package p

var global = 1

func f(a int, rest ...string) (n int) {
	x := a + global
	for i, v := range rest {
		x += i
		_ = v
	}
	if y := x; y > 0 {
		return y
	}
	g := func(x int) int { return x }
	n = g(x)
	return
}
";
    assert_eq!(
        symbols("go", src),
        [
            "a: 4d 5",
            "rest: 4d 6",
            "n: 4d 14",
            "x: 5d 7 10 14",
            "i: 6d 7",
            "v: 6d 8",
            "y: 10d 10 11",
            "g: 13d 14",
            "x: 13d 13",
        ]
    );
}

#[test]
fn rust() {
    let src = "fn f(s: &str, items: Vec<i32>) -> usize {
    let s = s.trim();
    let (a, mut b) = (1, 2);
    b += a;
    for item in &items {
        if let Some(x) = Some(*item) {
            b += x as usize;
        }
    }
    match s.len() {
        0 => b,
        n => n + helper(n),
    }
}

fn helper(n: usize) -> usize {
    let add = |x: usize, y| x + y;
    fn inner(v: usize) -> usize { v }
    add(inner(n), 1)
}
";
    assert_eq!(
        symbols("rust", src),
        [
            "s: 0d 1",
            "items: 0d 4",
            "s: 1d 9",
            "a: 2d 3",
            "b: 2d 3 6 10",
            "item: 4d 5",
            "x: 5d 6",
            "n: 11d 11 11",
            "n: 15d 18",
            "add: 16d 18",
            "x: 16d 16",
            "y: 16d 16",
            "inner: 17d 18",
            "v: 17d 17",
        ]
    );
}

#[test]
fn python() {
    let src = "import os

def f(a, b=1, *args, c: int = 2, **kw):
    x = a
    x = x + b
    for i, j in kw.items():
        print(i, j, os.sep, args, c)
    def inner(y):
        return y + x
    total = sum(v for v in [x])
    obj.x = 1
    g(x=total)
    with open(a) as fh:
        fh.read()
    return inner
";
    assert_eq!(
        symbols("python", src),
        [
            "a: 2d 3 12",
            "b: 2d 4",
            "args: 2d 6",
            "c: 2d 6",
            "kw: 2d 5",
            "x: 3d 4d 4 8 9",
            "i: 5d 6",
            "j: 5d 6",
            "inner: 7d 14",
            "y: 7d 8",
            "total: 9d 11",
            "v: 9 9d",
            "fh: 12d 13",
        ]
    );
}

#[test]
fn java() {
    let src = "class A {
    int field;
    int f(int a, String... rest) {
        int x = a + field;
        for (String s : rest) { x += s.length(); }
        try (var r = open()) { r.close(); } catch (Exception e) { log(e); }
        Runnable run = () -> use(x);
        list.forEach(item -> item.go());
        return this.x;
    }
}
";
    assert_eq!(
        symbols("java", src),
        [
            "a: 2d 3",
            "rest: 2d 4",
            "x: 3d 4 6",
            "s: 4d 4",
            "r: 5d 5",
            "e: 5d 5",
            "run: 6d",
            "item: 7d 7",
        ]
    );
}

#[test]
fn javascript_and_typescript() {
    let js = "const top = 1;
function f(a, { b, c: d }, [e], g = 2, ...rest) {
  let x = a + b + d + e + g + top;
  const h = (y) => y + x;
  function inner(z) { return z; }
  for (const k in rest) { inner(k); }
  try { h(1); } catch (err) { console.log(err, { x }); }
  return o.x;
}
";
    assert_eq!(
        symbols("javascript", js),
        [
            "a: 1d 2",
            "b: 1d 2",
            "d: 1d 2",
            "e: 1d 2",
            "g: 1d 2",
            "rest: 1d 5",
            "x: 2d 3 6",
            "h: 3d 6",
            "y: 3d 3",
            "inner: 4d 5",
            "z: 4d 4",
            "k: 5d 5",
            "err: 6d 6",
        ]
    );
    let ts = "function f(a: number, b?: string): number {
  const c = a + (b?.length ?? 0);
  return c;
}
";
    let want = ["a: 0d 1", "b: 0d 1", "c: 1d 2"];
    assert_eq!(symbols("typescript", ts), want);
    assert_eq!(symbols("tsx", ts), want);
}

#[test]
fn c_and_cpp() {
    let src = "int g;
int f(int a, char *p, int arr[]) {
    int x = a + g;
    char *q = p;
    for (int i = 0; i < x; i++) { arr[i] = *q; }
    return x;
}
";
    let want = [
        "a: 1d 2",
        "p: 1d 3",
        "arr: 1d 4",
        "x: 2d 4 5",
        "q: 3d 4",
        "i: 4d 4 4 4",
    ];
    assert_eq!(symbols("c", src), want);
    assert_eq!(symbols("cpp", src), want);
    let cpp = "void f(std::vector<int>& v) {
    for (auto& e : v) { use(e); }
    auto l = [](int y) { return y; };
    l(1);
}
";
    assert_eq!(
        symbols("cpp", cpp),
        ["v: 0d 1", "e: 1d 1", "l: 2d 3", "y: 2d 2"]
    );
}

#[test]
fn positions_are_utf16() {
    let locals = analyze(
        "go",
        "package p\nfunc f() {\n\ts := \"é😀\"; t := s\n}\n".as_bytes(),
    )
    .unwrap();
    let t = locals.occurrences.iter().find(|o| o.start > 8).unwrap();
    // `t` follows `"é😀"; ` : the emoji is two UTF-16 units.
    assert_eq!((t.line, t.start, t.end), (2, 13, 14));
    assert!(locals.at(2, 13).is_some());
    assert_eq!(locals.of(0).count(), 2);
    assert_eq!(scip_symbol(3), "local 3");
}
