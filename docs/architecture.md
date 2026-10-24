# undercoat 架构

一个 crate 承载两条方言链路（less / scss）。两条链路在**装配**与**产出**上共用同一套
基础设施，只在**求值**上分开——两个方言连「什么是值」都不一致（见文末），强行抽象会让
两侧的逐字节基线同时损坏。

## 流水线

源文件先进装配（`merge::Merged`：合并串 + 偏移段表），交给 `oxc-css-parser` 解析成带 `Span`
的 AST，在求值处分岔成 less / scss 两条支链，再汇入共享的 `doc::Item` IR，由 `doc::print`
按 `Style` 写出字节，`print::Printer` 同时记下 sourcemap 映射，最后得到 CSS 字节与 `.map`。

六段各自的职责、代码落点、`Span` 如何经段表换回「文件 + 行列」，以及刻意不越界的 oxc 边界，
见 [pipeline.md](pipeline.md)：

![编译流水线](img/pipeline.svg)

## 公共层

| 模块 | 职责 |
| --- | --- |
| `lib.rs` | 按后缀分流；对外只暴露 `compile_file` / `Options` / `Output` / `CompileError` |
| `error.rs` | `CompileError`（消息 + 文件/行/列），实现 `std::error::Error` |
| `merge.rs` | 合并串 `Merged` + 偏移段表 `Seg`；`locate_exact` 精确命中，越界回退由调用方决定 |
| `text.rs` | 字节级文本原语：`skip_string` / `skip_comment` / `matching` / `split_top_level` |
| `doc.rs` | 共享 IR、`Style`、打印器 `print` |
| `print.rs` | 只追加的字节写入器（缩进、换行、segment 记录），不回改已写内容 |
| `sourcemap.rs` | sourcemap v3 编码（`mappings` 的 VLQ、`sourcesContent`） |

## 产物形态差异：全部在 `Style`

| 字段 | less | sass | 含义 |
| --- | --- | --- | --- |
| `blank_line_after_rule` | false | true | 规则之间是否空行 |
| `indent_selector_continuation` | true | false | 选择器续行是否缩进（dart 写在第 0 列） |
| `charset` | false | true | 非 ASCII 内容是否补 `@charset` |
| `trailing_newline` | true | false | 产物末尾是否换行（CLI 侧另补） |
| `sources_content` | 按多源 | true | map 是否内嵌源文本 |
| `verbatim_comments` | true | false | 注释是否原样回写（dart 会重排续行缩进） |

`doc::Item::Raw` 是唯一的逃生舱，只给「本来就是任意 CSS 文本」的东西用；哪四类、为什么
不能用结构化节点表达，见 [pipeline.md](pipeline.md) 的 IR 一节。

## 刻意不共用的三件事

| 领域 | 为什么不能合并 |
| --- | --- |
| 值模型与数字精度 | less 的值是「CSS 文本 + 惰性绑定」（变量可指向 AST、文本、列表、颜色、detached ruleset，因为它允许把变量当选择器展开），且每次算术过 8 位小数 `fround`；scss 是带类型的 SassScript 值，序列化保留 10 位。 |
| An+B 归一化 | less 故意**保留** `:nth-child` 参数里的空白，dart-sass **去掉**全部空白。同名函数、相反语义。 |
| `@media` prelude 归一 | 两侧同名函数，实现规模差一个数量级（less 十余行、dart 一百三十余行），合并必然改字节。 |

这三条是「看名字像、看语义反」，注释里已逐个标注，避免后来者顺手统一。

## 测试布局

- `tests/fixtures/`：less 语料，`case.less` 与 `case.css` 成对，逐字节比对。
- `tests/fixtures-scss*/`：scss 语料，按开发批次分组；`main.scss` + 期望产物，
  另有 `*-errors` 目录锁定报错文案与行列。
- `tests/*.rs`：每个文件是一个测试入口（golden / 报错 / sourcemap / 模块系统 /
  `@extend` 选择器 / `.sass` AST 等价）。逐字节基线由 `scripts/style-diff/` 下的对拍
  脚本对 less 4.1.3 与 dart-sass 生成。
