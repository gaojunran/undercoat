//! 产出装配：注释游标、静态容器校验、块/声明排序辅助。
//!
//! 由 `scss/mod.rs` 拆分而来（纯搬运）。

use super::*;

impl<'a> Converter<'a> {
    /// 输出游标之前的注释。`#{}` 在这里求值（mixin 体里的横幅注释依赖它）。
    pub(super) fn flush_comments(
        &mut self,
        limit: usize,
        push: &mut dyn FnMut(Comment),
    ) -> Result<(), Box<CompileError>> {
        while let Some(c) = self.comments.get(self.cursor) {
            if c.span.start >= limit {
                break;
            }
            let text = self
                .eval
                .interpolate(&c.text, c.span.start)
                .map_err(|e| self.eval_err(e))?;
            let (_, _, column) = self.merged.position(c.span.start);
            push(Comment {
                text,
                span: c.span,
                column: column as usize,
            });
            self.cursor += 1;
        }
        Ok(())
    }

    pub(super) fn skip_comments(&mut self, limit: usize) {
        while let Some(c) = self.comments.get(self.cursor) {
            if c.span.start >= limit {
                break;
            }
            self.cursor += 1;
        }
    }
}

/// mixin 体（含嵌套块）里是否出现 `@content`：决定传 content 块是否报错。
pub(super) fn has_content(stmts: &[Statement<'_>]) -> bool {
    stmts.iter().any(|st| match st {
        Statement::AtRule(at) => {
            at.name.name.eq_ignore_ascii_case("content")
                || at
                    .block
                    .as_ref()
                    .is_some_and(|b| has_content(&b.statements))
        }
        Statement::QualifiedRule(rule) => has_content(&rule.block.statements),
        Statement::SassIfAtRule(rule) => {
            std::iter::once(&rule.if_clause)
                .chain(rule.else_if_clauses.iter())
                .any(|c| has_content(&c.block.statements))
                || rule
                    .else_clause
                    .as_ref()
                    .is_some_and(|b| has_content(&b.statements))
        }
        _ => false,
    })
}

/// 静态容器校验：一次遍历整份（已合并的）样式表，与是否执行到无关。
pub(super) fn validate_structure(merged: &Merged, stmts: &[Statement<'_>]) -> Result<(), Box<CompileError>> {
    validate_block(merged, stmts, Nesting::default())
}

fn validate_block(
    merged: &Merged,
    stmts: &[Statement<'_>],
    nesting: Nesting,
) -> Result<(), Box<CompileError>> {
    const NOT_ALLOWED: &str = "This at-rule is not allowed here.";
    let fail = |at: usize, message: &str| -> Result<(), Box<CompileError>> {
        Err(merged.error_at(at, message.to_string()).into())
    };
    for stmt in stmts {
        match stmt {
            // function 体只能是「变量 + 控制流 + @return/@debug/@warn/@error」。
            Statement::Declaration(decl) if nesting.function => {
                return fail(
                    decl.span.start,
                    "@function rules may not contain declarations.",
                );
            }
            Statement::QualifiedRule(rule) if nesting.function => {
                return fail(
                    rule.span.start,
                    "@function rules may not contain style rules.",
                );
            }
            Statement::AtRule(at) => {
                let name = at.name.name.to_ascii_lowercase();
                // dart 报的是 at-rule 整体 span 的起点（即 `@`）。
                let at_pos = at.span.start;
                if nesting.function && !Nesting::function_allows(&name) {
                    return fail(at_pos, NOT_ALLOWED);
                }
                match name.as_str() {
                    "mixin" => {
                        if nesting.mixin || nesting.content_block {
                            return fail(at_pos, "Mixins may not contain mixin declarations.");
                        }
                        if nesting.control {
                            return fail(
                                at_pos,
                                "Mixins may not be declared in control directives.",
                            );
                        }
                    }
                    "function" => {
                        if nesting.mixin || nesting.content_block {
                            return fail(at_pos, "Mixins may not contain function declarations.");
                        }
                        if nesting.control {
                            return fail(
                                at_pos,
                                "Functions may not be declared in control directives.",
                            );
                        }
                    }
                    "return" if !nesting.function => return fail(at_pos, NOT_ALLOWED),
                    // content 块里的 `@content` 也是错的（dart 只认 `_inMixin`）。
                    "content" if !nesting.mixin => {
                        return fail(
                            at_pos,
                            "@content is only allowed within mixin declarations.",
                        );
                    }
                    _ => {}
                }
                let inner = match name.as_str() {
                    "mixin" => Nesting {
                        mixin: true,
                        ..nesting
                    },
                    "function" => Nesting {
                        function: true,
                        ..nesting
                    },
                    "include" => Nesting {
                        content_block: true,
                        ..nesting
                    },
                    "if" | "each" | "for" | "while" => Nesting {
                        control: true,
                        ..nesting
                    },
                    _ => nesting,
                };
                if let Some(block) = &at.block {
                    validate_block(merged, &block.statements, inner)?;
                }
            }
            Statement::QualifiedRule(rule) => {
                validate_block(merged, &rule.block.statements, nesting)?;
            }
            Statement::SassIfAtRule(rule) => {
                let inner = Nesting {
                    control: true,
                    ..nesting
                };
                validate_block(merged, &rule.if_clause.block.statements, inner)?;
                for clause in &rule.else_if_clauses {
                    validate_block(merged, &clause.block.statements, inner)?;
                }
                if let Some(block) = &rule.else_clause {
                    validate_block(merged, &block.statements, inner)?;
                }
            }
            Statement::UnknownSassAtRule(rule) => {
                if let Some(block) = &rule.block {
                    validate_block(merged, &block.statements, nesting)?;
                }
            }
            Statement::KeyframeBlock(block) => {
                validate_block(merged, &block.block.statements, nesting)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// 空块会被整块丢弃的条件 at-rule（dart-sass 实测：只有 media / supports）。
pub(super) fn is_conditional(name: &str) -> bool {
    matches!(name, "media" | "supports")
}

/// 体只允许声明的 at-rule（`@font-face` 里出现 `@include` 也要走声明列表）。
pub(super) fn is_decl_at_rule(name: &str) -> bool {
    matches!(
        name,
        "font-face" | "property" | "counter-style" | "font-palette-values" | "viewport"
    )
}

/// 给节点打上「源级规则组的结尾」标记（序列化时空一行）。
pub(super) fn mark_group_end(item: &mut Item) {
    match item {
        Item::Rule(rule) => rule.group_end = true,
        Item::AtRule(at) => at.group_end = true,
        _ => {}
    }
}
