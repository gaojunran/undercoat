# undercoat architecture

One crate carries two dialect chains (less / scss). They share one set of infrastructure for
**assembly** and **output**, and part ways only in **evaluation** — the two dialects do not even
agree on what a value is (see the end of this page), and forcing an abstraction over that would
break the byte-exact baselines on both sides at once.

## Pipeline

A source file enters assembly (`merge::Merged`: merged text + segment table), is handed to
`oxc-css-parser` for parsing into an AST carrying `Span`s, forks into a less branch and a scss
branch at evaluation, flows back together into the shared `doc::Item` IR, is written out as bytes by
`doc::print` according to `Style`, with `print::Printer` recording source map entries along the way
— and comes out as CSS bytes plus a `.map`.

How the six stages divide the work, where their code sits, how a `Span` is mapped back through the
segment table to a file plus line/column, and where oxc deliberately stops, are all in
[pipeline.md](pipeline.md):

![Compilation pipeline](img/pipeline.svg)

## Shared layer

| Module | Responsibility |
| --- | --- |
| `lib.rs` | Dispatches on the file extension; the only public surface is `compile_file` / `Options` / `Output` / `CompileError` |
| `error.rs` | `CompileError` (message + file/line/column), implementing `std::error::Error` |
| `merge.rs` | Merged text `Merged` + offset segment table `Seg`; `locate_exact` requires an exact hit, and out-of-range fallback is left to the caller |
| `text.rs` | Byte-level text primitives: `skip_string` / `skip_comment` / `matching` / `split_top_level` |
| `doc.rs` | Shared IR, `Style`, printer `print` |
| `print.rs` | Append-only byte writer (indentation, newlines, segment recording); never rewrites what it has already written |
| `sourcemap.rs` | source map v3 encoding (VLQ for `mappings`, `sourcesContent`) |

## Output shape differences: all in `Style`

| Field | less | sass | Meaning |
| --- | --- | --- | --- |
| `blank_line_after_rule` | false | true | blank line between rules |
| `indent_selector_continuation` | true | false | indent continuation lines of a selector list (dart writes them at column 0) |
| `charset` | false | true | emit `@charset` when the output contains non-ASCII |
| `trailing_newline` | true | false | trailing newline at end of output (the CLI adds its own) |
| `sources_content` | multi-source | true | embed source text in the map |
| `verbatim_comments` | true | false | write comments verbatim (dart re-indents continuation lines) |

`doc::Item::Raw` is the only escape hatch, reserved for things that are "arbitrary CSS text" by
nature; which four take it, and why they cannot be expressed as structured nodes, is in the IR
section of [pipeline.md](pipeline.md).

## The three things deliberately not shared

| Area | Why they cannot be merged |
| --- | --- |
| Value model and numeric precision | A less value is "CSS text + lazy bindings" (a variable may point at an AST, at text, at a list, at a color, at a detached ruleset, because less allows using a variable as a selector), and every arithmetic step runs through `fround` at 8 decimals; scss has typed SassScript values, serialized with 10. |
| An+B normalization | less deliberately **keeps** the whitespace inside `:nth-child` arguments, dart-sass **removes** all of it. Same-named functions, opposite semantics. |
| `@media` prelude normalization | Same-named functions on both sides, implementations an order of magnitude apart (a dozen lines on the less side, a hundred and thirty-odd on the dart side); merging them would necessarily change bytes. |

These three look alike by name and are opposite in semantics; each one is marked in the comments so
that nobody later "helpfully" unifies them.

## Test layout

- `tests/fixtures/`: the less corpus, `case.less` paired with `case.css`, compared byte for byte.
- `tests/fixtures-scss*/`: the scss corpus, grouped by development batch; `main.scss` plus the
  expected output, with separate `*-errors` directories that pin error messages and line/column.
- `tests/*.rs`: one test entry per file (golden / errors / source maps / module system /
  `@extend` selectors / `.sass` AST equivalence). Byte-exact baselines are regenerated with
  `tools/undercoat-diff`, which compares against less 4.1.3 and dart-sass.
