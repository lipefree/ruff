use std::sync::LazyLock;
use {
    itertools::Either::{Left, Right},
    regex::Regex,
};

use ruff_python_ast::{
    self as ast, BytesLiteralFlags, Expr, FStringFlags, FStringPart, InterpolatedStringElement,
    InterpolatedStringLiteralElement, Stmt, StringFlags,
};
use ruff_python_ast::{AtomicNodeIndex, visitor::transformer::Transformer};
use ruff_python_ast::{StringLiteralFlags, visitor::transformer};
use ruff_text_size::{Ranged, TextRange};

/// A struct to normalize AST nodes for the purpose of comparing formatted representations for
/// semantic equivalence.
///
/// Vis-à-vis comparing ASTs, comparing these normalized representations does the following:
/// - Ignores non-abstraction information that we've encoded into the AST, e.g., the difference
///   between `class C: ...` and `class C(): ...`, which is part of our AST but not `CPython`'s.
/// - Normalize strings. The formatter can re-indent docstrings, so we need to compare string
///   contents ignoring whitespace. (Black does the same.)
/// - The formatter can also reformat code snippets when they're Python code, which can of
///   course change the string in arbitrary ways. Black itself does not reformat code snippets,
///   so we carve our own path here by stripping everything that looks like code snippets from
///   string literals.
/// - Ignores nested tuples in deletions. (Black does the same.)
pub(crate) struct Normalizer;

impl Normalizer {
    /// Transform an AST module into a normalized representation.
    #[allow(dead_code)]
    pub(crate) fn visit_module(&self, module: &mut ast::Mod) {
        match module {
            ast::Mod::Module(module) => {
                self.visit_body(&mut module.body);
            }
            ast::Mod::Expression(expression) => {
                self.visit_expr(&mut expression.body);
            }
        }
    }
}

impl Transformer for Normalizer {
    fn visit_stmt(&self, stmt: &mut Stmt) {
        if let Stmt::Delete(delete) = stmt {
            // Treat `del a, b` and `del (a, b)` equivalently.
            delete.targets = delete
                .targets
                .clone()
                .into_iter()
                .flat_map(|target| {
                    if let Expr::Tuple(tuple) = target {
                        Left(tuple.elts.into_iter())
                    } else {
                        Right(std::iter::once(target))
                    }
                })
                .collect();
        }

        transformer::walk_stmt(self, stmt);
    }

    fn visit_expr(&self, expr: &mut Expr) {
        // Ruff supports joining implicitly concatenated strings. The code below implements this
        // at an AST level by joining the string literals in the AST if they can be joined (it doesn't mean that
        // they'll be joined in the formatted output but they could).
        // Comparable expression handles some of this by comparing the concatenated string
        // but not joining here doesn't play nicely with other string normalizations done in the
        // Normalizer.
        match expr {
            Expr::StringLiteral(string) => {
                if string.value.is_implicit_concatenated() {
                    let can_join = string.value.iter().all(|literal| {
                        !literal.flags.is_triple_quoted() && !literal.flags.prefix().is_raw()
                    });

                    if can_join {
                        string.value = ast::StringLiteralValue::single(ast::StringLiteral {
                            value: Box::from(string.value.to_str()),
                            range: string.range,
                            flags: StringLiteralFlags::empty(),
                            node_index: AtomicNodeIndex::dummy(),
                        });
                    }
                }
            }

            Expr::BytesLiteral(bytes) => {
                if bytes.value.is_implicit_concatenated() {
                    let can_join = bytes.value.iter().all(|literal| {
                        !literal.flags.is_triple_quoted() && !literal.flags.prefix().is_raw()
                    });

                    if can_join {
                        bytes.value = ast::BytesLiteralValue::single(ast::BytesLiteral {
                            value: bytes.value.bytes().collect(),
                            range: bytes.range,
                            flags: BytesLiteralFlags::empty(),
                            node_index: AtomicNodeIndex::dummy(),
                        });
                    }
                }
            }

            Expr::FString(fstring) => {
                if fstring.value.is_implicit_concatenated() {
                    let can_join = fstring.value.iter().all(|part| match part {
                        FStringPart::Literal(literal) => {
                            !literal.flags.is_triple_quoted() && !literal.flags.prefix().is_raw()
                        }
                        FStringPart::FString(string) => {
                            !string.flags.is_triple_quoted() && !string.flags.prefix().is_raw()
                        }
                    });

                    if can_join {
                        #[derive(Default)]
                        struct Collector {
                            elements: Vec<InterpolatedStringElement>,
                        }

                        impl Collector {
                            // The logic for concatenating adjacent string literals
                            // occurs here, implicitly: when we encounter a sequence
                            // of string literals, the first gets pushed to the
                            // `elements` vector, while subsequent strings
                            // are concatenated onto this top string.
                            fn push_literal(&mut self, literal: &str, range: TextRange) {
                                if let Some(InterpolatedStringElement::Literal(existing_literal)) =
                                    self.elements.last_mut()
                                {
                                    let value = std::mem::take(&mut existing_literal.value);
                                    let mut value = value.into_string();
                                    value.push_str(literal);
                                    existing_literal.value = value.into_boxed_str();
                                    existing_literal.range =
                                        TextRange::new(existing_literal.start(), range.end());
                                } else {
                                    self.elements.push(InterpolatedStringElement::Literal(
                                        InterpolatedStringLiteralElement {
                                            range,
                                            value: literal.into(),
                                            node_index: AtomicNodeIndex::dummy(),
                                        },
                                    ));
                                }
                            }

                            fn push_expression(&mut self, expression: ast::InterpolatedElement) {
                                self.elements
                                    .push(InterpolatedStringElement::Interpolation(expression));
                            }
                        }

                        let mut collector = Collector::default();

                        for part in &fstring.value {
                            match part {
                                ast::FStringPart::Literal(string_literal) => {
                                    collector
                                        .push_literal(&string_literal.value, string_literal.range);
                                }
                                ast::FStringPart::FString(fstring) => {
                                    for element in &fstring.elements {
                                        match element {
                                            ast::InterpolatedStringElement::Literal(literal) => {
                                                collector
                                                    .push_literal(&literal.value, literal.range);
                                            }
                                            ast::InterpolatedStringElement::Interpolation(
                                                expression,
                                            ) => {
                                                collector.push_expression(expression.clone());
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        fstring.value = ast::FStringValue::single(ast::FString {
                            elements: collector.elements.into(),
                            range: fstring.range,
                            flags: FStringFlags::empty(),
                            node_index: AtomicNodeIndex::dummy(),
                        });
                    }
                }
            }

            _ => {}
        }
        transformer::walk_expr(self, expr);
    }

    fn visit_interpolated_string_element(
        &self,
        interpolated_string_element: &mut InterpolatedStringElement,
    ) {
        let InterpolatedStringElement::Interpolation(interpolation) = interpolated_string_element
        else {
            return;
        };

        let Some(debug) = &mut interpolation.debug_text else {
            return;
        };

        // Changing the newlines to the configured newline is okay because Python normalizes all newlines to `\n`
        debug.leading = debug.leading.replace("\r\n", "\n").replace('\r', "\n");
        debug.trailing = debug.trailing.replace("\r\n", "\n").replace('\r', "\n");
    }

    fn visit_string_literal(&self, string_literal: &mut ast::StringLiteral) {
        static STRIP_DOC_TESTS: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"(?mx)
                    (
                        # strip doctest PS1 prompt lines
                        ^\s*>>>\s.*(\n|$)
                        |
                        # strip doctest PS2 prompt lines
                        # Also handles the case of an empty ... line.
                        ^\s*\.\.\.((\n|$)|\s.*(\n|$))
                    )+
                ",
            )
            .unwrap()
        });
        static STRIP_RST_BLOCKS: LazyLock<Regex> = LazyLock::new(|| {
            // This is kind of unfortunate, but it's pretty tricky (likely
            // impossible) to detect a reStructuredText block with a simple
            // regex. So we just look for the start of a block and remove
            // everything after it. Talk about a hammer.
            Regex::new(r"::(?s:.*)").unwrap()
        });
        static STRIP_MARKDOWN_BLOCKS: LazyLock<Regex> = LazyLock::new(|| {
            // This covers more than valid Markdown blocks, but that's OK.
            Regex::new(r"(```|~~~)\p{any}*(```|~~~|$)").unwrap()
        });

        // Start by (1) stripping everything that looks like a code
        // snippet, since code snippets may be completely reformatted if
        // they are Python code.
        string_literal.value = STRIP_DOC_TESTS
            .replace_all(
                &string_literal.value,
                "<DOCTEST-CODE-SNIPPET: Removed by normalizer>\n",
            )
            .into_owned()
            .into_boxed_str();
        string_literal.value = STRIP_RST_BLOCKS
            .replace_all(
                &string_literal.value,
                "<RSTBLOCK-CODE-SNIPPET: Removed by normalizer>\n",
            )
            .into_owned()
            .into_boxed_str();
        string_literal.value = STRIP_MARKDOWN_BLOCKS
            .replace_all(
                &string_literal.value,
                "<MARKDOWN-CODE-SNIPPET: Removed by normalizer>\n",
            )
            .into_owned()
            .into_boxed_str();
        // Normalize a string by (2) stripping any leading and trailing space from each
        // line, and (3) removing any blank lines from the start and end of the string.
        string_literal.value = string_literal
            .value
            .lines()
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned()
            .into_boxed_str();
    }
}
