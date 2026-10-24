# undercoat 架构

一个 crate 承载两条方言链路（less / scss）。两条链路在**装配**与**产出**上共用同一套
基础设施，只在**求值**上分开——两个方言连「什么是值」都不一致（见文末），强行抽象会让
两侧的逐字节基线同时损坏。

## 流水线

```
                   ┌─ less: 解析即求值（流式），值模型 = CSS 文本 + 惰性绑定
源文件 → 装载 → 求值 ┤
   ↑               └─ scss: 解析 → 求值 → Vec<Item>
   │                                │
   └─ merge::Merged                 ↓
      （合并串 + 偏移段表）    doc::print(items, &Style, …)
                                   ↓
                            print::Printer → CSS + sourcemap
```

四个阶段各自的职责边界：

| 阶段 | 做什么 | 代码 |
| --- | --- | --- |
| 装载 | `@import` / `@use` / `@forward` 解析、候选枚举、文本内联；把 `.sass` 缩进语法折成等义 SCSS。所有片段拼成**一份合并串**，同时记录 `(输出区间 → 源文件/偏移)` 段表 | `less/imports.rs`、`scss/imports.rs`、`scss/indented.rs` |
| 求值 | less：边遍历边求值，产出 IR；scss：求值成带类型的 `Value` 再生成 IR | `less/eval.rs`、`scss/eval.rs` |
| 产出（IR） | 两个方言共用的输出中间表示：`Rule` / `AtRule` / `Decl` / `Comment` / `Keyframe`，每个节点带源锚点 | `doc.rs` |
| 打印 | 按 `Style` 写字节并记 sourcemap 映射；两种方言的形态差异全部收敛在 `Style` | `doc.rs`、`print.rs`、`sourcemap.rs` |

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

```
blank_line_after_rule        less=false  sass=true    规则之间是否空行
indent_selector_continuation less=true   sass=false   选择器续行是否缩进（dart 写在第 0 列）
charset                      less=false  sass=true    非 ASCII 内容是否补 @charset
trailing_newline             less=true   sass=false   产物末尾是否换行（CLI 侧另补）
sources_content              less=按多源  sass=true    map 是否内嵌源文本
verbatim_comments            less=true   sass=false   注释是否原样回写（dart 会重排续行缩进）
```

`doc::Item::Raw` 是逃生舱，只给「任意 CSS 文本」用：`(inline)` 文件内容、`@import`
子编译产物、`e("…")` 内文、less `(reference)` 的紧凑声明体。任何可以用结构化节点表达的
东西都应该走结构化节点。

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
