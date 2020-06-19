//! Passes that resolve the property binding expression.
//!
//! Before this pass, all the expression are of type Expression::Uncompiled,
//! and there should no longer be Uncompiled expression after this pass.
//!
//! Most of the code for the resolving actualy lies in the expression_tree module

use crate::diagnostics::Diagnostics;
use crate::expression_tree::*;
use crate::object_tree::*;
use crate::parser::{syntax_nodes, Spanned, SyntaxKind, SyntaxNode, SyntaxNodeEx};
use crate::typeregister::{Type, TypeRegister};
use core::str::FromStr;
use std::{collections::HashMap, rc::Rc};

pub fn resolve_expressions(doc: &Document, diag: &mut Diagnostics, tr: &mut TypeRegister) {
    for component in &doc.inner_components {
        /// This represeresent a scope for the Component, where Component is the repeated component, but
        /// does not represent a component in the .60 file
        #[derive(Clone)]
        struct ComponentScope(Vec<ElementRc>);
        let scope = ComponentScope(vec![component.root_element.clone()]);

        recurse_elem(&component.root_element, &scope, &mut |elem, scope| {
            let mut scope = scope.clone();
            if elem.borrow().repeated.is_some() {
                scope.0.push(elem.clone())
            }

            // We are taking the binding to mutate them, as we cannot keep a borrow of the element
            // during the creation of the expression (we need to be able to borrow the Element to do lookups)
            // the `bindings` will be reset later
            let mut bindings = std::mem::take(&mut elem.borrow_mut().bindings);
            for (prop, expr) in &mut bindings {
                if let Expression::Uncompiled(node) = expr {
                    let mut lookup_ctx = LookupCtx {
                        tr,
                        property_type: elem.borrow().lookup_property(&*prop),
                        component: component.clone(),
                        component_scope: &scope.0,
                        diag,
                    };

                    let new_expr = if matches!(lookup_ctx.property_type, Type::Signal) {
                        //FIXME: proper signal suport (node is a codeblock)
                        node.child_node(SyntaxKind::Expression)
                            .map(|en| Expression::from_expression_node(en.into(), &mut lookup_ctx))
                            .unwrap_or(Expression::Invalid)
                    } else {
                        Expression::from_binding_expression_node(node.clone(), &mut lookup_ctx)
                    };
                    *expr = new_expr;
                }
            }
            elem.borrow_mut().bindings = bindings;
            let mut repeated = elem.borrow_mut().repeated.take();
            if let Some(r) = &mut repeated {
                if let Expression::Uncompiled(node) = &mut r.model {
                    let mut lookup_ctx = LookupCtx {
                        tr,
                        property_type: Type::Invalid, // FIXME: that should be a model
                        component: component.clone(),
                        component_scope: &scope.0,
                        diag,
                    };
                    r.model = Expression::from_expression_node(node.clone().into(), &mut lookup_ctx)
                        .maybe_convert_to(Type::Model, node, diag)
                }
            }
            elem.borrow_mut().repeated = repeated;
            scope
        })
    }
}

/// Contains information which allow to lookup identifier in expressions
struct LookupCtx<'a> {
    #[allow(dead_code)]
    /// The type register
    tr: &'a TypeRegister,
    /// the type of the property for which this expression refers.
    /// (some property come in the scope)
    property_type: Type,

    /// document_root
    component: Rc<Component>,

    /// Here is the stack in which id applies
    component_scope: &'a [ElementRc],

    /// Somewhere to report diagnostics
    diag: &'a mut Diagnostics,
}

fn find_element_by_id(roots: &[ElementRc], name: &str) -> Option<ElementRc> {
    for e in roots.iter().rev() {
        if e.borrow().id == name {
            return Some(e.clone());
        }
        for x in &e.borrow().children {
            if x.borrow().repeated.is_some() {
                continue;
            }
            if let Some(x) = find_element_by_id(&[x.clone()], name) {
                return Some(x);
            }
        }
    }
    None
}

impl Expression {
    fn from_binding_expression_node(node: SyntaxNode, ctx: &mut LookupCtx) -> Self {
        debug_assert_eq!(node.kind(), SyntaxKind::BindingExpression);
        let e = node
            .child_node(SyntaxKind::Expression)
            .map(|n| Self::from_expression_node(n.into(), ctx))
            .or_else(|| {
                node.child_node(SyntaxKind::CodeBlock).map(|c| Self::from_codeblock_node(c, ctx))
            })
            .unwrap_or(Self::Invalid);
        e.maybe_convert_to(ctx.property_type.clone(), &node, &mut ctx.diag)
    }

    fn from_codeblock_node(node: SyntaxNode, ctx: &mut LookupCtx) -> Expression {
        debug_assert_eq!(node.kind(), SyntaxKind::CodeBlock);
        Expression::CodeBlock(
            node.children()
                .filter(|n| n.kind() == SyntaxKind::Expression)
                .map(|n| Self::from_expression_node(n.into(), ctx))
                .collect(),
        )
    }

    fn from_expression_node(node: syntax_nodes::Expression, ctx: &mut LookupCtx) -> Self {
        node.Expression()
            .map(|n| Self::from_expression_node(n, ctx))
            .or_else(|| {
                node.BangExpression().map(|n| Self::from_bang_expresion_node(n.into(), ctx))
            })
            .or_else(|| node.QualifiedName().map(|s| Self::from_qualified_name_node(s.into(), ctx)))
            .or_else(|| {
                node.child_text(SyntaxKind::StringLiteral).map(|s| {
                    unescape_string(&s).map(Self::StringLiteral).unwrap_or_else(|| {
                        ctx.diag.push_error("Cannot parse string literal".into(), node.span());
                        Self::Invalid
                    })
                })
            })
            .or_else(|| {
                node.child_text(SyntaxKind::NumberLiteral).map(|s| {
                    f64::from_str(&s).ok().map(Self::NumberLiteral).unwrap_or_else(|| {
                        ctx.diag.push_error("Cannot parse number literal".into(), node.span());
                        Self::Invalid
                    })
                })
            })
            .or_else(|| {
                node.child_text(SyntaxKind::ColorLiteral).map(|s| {
                    parse_color_literal(&s)
                        .map(|i| Expression::Cast {
                            from: Box::new(Expression::NumberLiteral(i as _)),
                            to: Type::Color,
                        })
                        .unwrap_or_else(|| {
                            ctx.diag.push_error("Invalid color literal".into(), node.span());
                            Self::Invalid
                        })
                })
            })
            .or_else(|| {
                node.FunctionCallExpression().map(|n| Expression::FunctionCall {
                    function: Box::new(
                        n.child_node(SyntaxKind::Expression)
                            .map(|n| Self::from_expression_node(n.into(), ctx))
                            .unwrap_or(Expression::Invalid),
                    ),
                })
            })
            .or_else(|| node.SelfAssignment().map(|n| Self::from_self_assignement_node(n, ctx)))
            .or_else(|| node.BinaryExpression().map(|n| Self::from_binary_expression_node(n, ctx)))
            .or_else(|| {
                node.ConditionalExpression().map(|n| Self::from_conditional_expression_node(n, ctx))
            })
            .or_else(|| node.ObjectLiteral().map(|n| Self::from_object_literal_node(n, ctx)))
            .or_else(|| node.Array().map(|n| Self::from_array_node(n, ctx)))
            .unwrap_or(Self::Invalid)
    }

    fn from_bang_expresion_node(node: SyntaxNode, ctx: &mut LookupCtx) -> Self {
        match node.child_text(SyntaxKind::Identifier).as_ref().map(|x| x.as_str()) {
            None => {
                debug_assert!(false, "the parser should not allow that");
                ctx.diag.push_error("Missing bang keyword".into(), node.span());
                return Self::Invalid;
            }
            Some("img") => {
                // FIXME: we probably need a better syntax and make this at another level.
                let s = match node
                    .child_node(SyntaxKind::Expression)
                    .map_or(Self::Invalid, |n| Self::from_expression_node(n.into(), ctx))
                {
                    Expression::StringLiteral(p) => p,
                    _ => {
                        ctx.diag.push_error(
                            "img! Must be followed by a valid path".into(),
                            node.span(),
                        );
                        return Self::Invalid;
                    }
                };

                let absolute_source_path = {
                    let path = std::path::Path::new(&s);

                    if path.is_absolute() {
                        s
                    } else {
                        let path = ctx.diag.path(node.span()).parent().unwrap().join(path);
                        if path.is_absolute() {
                            path.to_string_lossy().to_string()
                        } else {
                            std::env::current_dir()
                                .unwrap()
                                .join(path)
                                .to_string_lossy()
                                .to_string()
                        }
                    }
                };

                Expression::ResourceReference { absolute_source_path }
            }
            Some(x) => {
                ctx.diag.push_error(format!("Unknown bang keyword `{}`", x), node.span());
                return Self::Invalid;
            }
        }
    }

    /// Perform the lookup
    fn from_qualified_name_node(node: SyntaxNode, ctx: &mut LookupCtx) -> Self {
        debug_assert_eq!(node.kind(), SyntaxKind::QualifiedName);

        let mut it = node.children_with_tokens().filter(|n| n.kind() == SyntaxKind::Identifier);

        let first = if let Some(first) = it.next() {
            first.into_token().unwrap()
        } else {
            // There must be at least one member (parser should ensure that)
            debug_assert!(ctx.diag.has_error());
            return Self::Invalid;
        };

        let first_str = first.text().as_str();

        let property = ctx.component.root_element.borrow().lookup_property(first_str);
        if property.is_property_type() {
            if let Some(x) = it.next() {
                ctx.diag.push_error(
                    "Cannot access fields of property".into(),
                    x.into_token().unwrap().span(),
                )
            }
            return Self::PropertyReference(NamedReference {
                element: Rc::downgrade(&ctx.component.root_element),
                name: first_str.to_string(),
            });
        } else if matches!(property, Type::Signal) {
            if let Some(x) = it.next() {
                ctx.diag.push_error(
                    "Cannot access fields of signal".into(),
                    x.into_token().unwrap().span(),
                )
            }
            return Self::SignalReference(NamedReference {
                element: Rc::downgrade(&ctx.component.root_element),
                name: first_str.to_string(),
            });
        } else if property.is_object_type() {
            todo!("Continue lookling up");
        }

        if let Some(elem) = find_element_by_id(ctx.component_scope, first_str) {
            let prop_name = if let Some(second) = it.next() {
                second.into_token().unwrap()
            } else {
                ctx.diag.push_error("Cannot take reference of an element".into(), node.span());
                return Self::Invalid;
            };

            let p = elem.borrow().lookup_property(prop_name.text().as_str());
            if p.is_property_type() {
                if let Some(x) = it.next() {
                    ctx.diag.push_error(
                        "Cannot access fields of property".into(),
                        x.into_token().unwrap().span(),
                    );
                    return Self::Invalid;
                }
                return Self::PropertyReference(NamedReference {
                    element: Rc::downgrade(&elem),
                    name: prop_name.text().to_string(),
                });
            } else if matches!(p, Type::Signal) {
                if let Some(x) = it.next() {
                    ctx.diag.push_error(
                        "Cannot access fields of signal".into(),
                        x.into_token().unwrap().span(),
                    )
                }
                return Self::SignalReference(NamedReference {
                    element: Rc::downgrade(&elem),
                    name: prop_name.to_string(),
                });
            } else {
                ctx.diag.push_error(
                    format!("Cannot access property '{}'", prop_name),
                    prop_name.span(),
                );
                return Self::Invalid;
            }
        }

        // Try to lookup an index or model property
        for scope in ctx.component_scope.iter().rev() {
            if let Some(repeated) = &scope.borrow().repeated {
                if first_str == repeated.index_id {
                    return Expression::RepeaterIndexReference { element: Rc::downgrade(scope) };
                } else if first_str == repeated.model_data_id {
                    return Expression::RepeaterModelReference { element: Rc::downgrade(scope) };
                }
            }
        }

        if it.next().is_some() {
            ctx.diag.push_error(format!("Cannot access id '{}'", first_str), node.span());
            return Expression::Invalid;
        }

        if matches!(ctx.property_type, Type::Color) {
            let value: Option<u32> = match first_str {
                "blue" => Some(0xff0000ff),
                "red" => Some(0xffff0000),
                "green" => Some(0xff00ff00),
                "yellow" => Some(0xffffff00),
                "black" => Some(0xff000000),
                "white" => Some(0xffffffff),
                _ => None,
            };
            if let Some(value) = value {
                return Expression::Cast {
                    from: Box::new(Expression::NumberLiteral(value as f64)),
                    to: Type::Color,
                };
            }
        }

        ctx.diag.push_error(format!("Unknown unqualified identifier '{}'", first_str), node.span());

        Self::Invalid
    }

    fn from_self_assignement_node(
        node: syntax_nodes::SelfAssignment,
        ctx: &mut LookupCtx,
    ) -> Expression {
        let (lhs_n, rhs_n) = node.Expression();
        let lhs = Self::from_expression_node(lhs_n.into(), ctx);
        if !matches!(lhs, Expression::PropertyReference{..}) {
            ctx.diag
                .push_error("Self assignement need to be done on a property".into(), node.span());
        }
        let rhs = Self::from_expression_node(rhs_n.clone().into(), ctx).maybe_convert_to(
            lhs.ty(),
            &rhs_n.into(),
            &mut ctx.diag,
        );
        Expression::SelfAssignment {
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            op: None
                .or(node.child_token(SyntaxKind::PlusEqual).and(Some('+')))
                .or(node.child_token(SyntaxKind::MinusEqual).and(Some('-')))
                .or(node.child_token(SyntaxKind::StarEqual).and(Some('*')))
                .or(node.child_token(SyntaxKind::DivEqual).and(Some('/')))
                .unwrap_or('_'),
        }
    }

    fn from_binary_expression_node(
        node: syntax_nodes::BinaryExpression,
        ctx: &mut LookupCtx,
    ) -> Expression {
        let (lhs_n, rhs_n) = node.Expression();
        let mut lhs = Self::from_expression_node(lhs_n.clone().into(), ctx);
        let mut rhs = Self::from_expression_node(rhs_n.clone().into(), ctx);

        let (lhs_ty, rhs_ty) = (lhs.ty(), rhs.ty());
        if lhs_ty != rhs_ty {
            if rhs_ty.can_convert(&lhs_ty) {
                rhs = rhs.maybe_convert_to(lhs_ty, &rhs_n.into(), &mut ctx.diag);
            } else {
                lhs = lhs.maybe_convert_to(rhs_ty, &lhs_n.into(), &mut ctx.diag);
            }
        }
        Expression::BinaryExpression {
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            op: None
                .or(node.child_token(SyntaxKind::Plus).and(Some('+')))
                .or(node.child_token(SyntaxKind::Minus).and(Some('-')))
                .or(node.child_token(SyntaxKind::Star).and(Some('*')))
                .or(node.child_token(SyntaxKind::Div).and(Some('/')))
                .unwrap_or('_'),
        }
    }

    fn from_conditional_expression_node(
        node: syntax_nodes::ConditionalExpression,
        ctx: &mut LookupCtx,
    ) -> Expression {
        let (condition_n, true_expr_n, false_expr_n) = node.Expression();
        // FIXME: we should we add bool to the context
        let condition = Self::from_expression_node(condition_n.clone().into(), ctx)
            .maybe_convert_to(Type::Bool, &condition_n.into(), &mut ctx.diag);
        let mut true_expr = Self::from_expression_node(true_expr_n.clone().into(), ctx);
        let mut false_expr = Self::from_expression_node(false_expr_n.clone().into(), ctx);
        let (true_ty, false_ty) = (true_expr.ty(), false_expr.ty());
        if true_ty != false_ty {
            if false_ty.can_convert(&true_ty) {
                false_expr =
                    false_expr.maybe_convert_to(true_ty, &false_expr_n.into(), &mut ctx.diag);
            } else {
                true_expr =
                    true_expr.maybe_convert_to(false_ty, &true_expr_n.into(), &mut ctx.diag);
            }
        }
        Expression::Condition {
            condition: Box::new(condition),
            true_expr: Box::new(true_expr),
            false_expr: Box::new(false_expr),
        }
    }

    fn from_object_literal_node(
        node: syntax_nodes::ObjectLiteral,
        ctx: &mut LookupCtx,
    ) -> Expression {
        let values: HashMap<String, Expression> = node
            .ObjectMember()
            .map(|n| {
                (
                    n.child_text(SyntaxKind::Identifier).unwrap_or_default(),
                    Expression::from_expression_node(n.Expression(), ctx),
                )
            })
            .collect();
        let ty = Type::Object(values.iter().map(|(k, v)| (k.clone(), v.ty())).collect());
        Expression::Object { ty, values }
    }

    fn from_array_node(node: syntax_nodes::Array, ctx: &mut LookupCtx) -> Expression {
        let mut values: Vec<Expression> =
            node.Expression().map(|e| Expression::from_expression_node(e, ctx)).collect();

        // FIXME: what's the type of an empty array ?
        // Also, be smarter about finding a common type
        let element_ty = values.first().map_or(Type::Invalid, |e| e.ty());

        let n = node.into();
        for e in values.iter_mut() {
            *e = core::mem::replace(e, Expression::Invalid).maybe_convert_to(
                element_ty.clone(),
                &n,
                ctx.diag,
            );
        }

        Expression::Array { element_ty, values }
    }
}

fn parse_color_literal(s: &str) -> Option<u32> {
    if !s.starts_with("#") {
        return None;
    }
    if !s.is_ascii() {
        return None;
    }
    let s = &s[1..];
    let (r, g, b, a) = match s.len() {
        3 => (
            u8::from_str_radix(&s[0..=0], 16).ok()? * 0x11,
            u8::from_str_radix(&s[1..=1], 16).ok()? * 0x11,
            u8::from_str_radix(&s[2..=2], 16).ok()? * 0x11,
            255u8,
        ),
        4 => (
            u8::from_str_radix(&s[0..=0], 16).ok()? * 0x11,
            u8::from_str_radix(&s[1..=1], 16).ok()? * 0x11,
            u8::from_str_radix(&s[2..=2], 16).ok()? * 0x11,
            u8::from_str_radix(&s[3..=3], 16).ok()? * 0x11,
        ),
        6 => (
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
            255u8,
        ),
        8 => (
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
            u8::from_str_radix(&s[6..8], 16).ok()?,
        ),
        _ => return None,
    };
    Some((a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | (b as u32) << 0)
}

#[test]
fn test_parse_color_literal() {
    assert_eq!(parse_color_literal("#abc"), Some(0xffaabbcc));
    assert_eq!(parse_color_literal("#ABC"), Some(0xffaabbcc));
    assert_eq!(parse_color_literal("#AbC"), Some(0xffaabbcc));
    assert_eq!(parse_color_literal("#AbCd"), Some(0xddaabbcc));
    assert_eq!(parse_color_literal("#01234567"), Some(0x67012345));
    assert_eq!(parse_color_literal("#012345"), Some(0xff012345));
    assert_eq!(parse_color_literal("_01234567"), None);
    assert_eq!(parse_color_literal("→↓←"), None);
    assert_eq!(parse_color_literal("#→↓←"), None);
    assert_eq!(parse_color_literal("#1234567890"), None);
}

fn unescape_string(string: &str) -> Option<String> {
    if !string.starts_with('"') || !string.ends_with('"') {
        return None;
    }
    let string = &string[1..(string.len() - 1)];
    // TODO: remove slashes
    return Some(string.into());
}
