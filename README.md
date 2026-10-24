# Undercoat

原生 Rust 的 CSS 预处理器：同一套引擎同时编译 **Less** 与 **Sass/SCSS**，产物与参照实现
逐字节一致，不需要 Node 运行时。

名字取「底漆」之意：Undercoat 是 CSS 之下的那一层——把 Less / Sass 源样式压平成标准
CSS，再交给浏览器渲染。

```console
$ undercoat app.scss -o app.css --source-map
```

## 特点

- **两种方言，一条流水线**：`@import` / `@use` / `@forward` 装配、求值、`@extend`
  展开、sourcemap 生成共用一套基础设施；方言差异只体现在值模型与 6 个输出形态开关上。
- **逐字节对齐参照实现**：Less 对齐 lessc 4.1.3；SCSS 能力对齐 dart-sass 1.93.2、
  输出形态对齐 1.35.2。bootstrap 4/5、Foundation、bulma、element-plus 等真实工程
  作为回归语料。
- **sourcemap 可反查**：多行选择器、注释续行、`@media` 内嵌规则逐行锚点，覆盖与
  dart-sass 逐行相等。
- **无 Node 依赖**：单二进制 / 纯库调用，适合嵌入构建工具或 CI。

## 用法

```rust
use std::path::Path;
use undercoat::{compile_file, Options};

let opts = Options { paths: vec!["/abs/styles".into()], ..Default::default() };
let out = compile_file(Path::new("/src/app.scss"), "@use 'sass:math'; a { width: math.div(1px, 3) }", &opts).unwrap();
println!("{}", out.css);
```

`compile_file` 按文件后缀分流：`.less` 走 less 通道；`.scss` / `.sass` / `.css` /
`.wxss` 走 SCSS 通道（`.sass` 缩进语法在装载期折成等义 SCSS 文本）。
返回值 `Output` 含 `css`、可选的 sourcemap `map` 与依赖文件绝对路径 `deps`；
错误统一为 `CompileError`（消息 + 源文件/行/列，解析期与求值期都尽量带上位置）。
模块职责与两条链路的共享边界见 [`docs/architecture.md`](docs/architecture.md)。

## 支持范围

- **Less**：语义对齐 less 4.1.3（变量惰性求值、mixin/守卫、`@import` 内联、
  detached ruleset、extend、`(reference)` 等导入选项）。
- **SCSS / Sass**：能力基线是现代 dart-sass 1.93.2（模块系统、内置函数、
  控制流、`@extend`、缩进语法），输出形态基线是 dart-sass 1.35.2——
  即同一份输入的 CSS 文本与 1.35.2 逐字节一致。
- **CSS**：`.css` / `.wxss` 走同一条通道，按原样完成变量与 `@import` 处理后输出。

## 已知差异

与上述基线相比，残留差异集中在颜色与 `calc()` 的取整/化简形态（数值等价但文本不同），
以及少数边界语法会显式报错而不是静默产出错误 CSS。

## License

MIT
