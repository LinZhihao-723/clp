//! The KQL parser: the parser rules of `Kql.g4` and the expression building of `kql.cpp`, without
//! recursion (groups live on a heap stack, so hostile nesting cannot overflow the call stack).

use super::lexer::Token;
use super::lexer::TokenKind;
use super::lexer::tokenize;
use super::literal::Literal;
use super::literal::unescape_kql;
use super::timestamp::parse_timestamp_literal;
use crate::ColumnPattern;
use crate::ColumnRef;
use crate::CompareOperator;
use crate::LogicalExpr;
use crate::Namespace;
use crate::PathToken;
use crate::PatternPiece;
use crate::Predicate;
use crate::PredicateTest;
use crate::QueryError;
use crate::QueryOptions;
use crate::QueryValue;
use crate::TextPattern;

/// Parses KQL into an expression.
///
/// # Returns
///
/// The expression on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`tokenize`]'s return values on failure.
/// * Forwards [`Parser::parse`]'s return values on failure.
pub fn parse(text: &str, options: &QueryOptions) -> Result<LogicalExpr, QueryError> {
    Parser {
        text,
        tokens: tokenize(text)?,
        next: 0,
        options,
    }
    .parse()
}

/// An expression with its depth (a predicate has depth 1).
struct Built {
    expr: LogicalExpr,
    depth: u64,
}

/// What closes a group.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Closer {
    End,
    Paren,
    Brace,
}

/// A binary connective between two operands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Connective {
    And,
    Or,
}

/// A query being parsed: the whole input, a parenthesized subquery, or the body of a nested
/// query `column: {...}`.
struct Group {
    closer: Closer,

    /// The column prefix of a nested query, applied to every column inside the group.
    prefix: Option<ColumnPattern>,

    num_pending_nots: u64,
    accumulated: Option<Built>,

    /// The connective of the chain this group built in `accumulated`, which later operands of the
    /// same connective join (AND and OR are associative under K3).
    chain: Option<Connective>,

    pending_connective: Option<Connective>,
}

impl Group {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// An empty group closed by `closer`, whose columns are prefixed with `prefix`.
    const fn new(closer: Closer, prefix: Option<ColumnPattern>) -> Self {
        Self {
            closer,
            prefix,
            num_pending_nots: 0,
            accumulated: None,
            chain: None,
            pending_connective: None,
        }
    }

    /// Applies the pending NOTs to `operand` and joins it to the group with the pending
    /// connective (left-associatively: `a OR b AND c` is `(a OR b) AND c`).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`QueryError::LimitExceeded`] if the result is deeper than `max_depth`.
    /// * [`QueryError::Syntax`] if two operands meet without a connective.
    fn push_operand(
        &mut self,
        operand: Built,
        max_depth: u32,
        position: usize,
    ) -> Result<(), QueryError> {
        let num_nots = std::mem::take(&mut self.num_pending_nots);
        let mut operand = operand;
        check_depth(operand.depth.saturating_add(num_nots), max_depth)?;
        for _ in 0..num_nots {
            operand = Built {
                expr: LogicalExpr::Not(Box::new(operand.expr)),
                depth: operand.depth + 1,
            };
        }

        let Some(mut accumulated) = self.accumulated.take() else {
            self.accumulated = Some(operand);
            return Ok(());
        };
        let Some(connective) = self.pending_connective.take() else {
            return Err(QueryError::Syntax {
                position,
                message: "expected AND or OR".into(),
            });
        };
        let depth = accumulated.depth.max(operand.depth + 1);
        check_depth(depth, max_depth)?;
        if Some(connective) == self.chain {
            if let LogicalExpr::And(children) | LogicalExpr::Or(children) = &mut accumulated.expr {
                children.push(operand.expr);
            }
            accumulated.depth = depth;
        } else {
            let children = vec![accumulated.expr, operand.expr];
            accumulated = Built {
                expr: match connective {
                    Connective::And => LogicalExpr::And(children),
                    Connective::Or => LogicalExpr::Or(children),
                },
                depth: accumulated.depth.max(operand.depth) + 1,
            };
            check_depth(accumulated.depth, max_depth)?;
            self.chain = Some(connective);
        }
        self.accumulated = Some(accumulated);
        Ok(())
    }
}

/// The parser state: the query text, its tokens and the position of the next token.
struct Parser<'query> {
    text: &'query str,
    tokens: Vec<Token>,
    next: usize,
    options: &'query QueryOptions,
}

impl<'query> Parser<'query> {
    /// Runs the state machine: alternately expect an operand (after the start, `(`, `{`, NOT, AND
    /// or OR) and a connective or closer (after an operand).
    ///
    /// # Returns
    ///
    /// The expression of the whole query on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`QueryError::Syntax`] if a token is unexpected.
    /// * Forwards [`Group::push_operand`]'s return values on failure.
    /// * Forwards [`Self::column`]'s return values on failure.
    /// * Forwards [`Self::expression`]'s return values on failure.
    fn parse(mut self) -> Result<LogicalExpr, QueryError> {
        let max_depth = self.options.limits.max_depth;
        let mut root = Group::new(Closer::End, None);
        let mut nested: Vec<Group> = Vec::new();
        loop {
            let token = self.advance();
            let group = nested.last_mut().unwrap_or(&mut root);
            let mut operand = match token.kind {
                TokenKind::Not => {
                    group.num_pending_nots = group.num_pending_nots.saturating_add(1);
                    continue;
                }
                TokenKind::LeftParen => {
                    let prefix = group.prefix.clone();
                    nested.push(Group::new(Closer::Paren, prefix));
                    continue;
                }
                TokenKind::Quoted | TokenKind::Unquoted => {
                    if TokenKind::Colon == self.peek(0) && TokenKind::LeftBrace == self.peek(1) {
                        let column = self.column(token, group.prefix.as_ref())?;
                        self.next += 2;
                        nested.push(Group::new(Closer::Brace, Some(column)));
                        continue;
                    }
                    self.expression(token, group.prefix.as_ref())?
                }
                _ => return Err(self.unexpected(token)),
            };

            loop {
                let group = nested.last_mut().unwrap_or(&mut root);
                let token = self.advance();
                group.push_operand(operand, max_depth, token.start)?;
                match (token.kind, group.closer) {
                    (TokenKind::And, _) => group.pending_connective = Some(Connective::And),
                    (TokenKind::Or, _) => group.pending_connective = Some(Connective::Or),
                    (TokenKind::RightParen, Closer::Paren)
                    | (TokenKind::RightBrace, Closer::Brace) => {
                        let Some(closed) = nested.pop().and_then(|group| group.accumulated) else {
                            return Err(self.unexpected(token));
                        };
                        operand = closed;
                        continue;
                    }
                    (TokenKind::End, Closer::End) => {
                        return root
                            .accumulated
                            .map(|built| built.expr)
                            .ok_or_else(|| self.unexpected(token));
                    }
                    _ => return Err(self.unexpected(token)),
                }
                break;
            }
        }
    }

    /// Parses the expression that starts with the literal `first`: a column-value expression
    /// (`col: value`, `col: (list)`, `col: timestamp(...)`), a range expression
    /// (`col < value`), or a bare value.
    ///
    /// # Returns
    ///
    /// The expression on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`QueryError::Syntax`] if the tokens after `first` do not form an expression.
    /// * Forwards [`Self::column`]'s return values on failure.
    /// * Forwards [`Self::literal`]'s return values on failure.
    /// * Forwards [`Self::timestamp`]'s return values on failure.
    /// * Forwards [`Self::list`]'s return values on failure.
    /// * Forwards [`ColumnPattern::prepend`]'s return values on failure.
    fn expression(
        &mut self,
        first: Token,
        prefix: Option<&ColumnPattern>,
    ) -> Result<Built, QueryError> {
        let operator = match self.peek(0) {
            TokenKind::Colon => CompareOperator::Equal,
            TokenKind::Range(operator) => operator,
            _ => {
                let any_depth = ColumnPattern {
                    namespace: Namespace::Default,
                    tokens: Box::new([PathToken::AnyDepth]),
                };
                let column = match prefix {
                    Some(prefix) => any_depth.prepend(prefix)?,
                    None => any_depth,
                };
                let literal = self.literal(first)?;
                return Ok(self.predicate(column, CompareOperator::Equal, literal));
            }
        };
        let column = self.column(first, prefix)?;
        self.next += 1;
        let value_token = self.advance();
        match value_token.kind {
            TokenKind::Quoted | TokenKind::Unquoted => {
                let literal = self.literal(value_token)?;
                Ok(self.predicate(column, operator, literal))
            }
            TokenKind::TimestampOpen => {
                let epoch_ns = self.timestamp()?;
                Ok(self.predicate(
                    column,
                    operator,
                    Literal::Value(QueryValue::Timestamp { epoch_ns }),
                ))
            }
            TokenKind::LeftParen if CompareOperator::Equal == operator => self.list(&column),
            _ => Err(self.unexpected(value_token)),
        }
    }

    /// Parses the rest of a list of values `(` [`AND` | `OR` | `NOT`] literal* `)` on `column`:
    /// an OR of equalities by default or with `OR`, an AND with `AND`, and an AND of negated
    /// equalities with `NOT`.
    ///
    /// # Returns
    ///
    /// The expression on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`QueryError::Syntax`] if a token in the list is not a literal.
    /// * Forwards [`Self::literal`]'s return values on failure.
    fn list(&mut self, column: &ColumnPattern) -> Result<Built, QueryError> {
        let condition = match self.peek(0) {
            kind @ (TokenKind::And | TokenKind::Or | TokenKind::Not) => {
                self.next += 1;
                Some(kind)
            }
            _ => None,
        };
        let is_negated = Some(TokenKind::Not) == condition;
        let mut children = Vec::new();
        loop {
            let token = self.advance();
            match token.kind {
                TokenKind::Quoted | TokenKind::Unquoted => {
                    let literal = self.literal(token)?;
                    let predicate = self.predicate(column.clone(), CompareOperator::Equal, literal);
                    children.push(if is_negated {
                        LogicalExpr::Not(Box::new(predicate.expr))
                    } else {
                        predicate.expr
                    });
                }
                TokenKind::RightParen => break,
                _ => return Err(self.unexpected(token)),
            }
        }
        let child_depth = if is_negated { 2 } else { 1 };
        let depth = if children.is_empty() {
            1
        } else {
            child_depth + 1
        };
        check_depth(depth, self.options.limits.max_depth)?;
        let expr = match condition {
            Some(TokenKind::And | TokenKind::Not) => LogicalExpr::And(children),
            _ => LogicalExpr::Or(children),
        };
        Ok(Built { expr, depth })
    }

    /// Parses the rest of `timestamp("...")` or `timestamp("...", "pattern")`.
    ///
    /// # Returns
    ///
    /// The timestamp in epoch nanoseconds on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`QueryError::Syntax`] if the arguments are not one or two quoted strings.
    /// * Forwards [`parse_timestamp_literal`]'s return values on failure.
    fn timestamp(&mut self) -> Result<i64, QueryError> {
        let timestamp = self.advance();
        if TokenKind::Quoted != timestamp.kind {
            return Err(self.unexpected(timestamp));
        }
        let mut token = self.advance();
        let mut pattern = None;
        if TokenKind::Comma == token.kind {
            let pattern_token = self.advance();
            if TokenKind::Quoted != pattern_token.kind {
                return Err(self.unexpected(pattern_token));
            }
            pattern = Some(self.span(pattern_token));
            token = self.advance();
        }
        if TokenKind::RightParen != token.kind {
            return Err(self.unexpected(token));
        }
        parse_timestamp_literal(self.span(timestamp), pattern)
    }

    /// # Returns
    ///
    /// The column a literal token denotes, prefixed with the enclosing nested queries' column, on
    /// success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ColumnPattern::tokenize`]'s return values on failure.
    /// * Forwards [`ColumnPattern::prepend`]'s return values on failure.
    fn column(
        &self,
        token: Token,
        prefix: Option<&ColumnPattern>,
    ) -> Result<ColumnPattern, QueryError> {
        let (raw, base_position) = self.literal_content(token);
        let column = ColumnPattern::tokenize(raw.as_bytes(), base_position)?;
        match prefix {
            Some(prefix) => column.prepend(prefix),
            None => Ok(column),
        }
    }

    /// # Returns
    ///
    /// The typed value of a literal token on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`QueryError::InvalidEscape`] if the literal contains an invalid escape sequence.
    fn literal(&self, token: Token) -> Result<Literal, QueryError> {
        let (raw, base_position) = self.literal_content(token);
        let escaped =
            unescape_kql(raw.as_bytes(), true).map_err(|offset| QueryError::InvalidEscape {
                position: base_position + offset,
            })?;
        Ok(Literal::from_escaped(&escaped))
    }

    /// # Returns
    ///
    /// The predicate `column operator literal`; the literal `*` under `:` is
    /// [`PredicateTest::Exists`].
    fn predicate(
        &self,
        column: ColumnPattern,
        operator: CompareOperator,
        literal: Literal,
    ) -> Built {
        let test = match (literal, operator) {
            (Literal::AnyValue, CompareOperator::Equal) => PredicateTest::Exists,
            (Literal::AnyValue, operator) => PredicateTest::Compare {
                operator,
                value: QueryValue::Text(TextPattern::new(vec![PatternPiece::AnyRun])),
                is_case_sensitive: self.options.is_case_sensitive,
            },
            (Literal::Value(value), operator) => PredicateTest::Compare {
                operator,
                value,
                is_case_sensitive: self.options.is_case_sensitive,
            },
        };
        Built {
            expr: LogicalExpr::Predicate(Predicate {
                column: ColumnRef::from_pattern(column),
                test,
            }),
            depth: 1,
        }
    }

    /// # Returns
    ///
    /// The text of a literal token without its quotes, and the position of that text.
    fn literal_content(&self, token: Token) -> (&'query str, usize) {
        let span = self.span(token);
        if TokenKind::Quoted == token.kind {
            let content = span
                .get(1..span.len().saturating_sub(1))
                .unwrap_or_default();
            (content, token.start + 1)
        } else {
            (span, token.start)
        }
    }

    /// # Returns
    ///
    /// The query text of `token`.
    fn span(&self, token: Token) -> &'query str {
        self.text.get(token.start..token.end).unwrap_or_default()
    }

    /// # Returns
    ///
    /// The kind of the token `offset` tokens after the next one, or [`TokenKind::End`] past the
    /// last token.
    fn peek(&self, offset: usize) -> TokenKind {
        self.tokens
            .get(self.next + offset)
            .map_or(TokenKind::End, |token| token.kind)
    }

    /// Consumes the next token.
    ///
    /// # Returns
    ///
    /// The consumed token, or an [`TokenKind::End`] token at the end of the text once every token
    /// is consumed.
    fn advance(&mut self) -> Token {
        let token = self.tokens.get(self.next).copied().unwrap_or(Token {
            kind: TokenKind::End,
            start: self.text.len(),
            end: self.text.len(),
        });
        self.next = (self.next + 1).min(self.tokens.len());
        token
    }

    /// # Returns
    ///
    /// The [`QueryError::Syntax`] error reporting that `token` is unexpected.
    fn unexpected(&self, token: Token) -> QueryError {
        let message = if TokenKind::End == token.kind {
            "unexpected end of query".into()
        } else {
            format!("unexpected `{}`", self.span(token)).into()
        };
        QueryError::Syntax {
            position: token.start,
            message,
        }
    }
}

/// # Errors
///
/// Returns an error if:
///
/// * [`QueryError::LimitExceeded`] if `depth` exceeds `max_depth`.
fn check_depth(depth: u64, max_depth: u32) -> Result<(), QueryError> {
    if depth > u64::from(max_depth) {
        return Err(QueryError::LimitExceeded {
            what: "nesting levels",
            actual: depth,
            limit: u64::from(max_depth),
        });
    }
    Ok(())
}
