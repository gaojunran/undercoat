# 编译流水线

一个样式入口文件进来，出去的是一段 CSS 字节（可选的还有 `.map`）。中间固定经过六段：
文本装配成一份合并串、交给 oxc 解析成带位置的语法树、方言各自的求值器生成共享 IR、
最后按方言的产物形态打印成字节。

这篇写给要动编译器内部的人：每一段做什么、代码落在哪个符号上、哪些东西是刻意交给
`oxc-css-parser`、哪些是本 crate 自己的语义。方言之间「为什么不共用」见
[architecture.md](architecture.md)，文末有一张整条链路的图。

入口是 `src/lib.rs` 的 `compile_file`：`.less` 进 less 通道，`.scss` / `.sass` / `.css` /
`.wxss` 进 scss 通道（`.sass` 在装配期折成等义 SCSS 文本）。两条通道各有自己的装配（①）
与解析驱动（②），共用的是 `merge` 的段表与 `doc` / `print` / `sourcemap` 这一层。

## 六段的边界

| 阶段 | 做什么 | 代码位置 |
| --- | --- | --- |
| ① 装配 | `@import` / `@use` / `@forward` 解析、候选枚举、文本内联；把 `.sass` 缩进语法折成等义 SCSS。所有片段拼成**一份合并串**，同时按片段记下段表（输出区间对应到哪个文件的哪个偏移） | `scss/imports.rs`、`scss/indented.rs`、`less/imports.rs`、`merge.rs` |
| ② 解析 | 只做词法与语法，产出带 `Span`（字节区间）的 AST | `scss/mod.rs`、`less/mod.rs` |
| ③ 求值 | less：边遍历边求值，产出 IR；scss：求值成带类型的 `Value` 再生成 IR | `less/eval.rs`、`scss/eval.rs` |
| ④ IR | 两个方言共用的输出中间表示：`Rule` / `AtRule` / `Decl` / `Comment` / `Keyframe`，每个节点带源锚点 | `doc.rs` |
| ⑤ 打印 | 按 `Style` 写字节并记 sourcemap 映射；两种方言的形态差异全部收敛在 `Style` | `doc.rs`、`print.rs` |
| ⑥ sourcemap | 把打印时记下的 `(输出偏移, 源 Span)` 换回文件与行列，VLQ 编码 | `sourcemap.rs`、`merge.rs`、`lib.rs` |

下面以 `a.scss` 为例走一遍；less 同构，差异点单独标出。

## 1. 装配（`scss::imports::Loader`）

这一阶段**还不碰 AST**，全是文本与路径操作：

- `@import`：按 dart 的候选枚举找文件（`_partial` 前缀、目录 index、`.scss/.sass/.css` 后缀），递归内联；
- `@use` / `@forward`：每个模块只装载一次，把模块内容**排到依赖者之前**，同时登记 `ModuleInfo { file, scope }`；模块不共享作用域，靠 scope 编号隔离；
- `.sass` 缩进语法在这里折成等义 SCSS（`scss/indented.rs`）；
- 所有片段拼成一份**合并串** `Merged.text`，每段同时记一条 `Seg { start, end, file, orig }`。

**代码位置**：`src/scss/mod.rs` 的 `compile` 调用 `Loader::new(...).run()`（`src/scss/imports.rs`
的 `Loader`，返回 `Loaded { merged, deps, modules, scopes, scope_of }`）；缩进语法走
`indented::to_scss`；合并串与段表是 `src/merge.rs` 的 `Merged` / `Seg`，片段靠
`Merged::append_part` 追加。less 侧对应 `src/less/mod.rs` 的 `compile` 调
`less::imports::expand_imports`。

为什么用合并串而不是「每个文件一棵 AST」：`@import` 的语义就是文本级搬移（共享作用域、嵌套位置不变），splice 之后后面所有阶段都只需要一套偏移坐标；出错时用段表把合并串偏移换回「哪个文件第几行」。

## 2. 解析（oxc-css-parser）

```rust
let alloc = Allocator::default();
let mut parser = ParserBuilder::new(&alloc, merged.text.as_str())
    .syntax(Syntax::Scss)      // less 通道：Syntax::Less
    .comments()                // 额外保留块注释的位置与内容
    .build();
let ast = parser.parse::<Stylesheet>()?;   // oxc 的 AST，节点带 oxc_css_parser::pos::Span
```

oxc 在这里只承担**词法 + 语法**两件事，产出带 `Span`（字节区间）的 AST。我们不用它的 visitor、不用它的 printer、也从不把它的 AST 直接序列化。两个方言的 `Syntax` 都是它支持的（`Syntax::Less` / `Syntax::Scss`），所以两条通道共用同一个解析器版本，升级只动一处依赖。

解析错误发生在合并串坐标上，所以报错要先过段表：`merged.locate_exact(span.start)` 先拿到「文件 + 文件内偏移」，再经 `position_in(file, local)` 换成「文件 + 行列」。

**代码位置**：`src/scss/mod.rs` 的 `compile`（`.syntax(Syntax::Scss)`）与 `src/less/mod.rs`
的 `compile`（`.syntax(Syntax::Less)`）。报错换算在 `src/merge.rs` 的 `Merged::locate_exact`
与 `Merged::position_in`；less 通道在 `compile` 里显式走这两步，scss 通道把它收进
`Merged::error_at`。

## 3. 求值（方言分开的最后一块）

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

**代码位置**：scss 是 `src/scss/mod.rs` 的 `Converter`；less 是 `src/less/mod.rs` 的
`Compiler`。上面列的 `decls.rs` / `flow.rs` / `value.rs` … 都在 `src/scss/` 与 `src/less/` 下。

## 4. IR（`doc.rs`）

```text
Item   = Rule | Keyframe | AtRule | Comment | Raw
Body   = Rules(Vec<Item>) | Decls(Vec<DeclItem>)
DeclItem = Decl | Comment | AtRule(无块)
```

每个节点带源 `Span`；`Rule.selector` 是 `SelectorLines`——**按输出行拆开**的 `(行文本, Span)` 列表。拆行的唯一原因是 sourcemap：dart 把多行选择器列表的每一行都映射到列表起点，不拆行就只能给第一行打点（这一条修完 bootstrap5 的 252 处映射缺口才归零）。

`Item::Raw` 是逃生舱，只有四类「本来就是任意 CSS 文本」的东西走它：`@import (inline)` 的文件内容、less 的子编译产物、`e("…")` 内文、`(reference)` 的紧凑声明体。

**代码位置**：`src/doc.rs` 的 `Item` / `Body` / `DeclItem` / `Rule` / `Raw`；`SelectorLines`
是 `Vec<(String, Span)>` 的类型别名，拆行函数是同文件的 `selector_lines`。`Style` 定义在
`src/doc.rs` 顶部（`Style::SASS` / `Style::LESS` 两个常量）。

## 5. 打印（`doc::print` 调 `print::Printer`）

```rust
let (css, map) = print(&items, &Style::SASS, entry, &merged, opts);
```

打印器是**只追加**的：每个来自源码的 token 走 `Printer::token(text, span)`（记一条 `Pending { offset, file, span }`），缩进/换行/逗号这类生成物走 `Printer::raw`。因为「能不能反查原始位置」由打印时是否带 span 决定，事后不需要再补映射，也不可能出现「改了已写字节但映射没跟着改」。

`Style` 是两条通道在产出层唯一允许不一致的地方（6 个字段：`blank_line_after_rule` /
`indent_selector_continuation` / `charset` / `trailing_newline` / `sources_content` /
`verbatim_comments`，逐个取值见 architecture.md）。

**代码位置**：`src/doc.rs` 的 `print` 与 `emit_item` / `emit_items` / `emit_selector`；
`src/print.rs` 的 `Printer`（`token` / `token_in` / `raw` / `newline` / `indent` / `finish`）
与私有的 `Pending`。

## 6. sourcemap（`sourcemap.rs`）

打印结束后，`Printer` 手里是 `(输出字节偏移, 源文件, 源 Span)` 的列表；逐条用 `merge` 的段表换回「文件 + 原始偏移」，再用该文件的 `LineIndex` 换算行列（列为 UTF-16），最后 VLQ 编码成 `mappings`。`sourcesContent` 是否输出由 `Style::sources_content` 决定（less 单源不写、多源写；dart 恒写）。

**代码位置**：`src/print.rs` 的 `Printer::finish`（`locate` 查段表）；`src/sourcemap.rs` 的
`vlq` / `encode_mappings` / `to_json`；`src/lib.rs` 的 `LineIndex::line_col`（列为 UTF-16）。

## oxc 的边界（刻意不越界）

| 用了 | 没用（以及为什么） |
| --- | --- |
| `ParserBuilder` + `Syntax::{Less,Scss}` | 不用它的 printer：产物形态要逐字节对齐 lessc / dart-sass，必须自己控制缩进、空行、`@charset`、选择器续行 |
| `Span`（字节区间，所有节点都带） | 不用它的 visitor：求值要控制帧栈、模块 scope、`@extend` 顺序，遍历顺序是我们的语义 |
| `parser.comments()`（块注释的内容与位置） | 不把 AST 直接当 IR：AST 是「语法形状」，IR 是「产物形状」，中间隔着求值 |

一句话：**oxc 负责把文本变成带位置的语法树，剩下的一切（装配、求值、形态、sourcemap）都是本 crate 的语义**。

## 现状

公共层小而稳定，体量几乎都在两个方言的求值上：

| 部分 | 行数 |
| --- | --- |
| `src/lib.rs`（入口分流、`Options` / `Output`、`LineIndex`） | 104 |
| `src/merge.rs`（合并串 `Merged` + 段表 `Seg`） | 331 |
| `src/doc.rs`（IR、`Style`、打印） | 841 |
| `src/print.rs`（只追加的字节写入器） | 253 |
| `src/sourcemap.rs`（VLQ / `sourcesContent`） | 168 |
| `src/less/*`（less 前端） | 9981 |
| `src/scss/*`（scss 前端） | 18261 |

加上 `src/error.rs`（44 行）与 `src/text.rs`（176 行），`src/` 共 30159 行（不含 `src/bin` 的 CLI）。

对外只有四个产品符号：`compile_file` / `Options` / `Output` / `CompileError`
（`LessOptions` 是 `Options::less` 的字段类型；`LineIndex` 与 `scss` 模块是 `#[doc(hidden)]`
的测试钩子）。也就是说，上面六个阶段全部是内部实现，换掉任何一段都不影响调用方。

## 链路图

主干是两条方言共用的部分，只有 ③ 处分叉再汇合：

![编译流水线](img/pipeline.svg)
