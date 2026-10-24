# undercoat

[![ci](https://github.com/gaojunran/undercoat/actions/workflows/ci.yml/badge.svg)](https://github.com/gaojunran/undercoat/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/undercoat.svg)](https://crates.io/crates/undercoat)
[![docs.rs](https://docs.rs/undercoat/badge.svg)](https://docs.rs/undercoat)

A native Rust CSS preprocessor. One engine compiles both **Less** and **Sass/SCSS**,
and its output is byte-compatible with the reference implementations — no Node runtime.

The name is the paint layer beneath the finish: undercoat takes Less / Sass sources,
flattens them into plain CSS, and hands them to the browser.

```console
$ undercoat app.scss -o app.css --source-map
$ undercoat app.less  -o app.css
```

## Why

- **Two dialects, one pipeline.** `@import` / `@use` / `@forward` assembly, evaluation,
  `@extend` resolution and source-map generation share one implementation. A dialect
  differs only in its value model and six output-shape switches.
- **Byte-compatible with the references.** Less targets `lessc` 4.1.3; SCSS targets
  dart-sass language semantics at 1.93.2 and output shape at 1.35.2 (same input ⇒ the
  same CSS text). Bootstrap 4/5, Foundation, Bulma and element-plus are part of the
  regression corpus.
- **Source maps you can trust.** Multi-line selector lists, comment continuations and
  rules nested in `@media` all map line by line, matching dart-sass coverage.
- **No Node.js.** A single binary or a plain library call — embeddable in build tools,
  bundlers and CI.

## Status

Both dialects are usable today: `.less`, `.scss`, `.sass` and plain `.css` / `.wxss`.
Known deviations are limited to text shape of colour/`calc()` rounding (numeric
equivalents) and a few edge-case constructs that are reported as errors instead of
silently emitting wrong CSS.

## CLI

```console
$ undercoat <file.(less|scss|css|wxss)> [--source-map] [--project-root DIR] [-o OUT]
```

## Library

```rust
use std::path::Path;
use undercoat::{compile_file, Options};

let opts = Options { paths: vec!["/abs/styles".into()], ..Default::default() };
let out = compile_file(
    Path::new("/src/app.scss"),
    "@use 'sass:math'; a { width: math.div(1px, 3) }",
    &opts,
)
.unwrap();

println!("{}", out.css);
```

`compile_file` dispatches on the file extension: `.less` goes to the Less channel;
`.scss` / `.sass` / `.css` / `.wxss` go to the SCSS channel (`.sass` indented syntax is
folded into equivalent SCSS text at load time). `Output` carries `css`, an optional
source map and the absolute paths of every dependency; errors are a `CompileError`
with file / line / column.

## Repository layout

```
src/                  the library (one module per concept, shared across dialects)
  doc.rs              output IR + `Style` (the only place dialect shapes differ) + printer
  merge.rs            merged-source string and offset segment table (imports, maps)
  text.rs             byte-level scanning primitives
  less/               Less front end: env, eval, selector, prelude, imports, comments, emit
  scss/               SCSS front end: decls, flow, module_use, selector, prelude, emit
tests/                golden / conformance / source-map suites
docs/architecture.md  module responsibilities and the boundaries between dialects
docs/pipeline.md      the six stages from source text to CSS bytes, with code locations
tools/undercoat-diff  semantic CSS comparator used for conformance runs
```

## Development

```console
$ cargo test            # unit + golden + conformance suites
$ cargo run --bin undercoat -- tests/fixtures/<case>/main.scss -o /tmp/out.css
```

The conformance corpus (`bootstrap`, `foundation`, `bulma`, …) is not vendored; suites
that need it take a directory via environment variable (for example
`SCSS_SASS_CORPUS`) and are skipped when it is absent.

## License

MIT. See [LICENSE](https://github.com/gaojunran/undercoat/blob/main/LICENSE).
