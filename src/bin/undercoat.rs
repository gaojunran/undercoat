//! 对拍/调试 CLI：`undercoat <file.(less|scss|css|wxss)> [--source-map] [--project-root DIR] [-o OUT]`

use std::path::PathBuf;

use undercoat::{Options, compile_file};

fn dump_statements(path: &std::path::Path, source: &str) {
    use oxc_css_parser::ast::{Statement, Stylesheet};
    use oxc_css_parser::{Allocator, ParserBuilder, Syntax};
    let syntax = if path.extension().is_some_and(|e| e == "less") {
        Syntax::Less
    } else {
        Syntax::Scss
    };
    let alloc = Allocator::default();
    let mut parser = ParserBuilder::new(&alloc, source)
        .syntax(syntax)
        .comments()
        .build();
    let ast: Stylesheet = match parser.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("parse error: {e:?}");
            return;
        }
    };
    fn kind(st: &Statement) -> &'static str {
        match st {
            Statement::Declaration(_) => "Declaration",
            Statement::QualifiedRule(_) => "QualifiedRule",
            Statement::AtRule(_) => "AtRule",
            Statement::LessVariableDeclaration(_) => "LessVarDecl",
            Statement::LessMixinCall(_) => "LessMixinCall",
            Statement::LessMixinDefinition(_) => "LessMixinDef",
            Statement::LessConditionalQualifiedRule(_) => "LessCondRule",
            _ => "Other",
        }
    }
    fn walk(stmts: &[Statement], src: &str, depth: usize) {
        for st in stmts {
            let s = st.span();
            let text = &src[s.start.min(src.len())..s.end.min(src.len())];
            let preview: String = text.chars().take(60).collect();
            eprintln!(
                "{}{} {:?} {:?}",
                "  ".repeat(depth),
                kind(st),
                (s.start, s.end),
                preview.replace('\n', "\\n")
            );
            if let Statement::QualifiedRule(r) = st {
                walk(&r.block.statements, src, depth + 1);
            }
            if let Statement::AtRule(a) = st {
                if let Some(b) = &a.block {
                    walk(&b.statements, src, depth + 1);
                }
            }
        }
    }
    eprintln!("=== {}", path.display());
    walk(&ast.statements, source, 0);
}

fn main() {
    let mut file: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut opts = Options::default();
    let mut dump = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dump" => dump = true,
            "--source-map" => opts.source_map = true,
            "--project-root" => opts.project_root = args.next(),
            "-o" | "--out" => out = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                eprintln!(
                    "usage: undercoat <file.(less|scss|css|wxss)> [--source-map] [--project-root DIR] [-o OUT]"
                );
                return;
            }
            other => file = Some(PathBuf::from(other)),
        }
    }
    let Some(file) = file else {
        eprintln!(
            "usage: undercoat <file.(less|scss|css|wxss)> [--source-map] [--project-root DIR] [-o OUT]"
        );
        std::process::exit(2);
    };
    let source = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("read {}: {e}", file.display());
            std::process::exit(1);
        }
    };
    if dump {
        dump_statements(&file, &source);
    }
    match compile_file(&file, &source, &opts) {
        Ok(output) => {
            match &out {
                Some(path) => {
                    // dart-sass CLI 写文件时补尾随换行；API 产物本身没有。
                    let body = if file.extension().is_some_and(|e| e != "less") {
                        format!("{}\n", output.css)
                    } else {
                        output.css.clone()
                    };
                    if let Err(e) = std::fs::write(path, body) {
                        eprintln!("write {}: {e}", path.display());
                        std::process::exit(1);
                    }
                }
                None => print!("{}", output.css),
            }
            if let Some(map) = &output.map {
                let map_path = out
                    .clone()
                    .map(|p| p.with_extension("css.map"))
                    .unwrap_or_else(|| PathBuf::from(format!("{}.map", file.display())));
                let _ = std::fs::write(&map_path, map);
            }
            for dep in &output.deps {
                eprintln!("dep: {dep}");
            }
        }
        Err(e) => {
            if let (Some(line), Some(column)) = (e.line, e.column) {
                if std::env::var_os("STYLE_NATIVE_TRACE").is_some() {
                    eprintln!(
                        "AT {}:{}:{}",
                        e.path.clone().unwrap_or_default(),
                        line,
                        column
                    );
                }
                eprintln!("LINE {} {}", line, column);
            }
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}
