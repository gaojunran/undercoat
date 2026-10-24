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

## 从源文件到字节：完整链路

下面以 `a.scss` 为例走一遍；less 同构，差异点单独标出。

### 1. 装配（`scss::imports::Loader`）

这一阶段**还不碰 AST**，全是文本与路径操作：

- `@import` → 按 dart 的候选枚举找文件（`_partial` 前缀、目录 index、`.scss/.sass/.css` 后缀），递归内联；
- `@use` / `@forward` → 每个模块只装载一次，把模块内容**排到依赖者之前**，同时登记 `ModuleInfo { file, scope }`；模块不共享作用域，靠 scope 编号隔离；
- `.sass` 缩进语法在这里折成等义 SCSS（`scss/indented.rs`）；
- 所有片段拼成一份**合并串** `Merged.text`，每段同时记一条 `Seg { start, end, file, orig_start }`。

为什么用合并串而不是「每个文件一棵 AST」：`@import` 的语义就是文本级搬移（共享作用域、嵌套位置不变），splice 之后后面所有阶段都只需要一套偏移坐标；出错时用段表把合并串偏移换回「哪个文件第几行」。

### 2. 解析（oxc-css-parser）

```rust
let alloc = Allocator::default();
let mut parser = ParserBuilder::new(&alloc, merged.text.as_str())
    .syntax(Syntax::Scss)      // less 通道：Syntax::Less
    .comments()                // 额外保留块注释的位置与内容
    .build();
let ast = parser.parse::<Stylesheet>()?;   // oxc 的 AST，节点带 oxc_css_parser::pos::Span
```

oxc 在这里只承担**词法 + 语法**两件事，产出带 `Span`（字节区间）的 AST。我们不用它的 visitor、不用它的 printer、也从不把它的 AST 直接序列化。两个方言的 `Syntax` 都是它支持的（`Syntax::Less` / `Syntax::Scss`），所以两条通道共用同一个解析器版本，升级只动一处依赖。

解析错误发生在合并串坐标上，所以报错要先过段表：`merged.locate_exact(span.start)` → `position_in(file, local)` → 「文件 + 行列」。

### 3. 求值（方言分开的最后一块）

两条通道从这里开始分叉——**这是整条链路上唯一真正方言相关的地方**：

- **scss**（`scss::Converter` 遍历 `ast.statements`）：
  - `decls.rs`：变量声明、嵌套属性、自定义属性；
  - `flow.rs`：`@mixin`/`@include`/`@content`/`@if`/`@each`/`@for`/`@while`，帧栈与作用域在这里；
  - `eval.rs` + `value.rs` + `builtins.rs` + `modules.rs`：带类型的 SassScript 值（数字带复合单位、颜色全程 f64 只在序列化取整、列表/映射/字符串）、内置函数与内置模块成员表；
  - `selector.rs`：嵌套展开（`&` 解析、`@at-root`）、`@extend` 的选择器合并；
  - `prelude.rs`：`@media`/`@supports` 的嵌套合并与变量替换；
  - `module_use.rs`：命名空间成员解析、`@forward` 惰性转发边、`@use … with` 配置。
- **less**（`less::Compiler` 遍历语句，边遍历边求值）：
  - `env.rs` 维护帧/作用域与惰性绑定；`eval.rs` 做 mixin 展开、guard、`@each`、变量求值；
  - `value.rs` 是「CSS 文本 + 惰性绑定」模型，算术按 less 的 8 位精度；
  - `guard.rs`、`file_fn.rs`（`data-uri`/`svg-gradient` 等）、`extend.rs`（`&:extend()`）。

求值阶段的输出统一是 `Vec<doc::Item>`——**不是字符串**。早期 less 是「边求值边写缓冲区」，导致 `@extend` 只能事后回头改已输出的文本；现在两边都先建 IR，`@extend` 直接改 IR 里的选择器，打印器也不需要「改写已写字节」这类接口。

### 4. IR（`doc.rs`）

```text
Item   = Rule | Keyframe | AtRule | Comment | Raw
Body   = Rules(Vec<Item>) | Decls(Vec<DeclItem>)
DeclItem = Decl | Comment | AtRule(无块)
```

每个节点带源 `Span`；`Rule.selector` 是 `SelectorLines`——**按输出行拆开**的 `(行文本, Span)` 列表。拆行的唯一原因是 sourcemap：dart 把多行选择器列表的每一行都映射到列表起点，不拆行就只能给第一行打点（这一条修完 bootstrap5 的 252 处映射缺口才归零）。

`Item::Raw` 是逃生舱，只有四类「本来就是任意 CSS 文本」的东西走它：`@import (inline)` 的文件内容、less 的子编译产物、`e("…")` 内文、`(reference)` 的紧凑声明体。

### 5. 打印（`doc::print` → `print::Printer`）

```rust
let (css, map) = print(&items, &Style::SASS, entry, &merged, opts);
```

打印器是**只追加**的：每个来自源码的 token 走 `Printer::token(text, span)`（记一条 `Pending { offset, file, span }`），缩进/换行/逗号这类生成物走 `Printer::raw`。因为「能不能反查原始位置」由打印时是否带 span 决定，事后不需要再补映射，也不可能出现「改了已写字节但映射没跟着改」。

`Style` 是两条通道在产出层唯一允许不一致的地方（6 个字段，见下一节）。

### 6. sourcemap（`sourcemap.rs`）

打印结束后，`Printer` 手里是 `(输出字节偏移, 源文件, 源 Span)` 的列表；逐条用 `merge` 的段表换回「文件 + 原始偏移」，再用该文件的 `LineIndex` 换算行列（列为 UTF-16），最后 VLQ 编码成 `mappings`。`sourcesContent` 是否输出由 `Style::sources_content` 决定（less 单源不写、多源写；dart 恒写）。

## oxc 的边界（刻意不越界）

| 用了 | 没用（以及为什么） |
| --- | --- |
| `ParserBuilder` + `Syntax::{Less,Scss}` | 不用它的 printer：产物形态要逐字节对齐 lessc / dart-sass，必须自己控制缩进、空行、`@charset`、选择器续行 |
| `Span`（字节区间，所有节点都带） | 不用它的 visitor：求值要控制帧栈、模块 scope、`@extend` 顺序，遍历顺序是我们的语义 |
| `parser.comments()`（块注释的内容与位置） | 不把 AST 直接当 IR：AST 是「语法形状」，IR 是「产物形状」，中间隔着求值 |

一句话：**oxc 负责把文本变成带位置的语法树，剩下的一切（装配、求值、形态、sourcemap）都是本 crate 的语义**。

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
